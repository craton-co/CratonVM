// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether preview features are enabled for this VM run.
///
/// HotSpot's default is off and it is a whole-process property fixed before the
/// first class is parsed (`Arguments::enable_preview()`), so a process global is
/// the shape of the thing rather than a shortcut. The reader is a leaf crate
/// with no access to argument parsing, so the launcher pushes the bit down here
/// once; see [`set_preview_enabled`].
static PREVIEW_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable or disable preview class files for the rest of the process.
///
/// Called once by the launcher after argument parsing, from the `--enable-preview`
/// JVM argument. It is deliberately a plain function and not an environment
/// variable: `--enable-preview` is a JVM argument on HotSpot, and adding a
/// `CRATONVM_*` twin would grow the declared flag surface for a switch that
/// already has a spec-mandated spelling.
///
/// Off by default, which is HotSpot's default and also what
/// `jdk/internal/misc/PreviewFeatures.isPreviewEnabled` already answers in this
/// tree — the two must be set from the same bit or a class file the VM refused
/// to load would still be told preview is on.
pub fn set_preview_enabled(enabled: bool) {
    PREVIEW_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Whether preview class files may be loaded in this run.
pub fn preview_enabled() -> bool {
    PREVIEW_ENABLED.load(Ordering::Relaxed)
}

/// Why a `major.minor` pair is not loadable, one variant per distinct
/// `UnsupportedClassVersionError` message HotSpot produces.
///
/// The variants are ordered as HotSpot evaluates them
/// (`ClassFileParser::verify_class_version`), and the order is load-bearing:
/// a `68.65535` class file reports [`Self::PreviewMajorMismatch`] whether or not
/// preview is enabled, because the major-version check runs first. Measured on
/// Adoptium 25.0.3.9 — see docs/known-issues/jdk-only/W7-28-preview-classfile-gating.md
/// for the transcript of all five.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionRejection {
    /// `major < 45`: predates the class file format.
    MajorTooOld,
    /// `major > MAX_SUPPORTED.major`: compiled by a newer runtime.
    MajorTooNew,
    /// `minor == 65535` but `major` is not this runtime's major version. A
    /// preview class file from an older release is refused even *with* preview
    /// enabled — a preview feature's encoding is only guaranteed within the
    /// release that shipped it.
    PreviewMajorMismatch,
    /// `minor == 65535`, `major` matches, but preview is off for this run.
    PreviewNotEnabled,
    /// `major >= 56` with a minor that is neither 0 nor the preview marker.
    /// JVMS 4.1 constrains `minor_version` only from major 56 onwards, which is
    /// why `55.65535` loads on HotSpot and is not a preview class file.
    NonZeroMinor,
}

impl VersionRejection {
    /// HotSpot's `java.lang.UnsupportedClassVersionError` message, verbatim.
    ///
    /// `internal_name` is the slash-separated internal form: HotSpot prints
    /// `pk/C`, not `pk.C`, because the message is built from the parser's
    /// `Symbol*` rather than from an external name. Measured — a probe that
    /// loaded a `69.65535` `pk.C` through `Class.forName` reported
    /// `Preview features are not enabled for pk/C (class file version 69.65535)`.
    ///
    /// The two version literals embedded in the text are derived from
    /// [`ClassFileVersion::MAX_SUPPORTED`] rather than written out, so bumping
    /// the supported release moves them together. Note the asymmetry, which is
    /// HotSpot's and not a typo here: the too-new message names `69.0` while the
    /// preview-mismatch message names `69.65535`.
    pub fn message(&self, version: ClassFileVersion, internal_name: &str) -> String {
        let max = ClassFileVersion::MAX_SUPPORTED;
        match self {
            Self::MajorTooOld => format!(
                "{internal_name} (class file version {}.{}) was compiled with an invalid major version",
                version.major, version.minor
            ),
            Self::MajorTooNew => format!(
                "{internal_name} has been compiled by a more recent version of the Java Runtime \
                 (class file version {}.{}), this version of the Java Runtime only recognizes \
                 class file versions up to {}.{}",
                version.major, version.minor, max.major, max.minor
            ),
            Self::PreviewMajorMismatch => format!(
                "{internal_name} (class file version {}.{}) was compiled with preview features \
                 that are unsupported. This version of the Java Runtime only recognizes preview \
                 features for class file version {}.{}",
                version.major,
                version.minor,
                max.major,
                ClassFileVersion::PREVIEW_MINOR
            ),
            Self::PreviewNotEnabled => format!(
                "Preview features are not enabled for {internal_name} (class file version {}.{}). \
                 Try running with '--enable-preview'",
                version.major, version.minor
            ),
            Self::NonZeroMinor => format!(
                "{internal_name} (class file version {}.{}) was compiled with an invalid non-zero \
                 minor version",
                version.major, version.minor
            ),
        }
    }
}

