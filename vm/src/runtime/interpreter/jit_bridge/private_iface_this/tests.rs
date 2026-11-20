use super::*;

/// Wave 133 (lane L4): the `invokespecial` form of the proof.
#[test]
fn special_sites_on_this_are_proven_and_on_an_argument_are_not() {
    let proven = |code: &[u8], is_static: bool, cp_idx: u16| {
        every_special_site_receiver_is_this(code, &[], is_static, "()Ljava/lang/String;", |cp| {
            cp == cp_idx
        })
    };
    // aload_0 invokespecial #7 priv()Ljava/lang/String; areturn
    let on_this = [0x2a, 0xb7, 0, 7, 0xb0];
    assert!(proven(&on_this, false, 7));
    // A static method's site is never proven.
    assert!(!proven(&on_this, true, 7));
    // Another constant-pool index: no site, nothing to prove.
    assert!(proven(&on_this, true, 8));
    // aload_1 invokespecial #7 areturn: the i132-L6 `special-bad` shape.
    let on_arg = [0x2b, 0xb7, 0, 7, 0xb0];
    assert!(!proven(&on_arg, false, 7));
    // aload_1 astore_0 first: local 0 is no longer `this`.
    let stored = [0x2b, 0x4b, 0x2a, 0xb7, 0, 7, 0xb0];
    assert!(!proven(&stored, false, 7));
}

/// `invokeinterface #cp, count, 0`.
fn invokeinterface(cp: u16, count: u8) -> [u8; 5] {
    let [hi, lo] = cp.to_be_bytes();
    [0xb9, hi, lo, count, 0]
}

#[test]
fn arg_words_counts_categories_and_arrays() {
    assert_eq!(arg_words("()I"), Some(0));
    assert_eq!(arg_words("(Ljava/lang/String;)I"), Some(1));
    assert_eq!(arg_words("(J)Ljava/lang/Object;"), Some(2));
    assert_eq!(arg_words("(Ljava/lang/foreign/MemoryLayout;J)V"), Some(3));
    assert_eq!(arg_words("([[J[Ljava/lang/Object;D)V"), Some(4));
    assert_eq!(arg_words("I"), None);
    assert_eq!(arg_words("(Ljava/lang/String"), None);
}

#[test]
fn match_result_group_shape_is_proven() {
    // aload_0 aload_0 aload_1 invokeinterface #1 groupNumber(String)I
    // invokeinterface #2 start(I)I ireturn
    let mut code = vec![0x2a, 0x2a, 0x2b];
    code.extend_from_slice(&invokeinterface(1, 2));
    code.extend_from_slice(&invokeinterface(2, 2));
    code.push(0xac);
    assert!(every_site_receiver_is_this(&code, &[], 1, None, "(Ljava/lang/String;)I"));
    // The outer call's receiver is also `this`, but it is not asked.
    assert!(!every_site_receiver_is_this(&code, &[], 9, None, "()I"));
}

#[test]
fn allocate_from_string_shape_is_proven() {
    // aload_0 iload 5 i2l iload_3 i2l ladd invokeinterface #40 (J)
    let mut code = vec![0x2a, 0x15, 5, 0x85, 0x1d, 0x85, 0x61];
    code.extend_from_slice(&invokeinterface(40, 3));
    code.push(0xb0);
    assert!(every_site_receiver_is_this(
        &code,
        &[],
        40,
        Some(7),
        "(J)Ljava/lang/foreign/MemorySegment;"
    ));
    assert!(!every_site_receiver_is_this(
        &code,
        &[],
        40,
        Some(0),
        "(J)Ljava/lang/foreign/MemorySegment;"
    ));
}

#[test]
fn another_receiver_a_stored_local_zero_or_a_join_is_not_proven() {
    let desc = "(Ljava/lang/String;)I";
    // aload_1 aload_2 invokeinterface: the receiver is local 1.
    let mut other = vec![0x2b, 0x2c];
    other.extend_from_slice(&invokeinterface(1, 2));
    other.push(0xac);
    assert!(!every_site_receiver_is_this(&other, &[], 1, None, desc));
    // aload_1 astore_0 aload_0 aload_1 invokeinterface: local 0 reassigned.
    let mut stored = vec![0x2b, 0x4b, 0x2a, 0x2b];
    stored.extend_from_slice(&invokeinterface(1, 2));
    stored.push(0xac);
    assert!(!every_site_receiver_is_this(&stored, &[], 1, None, desc));
    // aload_0 aload_1 invokeinterface, with a handler starting at the
    // `aload_1`: control may reach the invoke with another stack.
    let mut joined = vec![0x2a, 0x2b];
    joined.extend_from_slice(&invokeinterface(1, 2));
    joined.push(0xac);
    assert!(every_site_receiver_is_this(&joined, &[], 1, None, desc));
    assert!(!every_site_receiver_is_this(&joined, &[1], 1, None, desc));
    // A `dup` in the window is not stepped over.
    let mut dup = vec![0x2a, 0x2b, 0x59, 0x57];
    dup.extend_from_slice(&invokeinterface(1, 2));
    dup.push(0xac);
    assert!(!every_site_receiver_is_this(&dup, &[], 1, None, desc));
    // `jsr` anywhere refuses the method.
    let mut jsr = vec![0x2a, 0x2b];
    jsr.extend_from_slice(&invokeinterface(1, 2));
    jsr.extend_from_slice(&[0xa8, 0x00, 0x03, 0xac]);
    assert!(!every_site_receiver_is_this(&jsr, &[], 1, None, desc));
}

#[test]
fn a_branch_into_the_window_is_not_proven() {
    let desc = "(Ljava/lang/String;)I";
    // 0: aload_0  1: iload_1  2: ifeq +6 (-> 8)  5: aload_0  6: aload_2
    // 7: nop      8: ... invoke at 8 reached from the branch with other stack
    let mut code = vec![0x2a, 0x1b, 0x99, 0x00, 0x06, 0x2a, 0x2c, 0x00];
    code.extend_from_slice(&invokeinterface(1, 2));
    code.push(0xac);
    // `nop` is not in the step list either, so this is refused twice over;
    // without the nop the branch target is the invoke itself.
    assert!(!every_site_receiver_is_this(&code, &[], 1, None, desc));
    let mut direct = vec![0x2a, 0x1b, 0x99, 0x00, 0x05, 0x2a, 0x2c];
    direct.extend_from_slice(&invokeinterface(1, 2));
    direct.push(0xac);
    assert!(!every_site_receiver_is_this(&direct, &[], 1, None, desc));
}
