use super::AssertionDirectives;

fn parse(args: &[&str]) -> AssertionDirectives {
    let mut d = AssertionDirectives::default();
    for a in args {
        assert!(d.apply_option(a), "{a} is an assertion option");
    }
    d
}

// (user, inner, string, list) as L5W106AssertionScopes prints them.
fn row(d: &AssertionDirectives) -> [bool; 4] {
    [
        d.enabled("L5W106AssertionScopes", false, false),
        d.enabled("L5W106AssertionScopes$Inner", false, false),
        d.enabled("java/lang/String", true, false),
        d.enabled("java/util/ArrayList", true, false),
    ]
}

#[test]
fn hotspot_rows_of_the_probe() {
    assert_eq!(row(&parse(&[])), [false, false, false, false]);
    assert_eq!(row(&parse(&["-ea"])), [true, true, false, false]);
    assert_eq!(row(&parse(&["-esa"])), [false, false, true, true]);
    assert_eq!(
        row(&parse(&["-ea", "-da:L5W106AssertionScopes"])),
        [false, true, false, false]
    );
    assert_eq!(
        row(&parse(&["-ea", "-da:L5W106AssertionScopes$Inner"])),
        [true, false, false, false]
    );
    assert_eq!(row(&parse(&["-ea:..."])), [true, true, false, false]);
    assert_eq!(
        row(&parse(&["-ea:L5W106AssertionScopes"])),
        [true, false, false, false]
    );
    assert_eq!(row(&parse(&["-ea:java.util..."])), [false, false, false, true]);
    assert_eq!(row(&parse(&["-da", "-esa"])), [false, false, true, true]);
}

#[test]
fn last_option_wins_and_packages_nest() {
    let d = parse(&["-ea:a.b...", "-da:a.b.c...", "-ea:a.b.c.D", "-da:a.b.c.D"]);
    assert!(d.enabled("a/b/X", false, false));
    assert!(!d.enabled("a/b/c/X", false, false));
    assert!(!d.enabled("a/b/c/D", false, false));
    assert!(!d.enabled("a/b/c/d/E", false, false), "a.b.c... covers a.b.c.d");
    assert!(!d.enabled("a/bc/X", false, false), "a.b... is not a prefix match");
    // The unnamed-package option does not reach a packaged class.
    let d = parse(&["-ea:..."]);
    assert!(!d.enabled("a/X", false, false));
    assert!(d.enabled("X", false, false));
}

#[test]
fn non_options_are_refused() {
    let mut d = AssertionDirectives::default();
    for a in ["-esa:x", "-eax", "-e", "-enableassertionsX", "-Xmx1g"] {
        assert!(!d.apply_option(a), "{a}");
    }
    assert!(d.is_empty());
    assert!(d.apply_option("-enableassertions:p..."));
    assert_eq!(d.dotted_packages(), vec![("p".to_string(), true)]);
}

#[test]
fn fallback_is_the_user_default_only() {
    let d = AssertionDirectives::default();
    assert!(d.enabled("p/A", false, true));
    assert!(!d.enabled("java/lang/String", true, true));
    let d = parse(&["-da"]);
    assert!(!d.enabled("p/A", false, true));
}
