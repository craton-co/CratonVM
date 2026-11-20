use super::*;

#[test]
fn java_version_names() {
    assert_eq!(ClassFileVersion::JAVA_8.java_version_name(), "8");
    assert_eq!(ClassFileVersion::JAVA_11.java_version_name(), "11");
    assert_eq!(ClassFileVersion::JAVA_21.java_version_name(), "21");
    assert_eq!(ClassFileVersion::JAVA_25.java_version_name(), "25");
}

#[test]
fn version_ordering() {
    assert!(ClassFileVersion::JAVA_7 < ClassFileVersion::JAVA_8);
    assert!(ClassFileVersion::JAVA_21 < ClassFileVersion::JAVA_25);
    assert!(ClassFileVersion::JAVA_25 <= ClassFileVersion::MAX_SUPPORTED);
}

#[test]
fn supported_versions() {
    assert!(ClassFileVersion::JAVA_7.is_supported());
    assert!(ClassFileVersion::JAVA_21.is_supported());
    assert!(ClassFileVersion::JAVA_22.is_supported());
    assert!(ClassFileVersion::JAVA_25.is_supported());
    assert!(ClassFileVersion::JAVA_1.is_supported());
    assert!(ClassFileVersion::new(45, u16::MAX).is_supported());
    assert!(ClassFileVersion::new(
        ClassFileVersion::MAX_SUPPORTED.major,
        ClassFileVersion::PREVIEW_MINOR
    )
    .is_supported());
    assert!(!ClassFileVersion::new(70, 0).is_supported());
    assert!(!ClassFileVersion::new(68, ClassFileVersion::PREVIEW_MINOR).is_supported());
    assert!(!ClassFileVersion::new(68, 1).is_supported());
    assert!(!ClassFileVersion::new(ClassFileVersion::MAX_SUPPORTED.major, 1).is_supported());
    // Versions below major 45 predate the class file format.
    assert!(!ClassFileVersion::new(44, 0).is_supported());
    assert!(!ClassFileVersion::new(0, 0).is_supported());
}

/// Every row here is a `java` invocation on Adoptium 25.0.3.9 against a
/// hand-edited class file, not a reading of the spec. The transcript is in
/// docs/feature-designs/class-resolution-and-caching.md.
///
/// The global switch is deliberately not used: `verify` takes the bit as an
/// argument precisely so the truth table can be asserted without a
/// process-global that a parallel test would race.
#[test]
fn verify_matches_hotspot() {
    let max = ClassFileVersion::MAX_SUPPORTED.major;
    let prv = ClassFileVersion::PREVIEW_MINOR;
    let v = ClassFileVersion::new;

    // The one pair whose verdict depends on the switch. `A69p` at 69.65535:
    // refused with no flag, runs with `--enable-preview`.
    assert_eq!(
        v(max, prv).verify(false),
        Err(VersionRejection::PreviewNotEnabled)
    );
    assert_eq!(v(max, prv).verify(true), Ok(()));

    // Rule 2, and note it does NOT depend on the switch: `A68p` and `B56p`
    // both reported the same mismatch with and without `--enable-preview`.
    for major in [56, 68] {
        assert_eq!(
            v(major, prv).verify(false),
            Err(VersionRejection::PreviewMajorMismatch)
        );
        assert_eq!(
            v(major, prv).verify(true),
            Err(VersionRejection::PreviewMajorMismatch)
        );
    }

    // Below major 56 the minor version is unconstrained, so 0xFFFF is junk
    // rather than a preview marker. `A45p` (45.65535) and `B55p` (55.65535)
    // both printed their `ran-` line on plain `java`. A preview check keyed
    // on `minor == 65535` alone would refuse these — this is the over-deny
    // the whole shape of `verify` exists to avoid.
    for major in [45, 52, 55] {
        assert_eq!(v(major, prv).verify(false), Ok(()));
        assert_eq!(v(major, 1).verify(false), Ok(()));
        assert!(!v(major, prv).is_preview());
    }

    // Non-zero, non-preview minor from major 56 up: `B56x` (56.1) and
    // `F69` (69.1), both refused, both with and without the flag.
    assert_eq!(v(56, 1).verify(true), Err(VersionRejection::NonZeroMinor));
    assert_eq!(v(max, 1).verify(true), Err(VersionRejection::NonZeroMinor));

    // The range ends. `D70` at 70.65535 reported too-new, not preview —
    // the major bound outranks every preview rule.
    assert_eq!(
        v(max + 1, prv).verify(true),
        Err(VersionRejection::MajorTooNew)
    );
    assert_eq!(v(44, 0).verify(true), Err(VersionRejection::MajorTooOld));

    // The JDK's own class files, and every class file in every existing
    // workload: minor 0, unaffected by the switch in either position.
    for major in 45..=max {
        assert_eq!(v(major, 0).verify(false), Ok(()));
        assert_eq!(v(major, 0).verify(true), Ok(()));
    }
}

/// HotSpot's `UnsupportedClassVersionError` messages, verbatim. Copied from
/// the run, not composed — including the missing full stop after
/// `'--enable-preview'` and the slash-separated class name.
#[test]
fn rejection_messages_are_hotspots() {
    let max = ClassFileVersion::MAX_SUPPORTED.major;
    let prv = ClassFileVersion::PREVIEW_MINOR;

    assert_eq!(
        VersionRejection::PreviewNotEnabled.message(ClassFileVersion::new(max, prv), "pk/C"),
        "Preview features are not enabled for pk/C (class file version 69.65535). \
         Try running with '--enable-preview'"
    );
    assert_eq!(
        VersionRejection::PreviewMajorMismatch.message(ClassFileVersion::new(68, prv), "pk/D"),
        "pk/D (class file version 68.65535) was compiled with preview features that are \
         unsupported. This version of the Java Runtime only recognizes preview features \
         for class file version 69.65535"
    );
    assert_eq!(
        VersionRejection::NonZeroMinor.message(ClassFileVersion::new(56, 1), "B56x"),
        "B56x (class file version 56.1) was compiled with an invalid non-zero minor version"
    );
    assert_eq!(
        VersionRejection::MajorTooNew.message(ClassFileVersion::new(70, prv), "D70"),
        "D70 has been compiled by a more recent version of the Java Runtime (class file \
         version 70.65535), this version of the Java Runtime only recognizes class file \
         versions up to 69.0"
    );
    assert_eq!(
        VersionRejection::MajorTooOld.message(ClassFileVersion::new(44, 0), "E44"),
        "E44 (class file version 44.0) was compiled with an invalid major version"
    );
}

/// A VM with preview off refuses a `69.65535` class file, HotSpot's
/// default; one with preview on accepts it. The bit is per parse since
/// interpreter round i5 wave 101 (lane L5): there is no process global
/// left for a test to read or flip.
#[test]
fn preview_is_decided_per_verify() {
    let preview = ClassFileVersion::new(
        ClassFileVersion::MAX_SUPPORTED.major,
        ClassFileVersion::PREVIEW_MINOR,
    );
    assert_eq!(
        preview.verify(false),
        Err(VersionRejection::PreviewNotEnabled)
    );
    assert_eq!(preview.verify(true), Ok(()));
}
