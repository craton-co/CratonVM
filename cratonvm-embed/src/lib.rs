// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! # `cratonvm-embed` — curated Rust embedding facade for CratonVM
//!
//! This is **Layer 1** of `docs/feature-designs/embedding-api.md`: a thin,
//! curated, semver-stable facade over the `cratonvm-vm` crate's embedding
//! surface. It re-exports *exactly* the supported types and adds the few
//! conveniences an embedder needs, so a host does not have to depend on the
//! whole `cratonvm-vm` crate and reach into its internals.
//!
//! For a **C / non-Rust** host, use the `libcratonvm` crate instead (the
//! `cdylib`/`staticlib` with the JNI Invocation API + the flat `cratonvm_*` C
//! ABI). This crate is for **Rust** hosts that want a stable, minimal API.
//!
//! ## Semver boundary
//!
//! The facade contract is the documented re-export list plus the helper
//! function signatures in this crate. `Vm`, `SharedVm`, and `JvmThread` are
//! concrete re-exports from `cratonvm-vm`, so their public inherent methods are
//! visible to downstream crates and form the practical Rust API while this crate
//! is on the 0.3 line. Treat methods not documented here as lower-level VM
//! pass-throughs rather than the intended long-term facade surface.
//!
//! ## Feature policy
//!
//! `cratonvm-embed` disables `cratonvm-vm` default features by default so the
//! facade dependency graph stays headless and avoids optional desktop or
//! experimental crates. Enable `vm-defaults` to mirror the VM crate's default
//! feature set (`awt` + `management`), `vm-experimental` for the optional
//! experiments on top of it, or individual forwarded features such as `awt` or
//! `management`.
//!
//! Note that disabling default features also drops `management`, which backs
//! `java.lang.management.ManagementFactory` — a host that calls it needs
//! `management` (or `vm-defaults`) turned on explicitly.
//!
//! ## Compatibility mode
//!
//! *Compatibility mode* selects **which substitutions the VM may make**;
//! [`JdkMode`] selects **which class library it boots**. The two are
//! independent, and both are set on the [`VmConfig`] before [`Vm::new`].
//!
//! [`CompatibilityMode`], [`ExecutionPolicy`] and [`JdkMode`] are re-exported
//! here for one concrete reason: this facade previously exposed only
//! [`VmConfig`], so an embedder could not *name* the type
//! [`VmConfig::with_compatibility_mode`], [`VmConfig::with_jdk_mode`] and
//! [`VmConfig::execution_policy`] deal in without adding a direct dependency on
//! `cratonvm-vm` — i.e. without giving up the whole point of the facade.
//!
//! **The default is [`CompatibilityMode::Compatible`], and you reach it by
//! doing nothing.** Strict mode is never inferred from a Cargo feature, from
//! `CRATONVM_REAL` / `CRATONVM_NO_STUBS`, or from what the host machine has
//! installed: those select a native-registry filter only and cannot express the
//! class-loading or dispatch half of the contract.
//!
//! **[`JdkMode`] is asymmetric between entry points; compatibility mode is not,
//! and that asymmetry is deliberate.** [`VmConfig::default`] (the embedding /
//! in-tree-test path) is [`JdkMode::Synthetic`] while
//! [`VmConfig::with_host_jdk_default`] (the launcher's base config, and the one
//! `libcratonvm`'s C entry points build on) is [`JdkMode::Real`], because the
//! two entry points genuinely want different class libraries. They **agree** on
//! [`CompatibilityMode::Compatible`] because neither may silently *want*
//! strictness: a caller that did not ask for it must never be given a policy
//! that rejects work it expects to succeed. Do not "align" the second split
//! with the first.
//!
//! JDK-only is an internal diagnostic, not a production posture — it is at
//! stage 1 of 4 of its rollout, so expect failures on programs that run fine
//! under `JdkMode::Real` + `Compatible`. See `docs/EMBEDDING.md` ("Choosing a
//! compatibility mode") and
//! `docs/feature-designs/jdk-only-mode.md` for the normative contract.
//!
//! ```
//! use cratonvm_embed::{CompatibilityMode, ExecutionPolicy, JdkMode, VmConfig, VmError};
//!
//! // Strict *and* coherent: the launcher base config already boots a real JDK,
//! // so `--jdk-only`'s "real class bytes are authoritative" rule is satisfiable.
//! let strict =
//!     VmConfig::with_host_jdk_default().with_compatibility_mode(CompatibilityMode::JdkOnly);
//! assert!(strict.is_jdk_only());
//! strict
//!     .validate_compatibility()
//!     .expect("real JDK + JDK-only is coherent");
//!
//! // The resolved token the native registry and class manager read at VM init.
//! let policy: ExecutionPolicy = strict.execution_policy();
//! assert!(policy.is_jdk_only() && policy.real_jdk);
//!
//! // Strict mode plus the synthetic class library asks for a VM whose whole
//! // class library is the ~5,200 stubs the policy forbids. That is rejected at
//! // configuration time, not later as an unexplained NoClassDefFoundError.
//! let conflict = strict.clone().with_jdk_mode(JdkMode::Synthetic);
//! match conflict.validate_compatibility() {
//!     Err(VmError::InvalidConfiguration(msg)) => {
//!         assert!(msg.contains("--jdk-only") && msg.contains("--synthetic-jdk"));
//!     }
//!     other => panic!("expected VmError::InvalidConfiguration, got {other:?}"),
//! }
//!
//! // Doing nothing is `Compatible`, on both entry points.
//! assert_eq!(
//!     VmConfig::default().compatibility_mode,
//!     CompatibilityMode::Compatible
//! );
//! assert_eq!(
//!     VmConfig::with_host_jdk_default().compatibility_mode,
//!     CompatibilityMode::Compatible
//! );
//! ```
//!
//! ## Lifecycle (API contract)
//!
//! ```text
//! Vm::new(VmConfig)        // construct + bootstrap to init level 1
//!   → System.initPhaseN    // advance init levels (the CLI/embedder drives these)
//!   → vm.invoke(...)        // drive static/instance calls
//!   → drop(vm)              // tear down
//! ```
//!
//! * **One `JvmThread` per Java thread.** `SharedVm` is `Send + Sync`; the
//!   per-thread `JvmThread` (inside `Vm`) is not shared across OS threads.
//! * **Natives are immutable after construction** — register them on the
//!   `VmConfig` / `SharedVm` before first use.
//! * **One VM per process** is the only tested configuration (the process-global
//!   signal handlers / sandbox roots make multiple or restarted VMs fragile).
//!
//! ## Example
//!
//! ```no_run
//! use cratonvm_embed::{Vm, VmConfig, Value};
//!
//! let mut vm = Vm::new(VmConfig::with_host_jdk_default());
//! // (drive System.initPhaseN here as the reference embedder does)
//! let args = cratonvm_embed::make_string_array(&mut vm, &["hello"]).unwrap();
//! let _ = vm.invoke("HelloWorld", "main", "([Ljava/lang/String;)V",
//!     &[Value::Object(Some(args))]);
//! ```

