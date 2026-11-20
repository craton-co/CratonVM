use super::DelegatedWrite;

/// The whole reason the answer is three-way rather than two.
///
/// This table is not observable from inside a Java program: every one of
/// the three answers returns from `println` without throwing, and the only
/// thing they change is whether the caller falls back to the raw console
/// fd — a write no code in the JVM can see, which is exactly why W7-70
/// could not measure this and why `probes/CloseFlushSwallowProbe.java`
/// asserts the Java-visible half and prints the rest.
/// W7-81-write-route-three-way.md
#[test]
fn an_absorbed_ioexception_is_routed_so_it_is_not_echoed() {
    // HotSpot's `catch (IOException x) { trouble = true; }` runs and the
    // bytes go NOWHERE. A caller that treats this as "not written" writes
    // them a second time, to a console HotSpot never touched.
    assert_eq!(
        DelegatedWrite::classify(false, true),
        DelegatedWrite::Absorbed
    );
    assert!(DelegatedWrite::Absorbed.routed());
    assert!(!DelegatedWrite::Absorbed.delivered());
}

#[test]
fn an_error_is_refused_so_the_console_fallback_survives() {
    // The picocli / JUnit-console `NoSuchMethodError` shape. HotSpot lets
    // it out of `println`; this VM cannot, so "not routed" — and the
    // console fallback it triggers — is what keeps the text from vanishing.
    assert_eq!(
        DelegatedWrite::classify(false, false),
        DelegatedWrite::Refused
    );
    assert!(!DelegatedWrite::Refused.routed());
    assert!(!DelegatedWrite::Refused.delivered());
}

#[test]
fn an_internal_error_is_refused_and_never_absorbed() {
    // `MethodCallFailed::InternalError` is not a Java throwable and can
    // never be the `IOException` a JDK `catch` names, so absorbing it is
    // never JDK parity — whatever the second argument says.
    assert_eq!(
        DelegatedWrite::classify(true, false),
        DelegatedWrite::Refused
    );
    assert_eq!(
        DelegatedWrite::classify(true, true),
        DelegatedWrite::Refused
    );
}

#[test]
fn a_clean_write_is_the_only_delivered_answer() {
    assert!(DelegatedWrite::Delivered.routed());
    assert!(DelegatedWrite::Delivered.delivered());
    // `record_write_failure`'s `bool` is `delivered()`, NOT `routed()`.
    // Conflating the two is the defect W7-81 unpicked, so pin them apart.
    assert_ne!(
        DelegatedWrite::Absorbed.routed(),
        DelegatedWrite::Absorbed.delivered()
    );
}
