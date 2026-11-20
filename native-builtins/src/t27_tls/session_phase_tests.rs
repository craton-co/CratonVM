use super::{session_phase, SessionDoor, SessionPhase};

/// The distinction that was missing.
#[test]
fn the_null_session_is_not_the_pending_session() {
    assert_ne!(
        session_phase(SessionDoor::Current, false),
        session_phase(SessionDoor::Handshake, false),
    );
}

/// …and the pending session is what `getSession()` answers afterwards, so
/// a mid-handshake `putValue` survives — `mustCallResumeTrustedOnSession
/// Resumption`, which HUNG when it did not. Asserted as the two phases the
/// carry rule joins, since they are deliberately distinct KEYS and
/// `engine_session_for` copies the bindings across.
#[test]
fn the_negotiated_session_is_the_one_that_was_pending() {
    assert_eq!(
        session_phase(SessionDoor::Current, true),
        SessionPhase::Negotiated
    );
    assert_eq!(
        session_phase(SessionDoor::Handshake, false),
        SessionPhase::Handshaking
    );
}

/// `getSession()` while a handshake is in flight is still the NULL
/// session, not the pending one.
#[test]
fn get_session_mid_handshake_is_still_the_null_session() {
    assert_eq!(
        session_phase(SessionDoor::Current, false),
        SessionPhase::Fresh
    );
}