#![forbid(unsafe_code)]

// ---------------------------------------------------------------------------
// Curated re-exports — the supported, semver-stable surface.
// ---------------------------------------------------------------------------

pub use cratonvm_vm::config::VmConfig;
/// The configuration-policy tokens: *which substitutions are permitted*
/// ([`CompatibilityMode`] / the resolved [`ExecutionPolicy`]) and *which class
/// library boots* ([`JdkMode`]).
///
/// Re-exported because the facade otherwise exposed only [`VmConfig`], leaving
/// an embedder unable to name the argument or return types of
/// [`VmConfig::with_compatibility_mode`], [`VmConfig::with_jdk_mode`] and
/// [`VmConfig::execution_policy`] without depending on `cratonvm-vm` directly.
/// See the crate-level "Compatibility mode" section for the resolution rules.
pub use cratonvm_vm::config::{CompatibilityMode, ExecutionPolicy, JdkMode};
pub use cratonvm_vm::error::{MethodCallFailed, MethodCallResult, VmError};
pub use cratonvm_vm::threading::{JvmThread, ThreadId};
pub use cratonvm_vm::types::{ObjectRef, Value};
pub use cratonvm_vm::vm::{SharedVm, StackTraceFrame, Vm};
pub use cratonvm_vm::ClassId;

// ---------------------------------------------------------------------------
// Conveniences — the small set of helpers an embedder always reaches for.
// ---------------------------------------------------------------------------

/// Build a `java.lang.String[]` from Rust strings — the array a host passes to
/// a `main(String[])` (or any `[Ljava/lang/String;` parameter).
///
/// Each element is a fresh, uninterned `java.lang.String`, matching launcher
/// argument behavior rather than string-literal (`ldc`) interning. Returns the
/// array handle, or a [`VmError`] if `java.lang.String` cannot be loaded. String
/// allocation itself uses the VM allocator and can still abort on heap
/// exhaustion, matching the underlying VM helper contract.
pub fn make_string_array(vm: &mut Vm, items: &[&str]) -> Result<ObjectRef, VmError> {
    let string_class = vm.load_class("java/lang/String")?;
    let arr = vm.new_ref_array(string_class, items.len());
    for (i, s) in items.iter().enumerate() {
        let js = cratonvm_vm::vm::create_java_string_uninterned(&vm.shared, s);
        // Index is in `[0, items.len())` by construction, so the bounds-check
        // (the `Err(i32)` AIOOBE index) cannot fire; ignore it.
        let _ = vm
            .shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Object(Some(js)));
    }
    Ok(arr)
}

