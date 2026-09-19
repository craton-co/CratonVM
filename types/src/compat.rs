// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The JDK-only compatibility policy token.
//!
//! `--jdk-only` means **real class bytes are authoritative**: no fabricated
//! compatibility class, no `SyntheticStub` native registered or invoked, and a
//! structured error instead of a silent substitution. That is a *runtime
//! policy*, not a build feature, so the token that expresses it has to be
//! visible to every crate that can make a substitution — the native registry,
//! the class manager, the interpreter and the launcher.
//!
//! Those crates do not share a type otherwise: `NativeKind` lives in
//! `native-api`, `ClassOrigin` lives in `classloading`, and neither may
//! reference the other. `types` is the only crate all of them already depend
//! on, so the policy token lives here and each crate keeps its own
//! `allowed_in(mode)` predicate next to its own enum.
//!
//! There is deliberately **no process global** here. The mode is carried by
//! [`ExecutionPolicy`] values held per-`VmConfig`, per-registry and
//! per-class-manager, and is set once at VM init. A process global would make
//! two VMs in one process share one policy, which this repository has already
//! been bitten by for native caches.

/// Which compatibility substitutions the VM permits. Orthogonal to `JdkMode`
/// (which selects *which class library*); this selects *which substitutions*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompatibilityMode {
    /// Existing real-JDK behaviour: bridges, intrinsics AND compatibility shims.
    #[default]
    Compatible,
    /// Real JDK bytes are authoritative. No fabricated compatibility classes,
    /// no `SyntheticStub` native registered or invoked.
    JdkOnly,
}

impl CompatibilityMode {
    /// Stable machine-greppable spelling: `"compatible"` / `"jdk-only"`.
    ///
    /// This string is a wire format: it is the `"mode"` field of the
    /// `--jdk-only-report` JSON and appears in the census dumps. Do not
    /// re-spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            CompatibilityMode::Compatible => "compatible",
            CompatibilityMode::JdkOnly => "jdk-only",
        }
    }

    /// Whether strict JDK-only policy is in force.
    pub fn is_jdk_only(self) -> bool {
        matches!(self, CompatibilityMode::JdkOnly)
    }
}

/// The policy object shared by init, class loading and dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionPolicy {
    pub compatibility_mode: CompatibilityMode,
    /// `true` when booting a real JDK image (`JdkMode::Real`). `JdkMode` itself
    /// lives in `vm`, which `types` cannot see, hence a bool.
    pub real_jdk: bool,
}

impl ExecutionPolicy {
    /// Today's behaviour: every substitution permitted.
    pub fn compatible(real_jdk: bool) -> Self {
        ExecutionPolicy {
            compatibility_mode: CompatibilityMode::Compatible,
            real_jdk,
        }
    }

    /// Strict mode. `--jdk-only` implies a real JDK image, so `real_jdk` is
    /// `true` by construction — there is no strict synthetic mode.
    pub fn jdk_only() -> Self {
        ExecutionPolicy {
            compatibility_mode: CompatibilityMode::JdkOnly,
            real_jdk: true,
        }
    }

    pub fn is_jdk_only(&self) -> bool {
        self.compatibility_mode.is_jdk_only()
    }
}

impl Default for ExecutionPolicy {
    fn default() -> Self {
        ExecutionPolicy::compatible(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_is_the_documented_wire_spelling() {
        assert_eq!(CompatibilityMode::Compatible.as_str(), "compatible");
        assert_eq!(CompatibilityMode::JdkOnly.as_str(), "jdk-only");
    }

    /// The report JSON round-trips through `as_str`, so the mapping has to be
    /// injective — two modes sharing a spelling would make the report unable to
    /// say which run produced it.
    #[test]
    fn as_str_round_trips() {
        for mode in [CompatibilityMode::Compatible, CompatibilityMode::JdkOnly] {
            let parsed = match mode.as_str() {
                "compatible" => CompatibilityMode::Compatible,
                "jdk-only" => CompatibilityMode::JdkOnly,
                other => panic!("unmapped spelling {other}"),
            };
            assert_eq!(parsed, mode);
        }
        assert_ne!(
            CompatibilityMode::Compatible.as_str(),
            CompatibilityMode::JdkOnly.as_str()
        );
    }

    /// Strictness must never be the default: it is opted into by `--jdk-only`
    /// and never inferred from a build feature or an unrelated env var.
    #[test]
    fn default_is_compatible() {
        assert_eq!(CompatibilityMode::default(), CompatibilityMode::Compatible);
        assert!(!CompatibilityMode::default().is_jdk_only());
    }

    #[test]
    fn is_jdk_only_matches_the_mode() {
        assert!(CompatibilityMode::JdkOnly.is_jdk_only());
        assert!(!CompatibilityMode::Compatible.is_jdk_only());
    }

    #[test]
    fn execution_policy_constructors() {
        let compat = ExecutionPolicy::compatible(true);
        assert_eq!(compat.compatibility_mode, CompatibilityMode::Compatible);
        assert!(compat.real_jdk);
        assert!(!compat.is_jdk_only());

        let synthetic = ExecutionPolicy::compatible(false);
        assert!(!synthetic.real_jdk);
        assert!(!synthetic.is_jdk_only());

        let strict = ExecutionPolicy::jdk_only();
        assert_eq!(strict.compatibility_mode, CompatibilityMode::JdkOnly);
        assert!(
            strict.real_jdk,
            "--jdk-only always implies a real JDK image"
        );
        assert!(strict.is_jdk_only());
    }

    /// The default policy is the *current* VM: real JDK, every substitution
    /// permitted. An embedded `VmConfig::default()` must not silently become
    /// strict.
    #[test]
    fn execution_policy_default_is_compatible_real_jdk() {
        assert_eq!(
            ExecutionPolicy::default(),
            ExecutionPolicy::compatible(true)
        );
        assert!(!ExecutionPolicy::default().is_jdk_only());
        assert!(ExecutionPolicy::default().real_jdk);
    }
}
