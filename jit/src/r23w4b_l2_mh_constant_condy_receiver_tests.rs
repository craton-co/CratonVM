use super::*;

/// cp 9: `MethodHandle.invokeExact(I)I`.
fn shape(cp: u16) -> Option<(usize, bool)> {
    (cp == 9).then_some((1, true))
}

/// JIT r23 wave 4b, lane L2 (L2W4B-1): the receiver walk names an `ldc` /
/// `ldc_w` as the receiver's instruction only when asked to, and never a
/// constant that is not the receiver.
#[test]
fn the_receiver_walk_accepts_an_ldc_only_when_asked() {
    // ldc #5 ; iload_1 ; invokevirtual #9
    let ldc = [0x12, 0x05, 0x1b, 0xb6, 0x00, 0x09];
    let none = vec![false; ldc.len()];
    assert_eq!(
        mh_constant_receiver_push(&ldc, ldc.len(), 3, 1, &none, &shape, true),
        Some((0, 5))
    );
    assert_eq!(
        mh_constant_receiver_push(&ldc, ldc.len(), 3, 1, &none, &shape, false),
        None
    );
    assert_eq!(mh_constant_receiver_getstatic(&ldc, ldc.len(), 3, 1, &none, &shape), None);
    // ldc_w #0x0105 ; iload_1 ; invokevirtual #9
    let ldc_w = [0x13, 0x01, 0x05, 0x1b, 0xb6, 0x00, 0x09];
    let none = vec![false; ldc_w.len()];
    assert_eq!(
        mh_constant_receiver_push(&ldc_w, ldc_w.len(), 4, 1, &none, &shape, true),
        Some((0, 0x0105))
    );
    // aload_0 ; ldc #5 ; invokevirtual #9: the ldc is the ARGUMENT.
    let arg = [0x2a, 0x12, 0x05, 0xb6, 0x00, 0x09];
    let none = vec![false; arg.len()];
    assert_eq!(
        mh_constant_receiver_push(&arg, arg.len(), 3, 1, &none, &shape, true),
        None
    );
}
