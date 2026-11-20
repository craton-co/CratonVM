use super::*;

#[test]
fn j27_wide_pure_takes_the_widest_factor_the_budget_holds() {
    // A 5-node promoted accumulation under the 160-node wide budget: 16 copies.
    assert_eq!(wide_factor(4, 5, true, 160), 16);
    // 40 nodes: 160 / 40 = 4 copies, the base.
    assert_eq!(wide_factor(4, 40, true, 160), 4);
    // 20 nodes: 8 copies (160 nodes).
    assert_eq!(wide_factor(4, 20, true, 160), 8);
}

#[test]
fn j27_wide_pure_never_narrows_and_leaves_a_deopting_body_alone() {
    assert_eq!(wide_factor(4, 5, false, 160), 4, "a body that can deopt keeps the base");
    assert_eq!(wide_factor(4, 40, true, 160), 4, "never below the base");
    assert_eq!(wide_factor(4, 0, true, 160), 4, "an empty body is refused elsewhere");    }