/// Read a `java.lang.String` handle back into a Rust `String`. Returns `None`
/// if the handle is not a `java.lang.String`.
pub fn read_string(vm: &Vm, obj: ObjectRef) -> Option<String> {
    cratonvm_vm::vm::read_java_string(&vm.shared.mem.heap, obj)
}

/// The runtime-class internal name of a heap object (`"java/lang/String"`),
/// or `None` if the class is unresolvable.
pub fn object_class_name(vm: &Vm, obj: ObjectRef) -> Option<String> {
    let class_id = vm.shared.mem.heap.class_id_of(obj);
    vm.class_name(class_id)
}

/// A human-readable description of a failed call: the internal VM error text,
/// or — for a thrown Java exception — the runtime class name of the throwable.
/// (Reading the throwable's `getMessage()` requires a live call, so this
/// read-only helper reports the class; invoke `getMessage` yourself if needed.)
pub fn describe_failure(vm: &Vm, e: &MethodCallFailed) -> String {
    match e {
        MethodCallFailed::InternalError(err) => err.to_string(),
        MethodCallFailed::ExceptionThrown(obj) => {
            let cls = object_class_name(vm, *obj).unwrap_or_else(|| "<unknown>".to_string());
            format!("{cls} thrown")
        }
    }
}

/// Resolve a named instance field on `class_id` to its layout slot index (most-
/// derived declaration wins; `None` if the class is unloaded or has no such
/// instance field). Thin pass-through to [`Vm::instance_field_index`] for symmetry
/// with the other facade helpers.
pub fn field_index(vm: &Vm, class_id: ClassId, name: &str) -> Option<usize> {
    vm.instance_field_index(class_id, name)
}

/// Descriptor-disambiguated field resolution: resolve `name` to its layout slot,
/// optionally requiring its JVM type `descriptor` (`"I"`, `"Ljava/lang/String;"`,
/// …) to match. `Some(descriptor)` lets a caller address a **shadowed**
/// super-class field that a subclass re-declares with the same name; `None` is
/// identical to [`field_index`] (most-derived wins). Thin pass-through to
/// [`Vm::instance_field_index_desc`].
pub fn field_index_desc(
    vm: &Vm,
    class_id: ClassId,
    name: &str,
    descriptor: Option<&str>,
) -> Option<usize> {
    vm.instance_field_index_desc(class_id, name, descriptor)
}

/// Read a named instance field of `obj`, resolved against its **runtime** class.
/// `None` if the field name is unknown.
pub fn get_field_by_name(vm: &Vm, obj: ObjectRef, name: &str) -> Option<Value> {
    let class_id = vm.shared.mem.heap.class_id_of(obj);
    let idx = vm.instance_field_index(class_id, name)?;
    Some(vm.get_instance_field(obj, idx))
}