impl fmt::Display for VersionRejection {
    /// The reason clause only. The reader does not know the class name when the
    /// version is checked — `this_class` is not read until after the constant
    /// pool — so the name-bearing form lives in [`Self::message`] and is built
    /// by whoever already holds the name. This clause is appended to the
    /// existing `unsupported class file version {major}.{minor}` text so the
    /// old message stays a prefix of the new one.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let max = ClassFileVersion::MAX_SUPPORTED;
        match self {
            Self::MajorTooOld => write!(f, "invalid major version"),
            Self::MajorTooNew => write!(
                f,
                "this Java Runtime only recognizes class file versions up to {}.{}",
                max.major, max.minor
            ),
            Self::PreviewMajorMismatch => write!(
                f,
                "compiled with preview features that are unsupported; this Java Runtime only \
                 recognizes preview features for class file version {}.{}",
                max.major,
                ClassFileVersion::PREVIEW_MINOR
            ),
            Self::PreviewNotEnabled => write!(
                f,
                "preview features are not enabled; try running with '--enable-preview'"
            ),
            Self::NonZeroMinor => write!(f, "invalid non-zero minor version"),
        }
    }
}

/// Represents the version of a Java class file.
///
/// The class file version determines which features are available. Each Java release
/// increments the major version number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClassFileVersion {
    pub major: u16,
    pub minor: u16,
}

impl ClassFileVersion {
    pub const JAVA_1: Self = Self {
        major: 45,
        minor: 3,
    };
    pub const JAVA_5: Self = Self {
        major: 49,
        minor: 0,
    };
    pub const JAVA_6: Self = Self {
        major: 50,
        minor: 0,
    };
    pub const JAVA_7: Self = Self {
        major: 51,
        minor: 0,
    };
    pub const JAVA_8: Self = Self {
        major: 52,
        minor: 0,
    };
    pub const JAVA_9: Self = Self {
        major: 53,
        minor: 0,
    };
    pub const JAVA_10: Self = Self {
        major: 54,
        minor: 0,
    };
    pub const JAVA_11: Self = Self {
        major: 55,
        minor: 0,
    };
    pub const JAVA_12: Self = Self {
        major: 56,
        minor: 0,
    };
    pub const JAVA_13: Self = Self {
        major: 57,
        minor: 0,
    };
    pub const JAVA_14: Self = Self {
        major: 58,
        minor: 0,
    };
    pub const JAVA_15: Self = Self {
        major: 59,
        minor: 0,
    };
    pub const JAVA_16: Self = Self {
        major: 60,
        minor: 0,
    };
    pub const JAVA_17: Self = Self {
        major: 61,
        minor: 0,
    };
    pub const JAVA_18: Self = Self {
        major: 62,
        minor: 0,
    };
    pub const JAVA_19: Self = Self {
        major: 63,
        minor: 0,
    };
    pub const JAVA_20: Self = Self {
        major: 64,
        minor: 0,
    };
    pub const JAVA_21: Self = Self {
        major: 65,
        minor: 0,
    };
    pub const JAVA_22: Self = Self {
        major: 66,
        minor: 0,
    };
    pub const JAVA_23: Self = Self {
        major: 67,
        minor: 0,
    };
    pub const JAVA_24: Self = Self {
        major: 68,
        minor: 0,
    };
    pub const JAVA_25: Self = Self {
        major: 69,
        minor: 0,
    };

    /// The maximum class file version this JVM supports.
    pub const MAX_SUPPORTED: Self = Self::JAVA_25;
    /// Minor version used by class files that depend on preview features.
    pub const PREVIEW_MINOR: u16 = 0xFFFF;

