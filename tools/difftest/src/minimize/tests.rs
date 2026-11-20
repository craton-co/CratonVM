use super::*;

/// A pure predicate: the candidate is interesting iff it still contains the
/// marker line `KEEP`. ddmin should strip everything else.
#[test]
fn ddmin_strips_to_the_interesting_line() {
    let src = "a\nb\nKEEP\nc\nd\ne\nf\n";
    let m = minimize_source(src, |s| s.lines().any(|l| l == "KEEP"));
    assert_eq!(m.source.trim(), "KEEP");
    assert_eq!(m.lines, 1);
    assert!(m.steps > 0);
}

#[test]
fn ddmin_keeps_two_required_lines() {
    // Interesting iff BOTH markers survive (and in order).
    let src = "x\nONE\ny\nz\nTWO\nw\n";
    let m = minimize_source(src, |s| {
        let ls: Vec<&str> = s.lines().collect();
        ls.contains(&"ONE") && ls.contains(&"TWO")
    });
    let kept: Vec<&str> = m.source.lines().collect();
    assert_eq!(kept, vec!["ONE", "TWO"]);
}

#[test]
fn ddmin_noop_when_nothing_removable() {
    // Every line is required ⇒ no reduction.
    let src = "ONE\nTWO\n";
    let m = minimize_source(src, |s| {
        let ls: Vec<&str> = s.lines().collect();
        ls.contains(&"ONE") && ls.contains(&"TWO")
    });
    assert_eq!(m.lines, 2);
    assert_eq!(m.steps, 0);
}

#[test]
fn ddmin_single_line_input_is_stable() {
    let m = minimize_source("only\n", |_| true);
    assert_eq!(m.source.trim(), "only");
}
