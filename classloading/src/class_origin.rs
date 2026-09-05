// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Class provenance — where a loaded [`Class`](crate::Class) came from.
//!
//! Every `Class` in the store carries a [`ClassOrigin`]. It answers a question
//! the VM previously could only answer as a single bool
//! (`Class::is_synthetic_stub`): *did real class bytes back this type, and if
//! not, what produced it?*
//!
//! That distinction is the whole of `--jdk-only` (see
//! `docs/feature-designs/jdk-only-mode.md` §1). Strict mode does **not** forbid
//! VM-created classes in general — array classes, hidden classes, lambdas,
//! dynamic proxies and reflection accessors are all legitimate products of a
//! conforming JVM and each carries its own origin here. What strict mode
//! forbids is exactly one of them: [`ClassOrigin::CompatibilityStub`], a
//! fabricated stand-in for a class whose real bytes were never found.
//!
//! The policy token itself ([`CompatibilityMode`]) lives in `cratonvm_types`
//! because `native-api` needs the same token for `NativeKind` and the two
//! crates may not see each other. Each crate keeps its own `allowed_in`
//! predicate next to its own enum; this module holds `classloading`'s half.

use std::sync::Arc;

use cratonvm_types::compat::CompatibilityMode;

use crate::class::{ClassId, ClassLoaderId};

/// Provenance of a loaded class.
///
/// Ordering of the variants is deliberate: real-bytes origins first, then the
/// legitimate VM-created ones, then the two that have no class file behind them
/// at all ([`ClassOrigin::VmInternal`] and [`ClassOrigin::CompatibilityStub`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassOrigin {
    /// Real bytes from the boot runtime image (jimage / jmod / boot classpath).
    BootImage {
        /// JPMS module the class was attributed to, when known.
        module: Option<Arc<str>>,
        /// Where the bytes came from — a jimage/jar URL, or the boot
        /// classpath entry.
        source: Arc<str>,
    },
    /// Real bytes from the application (`-classpath`) or extension finder.
    ApplicationClassPath { source: Arc<str> },
    /// Real bytes handed to `ClassLoader.defineClass` by a user-defined loader.
    UserDefined {
        loader: ClassLoaderId,
        source: Option<Arc<str>>,
    },
    /// A `[`-prefixed array class synthesised by the bootstrap loader from its
    /// component type (JVMS §5.3.3). Never reads a class file, and is **not** a
    /// compatibility stub in either mode.
    VmArray,
    /// A JEP 371 hidden class (`Lookup.defineHiddenClass`). Real bytes, no name
    /// in the loader's namespace.
    HiddenClass { host: Option<ClassId> },
    /// A lambda / method-reference implementation class spun by
    /// `LambdaMetafactory` (or by this VM's invokedynamic path).
    GeneratedLambda { host: Option<ClassId> },
    /// A `java.lang.reflect.Proxy` `$ProxyN` generated for a specific set of
    /// interfaces.
    GeneratedProxy { interfaces: Arc<[ClassId]> },
    /// A reflection or serialization accessor
    /// (`GeneratedMethodAccessorN`, `GeneratedConstructorAccessorN`,
    /// `GeneratedSerializationConstructorAccessorN`).
    ReflectionAccessor { host: Option<ClassId> },
    /// A VM-internal helper type with no Java-visible class file and no
    /// pretence of standing in for one — e.g. the primitive pseudo-classes and
    /// `cratonvm/synthetic/*` allocation shapes. Legitimate in both modes.
    VmInternal,
    /// A fabricated stand-in for a class whose real bytes were **not** found:
    /// the enterprise-prefix fallback, the native-backed JDK stubs, and the
    /// `ensure_synthetic_class` compatibility path.
    ///
    /// This is the single origin `--jdk-only` rejects.
    CompatibilityStub {
        /// Why the stub was minted, for the violation report. Short and
        /// operator-facing, e.g. `"enterprise-prefix fallback: no class file on
        /// any classpath entry"`.
        reason: Arc<str>,
    },
}