    pub fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    /// Returns the Java SE release name for this version.
    pub fn java_version_name(&self) -> String {
        match self.major {
            45 => "1.1".to_string(),
            46 => "1.2".to_string(),
            47 => "1.3".to_string(),
            48 => "1.4".to_string(),
            49 => "5".to_string(),
            50 => "6".to_string(),
            51 => "7".to_string(),
            52 => "8".to_string(),
            v if v >= 53 => format!("{}", v - 44),
            _ => format!("unknown({})", self.major),
        }
    }

    /// Returns `true` if this class file version is within the range the
    /// JVM supports: major version 45 through 69 (Java 25) inclusive, with
    /// minor-version rules from JVMS 4.1.
    ///
    /// **Shape only — this deliberately does not gate preview.** It answers
    /// `verify(preview_enabled = true)`, i.e. exactly what it answered before
    /// preview gating existed, so every caller that only asks "is this pair a
    /// version this parser understands" keeps its old answer. The load decision
    /// is [`Self::verify`]; nothing but a test should be calling this.
    pub fn is_supported(&self) -> bool {
        self.verify(true).is_ok()
    }

    /// Decide whether a class file with this version may be loaded.
    ///
    /// The check order is HotSpot's, and each arm was measured against Adoptium
    /// 25.0.3.9 rather than read off the spec — the transcript is in
    /// docs/known-issues/jdk-only/W7-28-preview-classfile-gating.md. Two of the
    /// arms are counter-intuitive enough to be worth stating here:
    ///
    /// * **`major <= 55` accepts any minor at all**, including 65535. JVMS 4.1
    ///   constrains `minor_version` only from major 56 onwards, so `55.65535`
    ///   is a class file with a junk minor and *not* a preview class file.
    ///   HotSpot runs it with no flag; measured, both directions. A preview
    ///   check keyed on `minor == 65535` alone would refuse it, and an
    ///   over-deny in a parser every loaded class passes through is worse than
    ///   the under-deny it replaces.
    /// * **the major-must-match rule outranks the enablement rule.** A
    ///   `68.65535` class file reports [`VersionRejection::PreviewMajorMismatch`]
    ///   with *and* without `--enable-preview` — preview bytecode from an older
    ///   release is never loadable, because a preview feature's encoding is only
    ///   stable within the release that shipped it.
    pub fn verify(&self, preview_enabled: bool) -> Result<(), VersionRejection> {
        if self.major < 45 {
            return Err(VersionRejection::MajorTooOld);
        }
        if self.major > Self::MAX_SUPPORTED.major {
            return Err(VersionRejection::MajorTooNew);
        }
        if self.major <= 55 {
            return Ok(());
        }
        if self.minor == 0 {
            return Ok(());
        }
        if self.minor == Self::PREVIEW_MINOR {
            if self.major != Self::MAX_SUPPORTED.major {
                return Err(VersionRejection::PreviewMajorMismatch);
            }
            if !preview_enabled {
                return Err(VersionRejection::PreviewNotEnabled);
            }
            return Ok(());
        }
        Err(VersionRejection::NonZeroMinor)
    }

    /// Whether this is a preview class file in the JVMS 4.1 sense.
    ///
    /// Not simply `minor == PREVIEW_MINOR`: below major 56 the minor version
    /// carries no meaning, so `55.65535` is not preview-flagged even though the
    /// bytes match.
    pub fn is_preview(&self) -> bool {
        self.major >= 56 && self.minor == Self::PREVIEW_MINOR
    }
}

impl fmt::Display for ClassFileVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{} (Java {})",
            self.major,
            self.minor,
            self.java_version_name()
        )
    }
}

#[cfg(test)]
mod tests {
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
    /// docs/known-issues/jdk-only/W7-28-preview-classfile-gating.md.
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

    /// The switch defaults off, which is HotSpot's default and the whole reason
    /// a run with no `--enable-preview` refuses a `69.65535` class file.
    ///
    /// This reads the process global directly, which is only sound because no
    /// test in this crate calls `set_preview_enabled` — `verify` takes the bit
    /// as an argument precisely so none needs to. If one ever does, this test
    /// becomes order-dependent and must move to its own integration test binary.
    #[test]
    fn preview_defaults_off() {
        assert!(!preview_enabled());
    }
}
