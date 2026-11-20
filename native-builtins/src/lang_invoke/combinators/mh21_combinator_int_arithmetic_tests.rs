use super::{filter_arity_exceeded, filter_index, fold_arguments_verdict, FoldVerdict};

fn types(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn mh21_filter_arity_is_the_jdks_wrapping_int_sum() {
    // (int, int) target: two filters at 0 fit, three do not, one at 2 does not.
    assert!(!filter_arity_exceeded(0, 2, 2));
    assert!(filter_arity_exceeded(0, 3, 2));
    assert!(filter_arity_exceeded(1, 2, 2));
    assert!(!filter_arity_exceeded(2, 0, 2));
    // pos == Integer.MAX_VALUE with one filter wraps to MIN_VALUE: the JDK's check
    // passes and `parameterType(MAX_VALUE)` raises afterwards.
    assert!(!filter_arity_exceeded(i32::MAX, 1, 2));
    // A negative position is not "too many" either.
    assert!(!filter_arity_exceeded(-5, 1, 2));
}

#[test]
fn mh21_filter_index_wraps_like_pos_plus_i() {
    assert_eq!(filter_index(3, 2), 5);
    assert_eq!(filter_index(-1, 0), -1);
    assert_eq!(filter_index(i32::MAX, 1), i32::MIN);
}

#[test]
fn mh21_fold_verdict_matches_fold_argument_checks() {
    let target = types(&["I", "J", "Ljava/lang/String;"]);
    // A value combiner (I)I folded at 0 needs target[0] == return and takes
    // target[1..] as its parameters: target[1] is J, so (I) does not fit.
    assert_eq!(
        fold_arguments_verdict(&target, &types(&["I"]), "I", 0),
        FoldVerdict::Mismatch
    );
    // (J)I folded at 0: target[1] == J, return I == target[0]: fits.
    assert_eq!(
        fold_arguments_verdict(&target, &types(&["J"]), "I", 0),
        FoldVerdict::Accept
    );
    // A void combiner (I) at 0 takes target[0..].
    assert_eq!(
        fold_arguments_verdict(&target, &types(&["I"]), "V", 0),
        FoldVerdict::Accept
    );
    // Too many combiner parameters: a plain mismatch, no index is read.
    assert_eq!(
        fold_arguments_verdict(
            &target,
            &types(&["I", "J", "Ljava/lang/String;", "I"]),
            "V",
            0
        ),
        FoldVerdict::Mismatch
    );
    // A value combiner whose return differs from target[pos].
    assert_eq!(
        fold_arguments_verdict(&target, &types(&["J"]), "J", 0),
        FoldVerdict::Mismatch
    );
}

#[test]
fn mh21_fold_negative_positions_raise_the_first_bad_index() {
    let target = types(&["I", "J"]);
    // pos -1, value combiner (I): after == 0, parameter 0 is target[0] == I (fits), then
    // the result check reads parameterType(-1).
    assert_eq!(
        fold_arguments_verdict(&target, &types(&["I"]), "I", -1),
        FoldVerdict::Aioobe(-1)
    );
    // pos -2, void combiner (I): after == -2, parameterType(-2) raises first.
    assert_eq!(
        fold_arguments_verdict(&target, &types(&["I"]), "V", -2),
        FoldVerdict::Aioobe(-2)
    );
    // pos -3, void combiner with no parameters: nothing is read, it fits.
    assert_eq!(
        fold_arguments_verdict(&target, &[], "V", -3),
        FoldVerdict::Accept
    );
}

#[test]
fn mh21_fold_int_overflow_raises_where_64_bit_sums_answered_a_mismatch() {
    let target = types(&["I", "J"]);
    // pos MAX, value combiner (J): after wraps to MIN, MIN + 1 <= 2 so the count
    // check passes, and parameterType(0 + MIN) raises.
    assert_eq!(
        fold_arguments_verdict(&target, &types(&["J"]), "I", i32::MAX),
        FoldVerdict::Aioobe(i32::MIN)
    );
    // pos MAX, value combiner with no parameters: the count check passes and the
    // result check reads parameterType(MAX).
    assert_eq!(
        fold_arguments_verdict(&target, &[], "I", i32::MAX),
        FoldVerdict::Aioobe(i32::MAX)
    );
    // pos MAX - 1, value combiner (J): after == MAX, MAX + 1 wraps negative, the
    // count check passes and parameterType(MAX) raises.
    assert_eq!(
        fold_arguments_verdict(&target, &types(&["J"]), "I", i32::MAX - 1),
        FoldVerdict::Aioobe(i32::MAX)
    );
    // pos MAX, void combiner with no parameters: after == MAX > 2, plain mismatch.
    assert_eq!(
        fold_arguments_verdict(&target, &[], "V", i32::MAX),
        FoldVerdict::Mismatch
    );
    // pos == the parameter count, a void combiner without parameters: fits.
    assert_eq!(
        fold_arguments_verdict(&target, &[], "V", 2),
        FoldVerdict::Accept
    );
}