impl ClassOrigin {
    /// Stable lowercase tag for the census / JSON report.
    ///
    /// This is a wire format: it is the `"origin"` field of every
    /// `--dump-class-origins` row. Do not re-spell it.
    pub fn as_str(&self) -> &'static str {
        match self {
            ClassOrigin::BootImage { .. } => "boot-image",
            ClassOrigin::ApplicationClassPath { .. } => "application-class-path",
            ClassOrigin::UserDefined { .. } => "user-defined",
            ClassOrigin::VmArray => "vm-array",
            ClassOrigin::HiddenClass { .. } => "hidden-class",
            ClassOrigin::GeneratedLambda { .. } => "generated-lambda",
            ClassOrigin::GeneratedProxy { .. } => "generated-proxy",
            ClassOrigin::ReflectionAccessor { .. } => "reflection-accessor",
            ClassOrigin::VmInternal => "vm-internal",
            ClassOrigin::CompatibilityStub { .. } => "compatibility-stub",
        }
    }

    /// Whether a class with this origin may exist under `mode`.
    ///
    /// Only [`ClassOrigin::CompatibilityStub`] is rejected under
    /// [`CompatibilityMode::JdkOnly`]. Arrays, hidden classes, lambdas, proxies
    /// and reflection accessors are all products a conforming JVM creates
    /// without any class file, and remain allowed (contract §1 item 6).
    pub fn allowed_in(&self, mode: CompatibilityMode) -> bool {
        match mode {
            CompatibilityMode::Compatible => true,
            CompatibilityMode::JdkOnly => !self.is_compatibility_stub(),
        }
    }

    /// Whether this is the one forbidden origin.
    ///
    /// `Class::is_synthetic_stub` is the derived mirror of this predicate; see
    /// [`Class::set_origin`](crate::Class::set_origin).
    pub fn is_compatibility_stub(&self) -> bool {
        matches!(self, ClassOrigin::CompatibilityStub { .. })
    }

    /// The `CompatibilityStub` reason, if this is one.
    pub fn reason(&self) -> Option<&str> {
        match self {
            ClassOrigin::CompatibilityStub { reason } => Some(reason),
            _ => None,
        }
    }

    /// Whether real class bytes backed this class.
    ///
    /// True for the three class-file origins plus hidden classes (which are
    /// defined from real, if in-memory, bytes). False for the VM-created and
    /// fabricated origins. Feeds `ClassOriginEntry::real_bytes_found`.
    pub fn has_real_bytes(&self) -> bool {
        matches!(
            self,
            ClassOrigin::BootImage { .. }
                | ClassOrigin::ApplicationClassPath { .. }
                | ClassOrigin::UserDefined { .. }
                | ClassOrigin::HiddenClass { .. }
        )
    }

    /// Convenience constructor so call sites do not repeat the `Arc::from`.
    pub fn compatibility_stub(reason: impl AsRef<str>) -> Self {
        ClassOrigin::CompatibilityStub {
            reason: Arc::<str>::from(reason.as_ref()),
        }
    }
}

impl Default for ClassOrigin {
    /// `VmInternal`, not `CompatibilityStub`.
    ///
    /// A `Class` built by a test fixture or a construction site that has not
    /// yet been taught about provenance must not be *counted* as a
    /// compatibility stub — that would make the census (and the CI zero-stub
    /// gate) fire on classes nobody fabricated.
    fn default() -> Self {
        ClassOrigin::VmInternal
    }
}

/// One row of the `--dump-class-origins` census.
///
/// Plain owned strings: the census is written out as JSON by `vm-cli`, which
/// must not hold a borrow of the class store while it renders.
#[derive(Debug, Clone)]
pub struct ClassOriginEntry {
    /// Internal (slash-form) class name.
    pub name: String,
    /// [`ClassOrigin::as_str`].
    pub origin: String,
    /// The [`ClassOrigin::CompatibilityStub`] reason, when the origin is one.
    pub reason: Option<String>,
    /// Who asked for the class, when the load path recorded it
    /// (`"owner/Class.method(Desc)"`).
    pub requested_by: Option<String>,
    /// Whether real class bytes backed this class.
    pub real_bytes_found: bool,
    /// Defining loader, as the flat `ClassLoaderId::to_native_id` wire value.
    pub loader_id: u32,
    /// Direct supertypes — superclass first, then declared interfaces, as
    /// internal names.
    ///
    /// # Why the census carries this
    ///
    /// A native registered on an **abstract** method intercepts every
    /// implementor, including user-defined ones, and that blast radius is the
    /// open question on `register_interface_natives` and on the 308
    /// inherited-abstract rows in
    /// `jdk-only-census-one-class-one-platform-FIXED-20260810.md`.
    /// Answering it needs the loaded class graph: the native census names the
    /// declaring class, and only this column says which loaded classes sit
    /// under it. `scripts/jdk-only-interception.py` does that join.
    ///
    /// Direct supertypes only — the transitive closure is the reader's job,
    /// and computing it here would make the row depend on load order.
    /// A name that no longer resolves in the store is skipped rather than
    /// rendered as a placeholder: a census must not invent a class name.
    pub supertypes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_vm_internal_not_a_stub() {
        assert_eq!(ClassOrigin::default(), ClassOrigin::VmInternal);
        assert!(!ClassOrigin::default().is_compatibility_stub());
    }