/// Write `value` into a named instance field of `obj`, resolved against its
/// **runtime** class — GC-barrier correct (see [`Vm::set_instance_field`]).
/// Returns `true` if the field was found and written, `false` if the name is
/// unknown. No type coercion is performed; the `Value` variant should match the
/// field's declared type.
pub fn set_field_by_name(vm: &Vm, obj: ObjectRef, name: &str, value: Value) -> bool {
    let class_id = vm.shared.mem.heap.class_id_of(obj);
    match vm.instance_field_index(class_id, name) {
        Some(idx) => {
            vm.set_instance_field(obj, idx, value);
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time pin that the curated surface re-exports the supported types.
    /// (A live round-trip would require the full VM bootstrap; the `libcratonvm`
    /// crate's `flat_api_live_vm` test covers the runtime path.)
    #[test]
    fn facade_reexports_exist() {
        fn _assert_sized<T>() {}
        _assert_sized::<VmConfig>();
        _assert_sized::<Vm>();
        _assert_sized::<SharedVm>();
        _assert_sized::<JvmThread>();
        _assert_sized::<ThreadId>();
        _assert_sized::<ClassId>();
        _assert_sized::<Value>();
        _assert_sized::<ObjectRef>();
        _assert_sized::<VmError>();
        _assert_sized::<MethodCallFailed>();
        _assert_sized::<StackTraceFrame>();
        _assert_sized::<CompatibilityMode>();
        _assert_sized::<ExecutionPolicy>();
        _assert_sized::<JdkMode>();
    }

    /// An embedded VM is `Compatible` unless the host says otherwise — on
    /// **both** constructors, and regardless of which class library they boot.
    ///
    /// This is the asymmetry pinned: `default()` and `with_host_jdk_default()`
    /// disagree about [`JdkMode`] (hermetic library vs. host JDK) and agree
    /// about [`CompatibilityMode`]. Strictness rejects work that `Compatible`
    /// accepts, so it may only ever come from an explicit request; a Cargo
    /// feature, an environment variable, or the other entry point's default
    /// must never supply it.
    #[test]
    fn embedded_default_is_compatible() {
        for cfg in [VmConfig::default(), VmConfig::with_host_jdk_default()] {
            assert_eq!(cfg.compatibility_mode, CompatibilityMode::Compatible);
            assert!(!cfg.is_jdk_only());
            assert!(!cfg.execution_policy().is_jdk_only());
            cfg.validate_compatibility()
                .expect("a default config is always coherent");
        }
        // The JDK mode is the half that *does* differ between the two.
        assert_eq!(VmConfig::default().jdk_mode(), JdkMode::Synthetic);
        assert_eq!(
            VmConfig::with_host_jdk_default().jdk_mode(),
            JdkMode::Real,
            "the launcher base config boots the host JDK"
        );
    }

    /// An explicit strict request round-trips through the facade's own types:
    /// what the host set is what `is_jdk_only` / `execution_policy` report.
    #[test]
    fn explicit_jdk_only_round_trips() {
        let cfg =
            VmConfig::with_host_jdk_default().with_compatibility_mode(CompatibilityMode::JdkOnly);
        assert_eq!(cfg.compatibility_mode, CompatibilityMode::JdkOnly);
        assert!(cfg.is_jdk_only());
        assert_eq!(cfg.compatibility_mode.as_str(), "jdk-only");

        let policy: ExecutionPolicy = cfg.execution_policy();
        assert_eq!(policy.compatibility_mode, CompatibilityMode::JdkOnly);
        assert!(policy.is_jdk_only());
        assert!(policy.real_jdk, "the launcher base config is real-JDK");
        assert_eq!(policy, ExecutionPolicy::jdk_only());

        // Setting the compatibility mode does not rewrite the JDK mode: this
        // config was already real, and quietly forcing `JdkMode::Real` would
        // erase the conflict `validate_compatibility` exists to report.
        assert_eq!(cfg.jdk_mode(), JdkMode::Real);
        cfg.validate_compatibility()
            .expect("real JDK + JDK-only is coherent");
    }

    /// Strict mode plus the synthetic class library is a configuration error,
    /// reported before the VM is built. The message names both flags and both
    /// ways out, because which correction is right depends on what the caller
    /// meant.
    #[test]
    fn jdk_only_plus_synthetic_is_rejected() {
        let cfg = VmConfig::default() // synthetic library
            .with_compatibility_mode(CompatibilityMode::JdkOnly);
        assert_eq!(cfg.jdk_mode(), JdkMode::Synthetic);
        assert!(cfg.is_jdk_only());

        match cfg.validate_compatibility() {
            Err(VmError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("--jdk-only"), "{msg}");
                assert!(msg.contains("--synthetic-jdk"), "{msg}");
            }
            other => panic!("expected VmError::InvalidConfiguration, got {other:?}"),
        }
    }

    /// Compile-time pin for the facade helper signatures documented as the
    /// stable embedding surface. These do not bootstrap a VM, so they stay
    /// cheap and avoid depending on host JDK availability.
    #[test]
    fn facade_helper_signatures_are_pinned() {
        type MakeStringArrayFn = for<'vm, 'items, 'item> fn(
            &'vm mut Vm,
            &'items [&'item str],
        ) -> Result<ObjectRef, VmError>;
        type ReadStringFn = for<'vm> fn(&'vm Vm, ObjectRef) -> Option<String>;
        type ObjectClassNameFn = for<'vm> fn(&'vm Vm, ObjectRef) -> Option<String>;
        type DescribeFailureFn = for<'vm, 'err> fn(&'vm Vm, &'err MethodCallFailed) -> String;
        type FieldIndexFn = for<'vm, 'name> fn(&'vm Vm, ClassId, &'name str) -> Option<usize>;
        type FieldIndexDescFn = for<'vm, 'name, 'desc> fn(
            &'vm Vm,
            ClassId,
            &'name str,
            Option<&'desc str>,
        ) -> Option<usize>;
        type GetFieldByNameFn = for<'vm, 'name> fn(&'vm Vm, ObjectRef, &'name str) -> Option<Value>;
        type SetFieldByNameFn = for<'vm, 'name> fn(&'vm Vm, ObjectRef, &'name str, Value) -> bool;

        let _make: MakeStringArrayFn = make_string_array;
        let _read: ReadStringFn = read_string;
        let _object_class: ObjectClassNameFn = object_class_name;
        let _describe: DescribeFailureFn = describe_failure;
        let _field_index: FieldIndexFn = field_index;
        let _field_index_desc: FieldIndexDescFn = field_index_desc;
        let _get_field: GetFieldByNameFn = get_field_by_name;
        let _set_field: SetFieldByNameFn = set_field_by_name;
    }
}
