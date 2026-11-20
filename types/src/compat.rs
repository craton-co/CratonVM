// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The Real-JDK compatibility policy token.
//!
//! `--real-jdk` means **real class bytes are authoritative**: no fabricated
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
///
/// # `Default` is `Compatible`, and that is NOT the launcher default
///
/// `#[default]` stays on `Compatible` deliberately. This impl is what a bare
/// `CompatibilityMode::default()` and every struct that derives `Default`
/// around one picks up — including `VmConfig::default()`, the embedding and
/// in-tree-test entry point. Those callers have not asked for a policy, and
/// `JdkOnly` rejects work `Compatible` accepts, so the permissive value is the
/// only safe thing to hand them.
///
/// The `cratonvm` launcher defaults to `JdkOnly` (since 2026-09-20), but it
/// says so at its own call site: `cratonvm_vm::config::for_launcher` reads
/// `LAUNCHER_DEFAULT_COMPATIBILITY_MODE`. Moving `#[default]` here to "make
/// them agree" would flip every embedder and every one of the ~5,000 in-tree
/// tests at the same time, silently, which is the opposite of what the
/// launcher change is for.
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
    /// `--real-jdk-report` JSON and appears in the census dumps. Do not
    /// re-spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            CompatibilityMode::Compatible => "compatible",
            CompatibilityMode::JdkOnly => "jdk-only",
        }
    }

    /// Whether strict Real-JDK policy is in force.
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

    /// Strict mode. `--real-jdk` implies a real JDK image, so `real_jdk` is
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
mod tests;
