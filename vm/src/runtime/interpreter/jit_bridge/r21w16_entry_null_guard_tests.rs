use super::entry_null_guard_field;

/// javac's `CompletableFuture.postComplete()` entry: `f = this; (h =
/// f.stack) != null || (f != this && ...)`, the loop body past pc 16.
fn post_complete_shape() -> Vec<u8> {
    let mut c = vec![
        0x2a, // 0: aload_0
        0x4c, // 1: astore_1
        0x2b, // 2: aload_1
        0xb4, 0x00, 0x05, // 3: getfield #5 (stack)
        0x59, // 6: dup
        0x4d, // 7: astore_2
        0xc7, 0x00, 0x0a, // 8: ifnonnull 18
        0x2b, // 11: aload_1
        0x2a, // 12: aload_0
        0xa5, 0x00, 0x0b, // 13: if_acmpeq 24
    ];
    // 16..24: the loop body (a call, never reached on the null path).
    c.extend_from_slice(&[0x2a, 0xb6, 0x00, 0x07, 0x57, 0xa7, 0xff, 0xec]);
    c.push(0xb1); // 24: return
    c
}

#[test]
fn r21w16_cf_post_complete_s_entry_is_a_null_guard_on_its_field() {
    assert_eq!(entry_null_guard_field(&post_complete_shape(), 3), Some(5));
    // `if (f == null) return; work();`
    let early = [0x2a, 0xb4, 0x00, 0x09, 0xc6, 0x00, 0x07, 0x2a, 0xb6, 0x00, 0x02, 0xb1];
    assert_eq!(entry_null_guard_field(&early, 1), Some(9));
}

#[test]
fn r21w16_cf_any_effect_or_unknown_on_the_null_path_refuses() {
    // The null path calls: `if (f != null) return; work();`.
    let calls = [0x2a, 0xb4, 0x00, 0x09, 0xc7, 0x00, 0x07, 0x2a, 0xb6, 0x00, 0x02, 0xb1];
    assert_eq!(entry_null_guard_field(&calls, 1), None);
    // Two different fields.
    let two = [
        0x2a, 0xb4, 0x00, 0x05, 0xc7, 0x00, 0x0b, 0x2a, 0xb4, 0x00, 0x06, 0xc7, 0x00, 0x04,
        0xb1, 0xb1,
    ];
    assert_eq!(entry_null_guard_field(&two, 1), None);
    // No field read at all: nothing to guard.
    assert_eq!(entry_null_guard_field(&[0xb1], 1), None);
    // A field of an unknown local, and a branch on an unknown value.
    assert_eq!(entry_null_guard_field(&[0x2b, 0xb4, 0x00, 0x05, 0xb1], 2), None);
    assert_eq!(
        entry_null_guard_field(&[0x2a, 0xb4, 0x00, 0x05, 0x57, 0x2b, 0xc6, 0x00, 0x04, 0xb1], 2),
        None
    );
    // A store on the null path.
    assert_eq!(
        entry_null_guard_field(&[0x2a, 0xb4, 0x00, 0x05, 0x57, 0x2a, 0x01, 0xb5, 0x00, 0x05, 0xb1], 1),
        None
    );
    // A value return is not a `void` exit; a self-loop runs out of steps.
    assert_eq!(entry_null_guard_field(&[0x2a, 0xb4, 0x00, 0x05, 0xb0], 1), None);
    assert_eq!(
        entry_null_guard_field(&[0x2a, 0xb4, 0x00, 0x05, 0x57, 0xa7, 0xff, 0xfb], 1),
        None
    );
    // A branch past the code.
    assert_eq!(entry_null_guard_field(&[0x2a, 0xb4, 0x00, 0x05, 0xc6, 0x00, 0x40], 1), None);
}