    /// The whole point of the enum: strict mode rejects exactly one variant.
    #[test]
    fn only_compatibility_stub_is_rejected_under_jdk_only() {
        let every = [
            ClassOrigin::BootImage {
                module: Some(Arc::from("java.base")),
                source: Arc::from("jrt:/java.base"),
            },
            ClassOrigin::ApplicationClassPath {
                source: Arc::from("file:/app.jar"),
            },
            ClassOrigin::UserDefined {
                loader: ClassLoaderId::UserDefined(7),
                source: None,
            },
            ClassOrigin::VmArray,
            ClassOrigin::HiddenClass { host: None },
            ClassOrigin::GeneratedLambda { host: None },
            ClassOrigin::GeneratedProxy {
                interfaces: Arc::from(Vec::<ClassId>::new()),
            },
            ClassOrigin::ReflectionAccessor { host: None },
            ClassOrigin::VmInternal,
        ];
        for origin in &every {
            assert!(
                origin.allowed_in(CompatibilityMode::JdkOnly),
                "{origin:?} must stay legal under --jdk-only"
            );
            assert!(origin.allowed_in(CompatibilityMode::Compatible));
        }
        let stub = ClassOrigin::compatibility_stub("test");
        assert!(stub.allowed_in(CompatibilityMode::Compatible));
        assert!(!stub.allowed_in(CompatibilityMode::JdkOnly));
        assert!(stub.is_compatibility_stub());
        assert_eq!(stub.reason(), Some("test"));
    }

    /// Array classes are the variant most at risk of being lumped in with
    /// stubs — they are VM-created and have no class file. Both modes allow
    /// them (contract §5, "Array classes get `VmArray`, never
    /// `CompatibilityStub`, in **both** modes").
    #[test]
    fn arrays_are_allowed_in_both_modes() {
        assert!(ClassOrigin::VmArray.allowed_in(CompatibilityMode::JdkOnly));
        assert!(ClassOrigin::VmArray.allowed_in(CompatibilityMode::Compatible));
        assert!(!ClassOrigin::VmArray.is_compatibility_stub());
    }

    #[test]
    fn tags_are_distinct_and_stable() {
        let tags = [
            ClassOrigin::BootImage {
                module: None,
                source: Arc::from(""),
            }
            .as_str(),
            ClassOrigin::ApplicationClassPath {
                source: Arc::from(""),
            }
            .as_str(),
            ClassOrigin::UserDefined {
                loader: ClassLoaderId::Bootstrap,
                source: None,
            }
            .as_str(),
            ClassOrigin::VmArray.as_str(),
            ClassOrigin::HiddenClass { host: None }.as_str(),
            ClassOrigin::GeneratedLambda { host: None }.as_str(),
            ClassOrigin::GeneratedProxy {
                interfaces: Arc::from(Vec::<ClassId>::new()),
            }
            .as_str(),
            ClassOrigin::ReflectionAccessor { host: None }.as_str(),
            ClassOrigin::VmInternal.as_str(),
            ClassOrigin::compatibility_stub("r").as_str(),
        ];
        let mut sorted = tags.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), tags.len(), "origin tags must be injective");
        assert_eq!(ClassOrigin::VmArray.as_str(), "vm-array");
        assert_eq!(
            ClassOrigin::compatibility_stub("r").as_str(),
            "compatibility-stub"
        );
    }

    #[test]
    fn real_bytes_predicate() {
        assert!(ClassOrigin::BootImage {
            module: None,
            source: Arc::from("jrt:/java.base"),
        }
        .has_real_bytes());
        assert!(ClassOrigin::HiddenClass { host: None }.has_real_bytes());
        assert!(!ClassOrigin::VmArray.has_real_bytes());
        assert!(!ClassOrigin::VmInternal.has_real_bytes());
        assert!(!ClassOrigin::compatibility_stub("r").has_real_bytes());
    }
}
