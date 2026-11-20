use super::speculation_id_for;
use crate::deopt::DeoptReason;

/// The property a loop rewrite depends on, and the one an index-derived id
/// would have broken.
///
/// A rewrite emits the loop body more than once, so the same guard is
/// recorded twice — same bci, same reason, different emitter pc.
/// `loop_rewrite::deopt_point_difference` treats a `speculation_id`
/// mismatch between two images of one bytecode as `PointDifference::Fatal`,
/// so an id that depended on emission ORDER would refuse every loop rewrite
/// in the tree, by a field nobody was reading yet.
#[test]
fn two_copies_of_one_guard_get_the_same_speculation_id() {
    assert_eq!(
        speculation_id_for(16, DeoptReason::BoundsCheck),
        speculation_id_for(16, DeoptReason::BoundsCheck),
    );
}

/// And the case the field exists for: a protected invoke carries a
/// re-execute guard AND a rethrow frame at one bci, and they are different
/// speculations. Telling them apart is what a `DespecRegistry` keyed on
/// `(method_key, bci, speculation_id)` would need in order to suppress one
/// without suppressing the other.
#[test]
fn two_speculations_at_one_bci_get_different_ids() {
    assert_ne!(
        speculation_id_for(6, DeoptReason::ReceiverTypeChanged),
        speculation_id_for(6, DeoptReason::PendingException),
    );
    // And the same speculation at two bcis is two speculations.
    assert_ne!(
        speculation_id_for(6, DeoptReason::BoundsCheck),
        speculation_id_for(7, DeoptReason::BoundsCheck),
    );
}

/// `0` stays the "no id assigned" value every other producer in the tree
/// still writes, so a consumer can tell a derived id from an unset one.
/// A derivation that lands on zero must be nudged rather than published.
#[test]
fn a_derived_id_is_never_the_unassigned_sentinel() {
    for bci in 0u32..512 {
        for reason in [
            DeoptReason::NullCheck,
            DeoptReason::BoundsCheck,
            DeoptReason::ReceiverTypeChanged,
            DeoptReason::PendingException,
            DeoptReason::OsrExit,
            DeoptReason::UnreachedCode,
        ] {
            assert_ne!(speculation_id_for(bci, reason), 0);
        }
    }
}
