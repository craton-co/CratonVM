use super::{native_mirror_refusal, NativeMirrorRefusal};

/// gcg g3/o (door 2): inside a native, a refused mirror on Generational
/// and ZGC always raises; on G1 it raises only once no Free region is
/// left for the infallible arm (which could then only abort), and the
/// switch off restores the infallible G1 arm.
#[test]
fn gcg_g3o_a_refused_native_mirror_raises_where_the_infallible_arm_would_abort() {
    use NativeMirrorRefusal::{Infallible, Raise};
    // Generational / ZGC: the switch and the region count play no part.
    assert_eq!(native_mirror_refusal(false, 0, false), Raise);
    assert_eq!(native_mirror_refusal(false, 7, true), Raise);
    // G1, switch on: the reserve while a Free region is left, else raise.
    assert_eq!(native_mirror_refusal(true, 3, true), Infallible);
    assert_eq!(native_mirror_refusal(true, 0, true), Raise);
    // G1, switch off: the pre-g3 answer, whatever is left.
    assert_eq!(native_mirror_refusal(true, 0, false), Infallible);
    assert_eq!(native_mirror_refusal(true, 3, false), Infallible);
}

