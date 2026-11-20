//! JIT round 16 wave 9 (lane ffm, UP1-1): the exact door is taken only by
//! a non-model handle whose type is proven, and only with the switch on;
//! every other choice is the one round 15 made.
use super::*;

#[test]
fn the_exact_door_takes_only_a_proven_non_model_handle() {
    use InvokerChoice::*;
    // (downcall, model, exact, unproven_generic, synthetic_site, door)
    let c = invoker_choice;
    // Switch off: round 15's choices.
    assert_eq!(c(false, false, true, true, true, false), Generic);
    assert_eq!(c(false, false, false, true, true, false), Generic);
    assert_eq!(c(false, true, true, true, true, false), SyntheticAtSite);
    assert_eq!(c(false, true, false, true, true, false), Generic);
    assert_eq!(c(false, true, false, false, true, false), SyntheticAtSite);
    assert_eq!(c(true, false, false, true, false, false), Synthetic);
    // Switch on: only the proven non-model handle moves.
    assert_eq!(c(false, false, true, true, true, true), ExactDoor);
    assert_eq!(c(false, false, false, true, true, true), Generic);
    assert_eq!(c(false, true, true, true, true, true), SyntheticAtSite);
    assert_eq!(c(false, true, false, true, true, true), Generic);
    assert_eq!(c(true, false, true, true, true, true), SyntheticAtSite);
}
