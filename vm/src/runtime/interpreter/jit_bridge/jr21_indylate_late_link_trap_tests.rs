use super::*;

/// The decision: never for an instruction no linked bridge serves; a
/// linked one only when the bridge serves it now; an unlinked one as the
/// per-instruction memo answers. Each closure runs only on its own arm.
#[test]
fn the_verdict_follows_the_instructions_state() {
    let never = || -> bool { panic!("not asked on this arm") };
    assert!(!indy_late_link_reprieve_verdict(None, never, never));
    assert!(indy_late_link_reprieve_verdict(Some(true), || true, never));
    assert!(!indy_late_link_reprieve_verdict(
        Some(true),
        || false,
        never
    ));
    assert!(indy_late_link_reprieve_verdict(Some(false), never, || true));
    assert!(!indy_late_link_reprieve_verdict(Some(false), never, || {
        false
    }));
}

