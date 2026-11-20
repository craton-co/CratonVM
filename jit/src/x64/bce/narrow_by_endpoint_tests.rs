use super::*;


/// The same endpoints one step in, where the fact IS satisfiable by exactly
/// one value, must keep that value rather than collapsing.
#[test]
fn a_fact_exactly_one_int_satisfies_narrows_to_that_int() {
    let top = Range::top(IntWidth::W32);

    let gt = narrow_by(top, Rel::Gt, i32::MAX as i64 - 1);
    assert_eq!(gt.lo(), Some(i32::MAX as i64));
    assert_eq!(gt.hi(), Some(i32::MAX as i64));

    let lt = narrow_by(top, Rel::Lt, i32::MIN as i64 + 1);
    assert_eq!(lt.lo(), Some(i32::MIN as i64));
    assert_eq!(lt.hi(), Some(i32::MIN as i64));
}

/// An `i64` endpoint that would wrap narrows to bottom, not to top.
///
/// Unreachable through `range_const_push` today, and the point is that the
/// reachability is a property of a DIFFERENT function. Without the checked
/// arithmetic, `x > i64::MAX` computes `i64::MIN` as the lower endpoint and
/// `Range::exact(W32, i64::MIN, i32::MAX)` clamps that back up to
/// `i32::MIN` — top, every `int` admitted, as the consequence of a fact no
/// value satisfies. That is the one shape in this function that narrows in
/// the unsound direction, and it is one widened constant decoder away.
#[test]
fn an_i64_endpoint_that_would_wrap_narrows_to_bottom() {
    let top = Range::top(IntWidth::W32);

    assert!(
        narrow_by(top, Rel::Gt, i64::MAX).is_empty(),
        "`x > i64::MAX` must be bottom; unchecked, `c + 1` wraps to \
         i64::MIN and the narrowing hands back top"
    );
    assert!(
        narrow_by(top, Rel::Lt, i64::MIN).is_empty(),
        "`x < i64::MIN` must be bottom; unchecked, `c - 1` wraps to \
         i64::MAX and the narrowing hands back top"
    );
    // One step in, where no wrap occurs, the answer is still bottom — for
    // the ordinary reason (no `int` is above `i64::MAX - 1`), which is what
    // makes the pair above about the wrap and not about the magnitude.
    assert!(narrow_by(top, Rel::Gt, i64::MAX - 1).is_empty());
    assert!(narrow_by(top, Rel::Lt, i64::MIN + 1).is_empty());
}
