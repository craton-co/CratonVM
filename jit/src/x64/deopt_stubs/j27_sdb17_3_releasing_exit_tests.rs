use super::releasing_exit_admitted;

#[test]
fn an_offered_protected_exit_is_refused_only_under_the_hand_over() {
    // Unprotected: always the method's own propagation.
    for (site, window, pass) in [(false, true, false), (true, true, false), (false, false, true)] {
        assert!(releasing_exit_admitted(false, site, window, pass));
    }
    // Protected and never offered: refused whatever is asked.
    assert!(!releasing_exit_admitted(true, false, false, false));
    assert!(!releasing_exit_admitted(true, false, true, true));
    // Offered: admitted without the hand-over, refused with it ...
    assert!(releasing_exit_admitted(true, true, false, false));
    assert!(!releasing_exit_admitted(true, true, true, false));
    // ... unless the census asks what a hand-over twin would admit.
    assert!(releasing_exit_admitted(true, true, true, true));
}
