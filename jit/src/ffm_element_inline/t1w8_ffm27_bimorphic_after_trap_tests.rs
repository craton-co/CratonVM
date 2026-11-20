use super::{
    ffm_after_trap_despec, ffm_bimorphic_after_trap_enabled, ffm_bimorphic_after_trap_pair,
    FfmAfterTrapDespec,
};
use crate::deopt::{speculation_id, DeoptReason, SPECULATION_ID_ANY};

/// The site's own unreached trap (its `UnstableIf` id at the bci) admits
/// the bimorphic plan after a de-spec, strictly when the one-class
/// expansion's class test (`ReceiverTypeChanged`) was withdrawn too; the
/// wildcard, the bounds guards' `UncommonTrap`, the null test, another
/// bci's id, or no trap at all do not.
#[test]
fn only_the_unreached_trap_at_the_bci_admits_the_bimorphic_plan() {
    let trap = speculation_id(19, DeoptReason::UnstableIf);
    let class_miss = speculation_id(19, DeoptReason::ReceiverTypeChanged);
    let guard = speculation_id(19, DeoptReason::UncommonTrap);
    let null = speculation_id(19, DeoptReason::NullCheck);
    let other_bci = speculation_id(20, DeoptReason::UnstableIf);
    assert_eq!(ffm_after_trap_despec(&[trap], 19), Some(FfmAfterTrapDespec::TrapOnly));
    assert_eq!(
        ffm_after_trap_despec(&[class_miss, trap], 19),
        Some(FfmAfterTrapDespec::TrapAndClassMiss)
    );
    assert_eq!(ffm_after_trap_despec(&[], 19), None, "nothing withdrawn");
    assert_eq!(ffm_after_trap_despec(&[class_miss], 19), None, "no trap fired");
    assert_eq!(ffm_after_trap_despec(&[trap, guard], 19), None, "bounds guards failed");
    assert_eq!(ffm_after_trap_despec(&[trap, null], 19), None, "null receivers");
    assert_eq!(ffm_after_trap_despec(&[trap, SPECULATION_ID_ANY], 19), None, "wildcard");
    assert_eq!(ffm_after_trap_despec(&[other_bci], 19), None, "another bci's id");
}

