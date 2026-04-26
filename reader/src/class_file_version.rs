use std::fmt;

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

    pub fn is_supported(&self) -> bool {
        *self <= Self::MAX_SUPPORTED
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
        assert!(!ClassFileVersion::new(70, 0).is_supported());
    }
}
