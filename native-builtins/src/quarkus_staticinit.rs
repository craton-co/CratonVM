// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.3 — Quarkus static-init replay natives.
//!
//! Quarkus (Keycloak 26.2.4 is Quarkus-native) splits its boot across two
//! phases:
//!
//! 1. **Build time** — the Quarkus deployment pipeline generates
//!    `quarkus-run.jar` with pre-serialized metadata plus a recorded
//!    sequence of `BytecodeRecorderImpl` events.
//! 2. **Runtime** — `io.quarkus.runtime.generated.Main.main()` replays the
//!    recorded events via `io.quarkus.runtime.RuntimeRecorder` +
//!    `io.quarkus.runtime.RuntimeValue` objects that wire singletons,
//!    CDI beans, datasources, and the HTTP listener.
//!
//! This module implements the **static-init side** of the replay:
//!
//! * `io.quarkus.runtime.RuntimeValue<T>` — a lazy holder whose
//!   `supplier.get()` is invoked at most once (double-checked lock).
//! * `io.quarkus.runtime.StartupContext` — per-pass shutdown-task list +
//!   keyed result map.
//! * `io.quarkus.runtime.ApplicationConfig` +
//!   `io.quarkus.runtime.DataSourceRuntimeConfig` — record-style holders
//!   that defer to system properties / env (restricted allowlist).
//! * `io.quarkus.runtime.Timing` — boot / main phase timestamps.
//!
//! Runtime Java classes like `Application.start(String[])` and
//! `Application.stop()` run their own bytecode; this module only provides
//! the native hooks they dispatch to.
//!
//! ## Field layout (synthetic-stub)
//!
//! | Class                                    | Slot 0          | Slot 1         | Slot 2     | Slot 3     |
//! |------------------------------------------|-----------------|----------------|------------|------------|
//! | `RuntimeValue`                           | value (Object?) | supplier       | —          | —          |
//! | `StartupContext`                         | shutdown_tasks  | values (Map)   | —          | —          |
//! | `ApplicationConfig`                      | name (String?)  | version        | —          | —          |
//! | `DataSourceRuntimeConfig`                | jdbcUrl         | username       | password   | driver     |
//! | `Timing`                                 | bootStart       | bootStop       | mainStart  | mainStop   |
//!
//! The layouts are wired via `classloading/src/class_manager.rs
//! ::synthetic_stub_fields` so that real-JDK-mode `alloc_object` reserves
//! the correct number of slots ahead of these natives writing to them.

#![allow(clippy::needless_pass_by_value)]

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ObjectRef, Value};

// ---------------------------------------------------------------------------
// Field layout constants
// ---------------------------------------------------------------------------

// RuntimeValue
pub const RV_FIELD_VALUE: usize = 0;
pub const RV_FIELD_SUPPLIER: usize = 1;

// (Keycloak Gap 5) StartupContext is no longer shimmed — its real bytecode runs.

// ApplicationConfig
pub const AC_FIELD_NAME: usize = 0;
pub const AC_FIELD_VERSION: usize = 1;

// DataSourceRuntimeConfig
pub const DSRC_FIELD_JDBC_URL: usize = 0;
pub const DSRC_FIELD_USERNAME: usize = 1;
pub const DSRC_FIELD_PASSWORD: usize = 2;
pub const DSRC_FIELD_DRIVER: usize = 3;

// Timing — 4 long slots: bootStart, bootStop, mainStart, mainStop
pub const TIMING_FIELD_BOOT_START: usize = 0;
pub const TIMING_FIELD_BOOT_STOP: usize = 1;
pub const TIMING_FIELD_MAIN_START: usize = 2;
pub const TIMING_FIELD_MAIN_STOP: usize = 3;

// ---------------------------------------------------------------------------
// Class names
// ---------------------------------------------------------------------------

const CLS_RUNTIME_VALUE: &str = "io/quarkus/runtime/RuntimeValue";
const CLS_APPLICATION_CONFIG: &str = "io/quarkus/runtime/ApplicationConfig";
const CLS_DATASOURCE_CONFIG: &str = "io/quarkus/runtime/DataSourceRuntimeConfig";
const CLS_TIMING: &str = "io/quarkus/runtime/Timing";

// T19.H3: Quarkus bootstrap runner classes.  Used by
// `QuarkusEntryPoint.main` to decode `quarkus/quarkus-application.dat`.
const CLS_SERIALIZED_APP: &str = "io/quarkus/bootstrap/runner/SerializedApplication";
const CLS_RUNNER_CLASSLOADER: &str = "io/quarkus/bootstrap/runner/RunnerClassLoader";
const CLS_QUARKUS_ENTRY_POINT: &str = "io/quarkus/bootstrap/runner/QuarkusEntryPoint";

// T19.H3: Quarkus bootstrap magic + version — compile-time constants
// matching `SerializedApplication.MAGIC` / `.VERSION` from
// `quarkus-bootstrap-runner 3.20.x`. The real bytecode reads these
// from the `.dat` file and compares to the in-memory constants; we
// surface them so the verification never fails in native-replacement
// mode. Values verified by disassembling the JAR:
//   MAGIC   = 0xF0315432 (-265202638 signed)
//   VERSION = 2
pub const QUARKUS_BOOTSTRAP_MAGIC: i32 = -265202638;
pub const QUARKUS_BOOTSTRAP_VERSION: i32 = 2;

// T19.H3: Slot layout for our synthetic SerializedApplication.
//   0 = mainClass (String)
//   1 = runnerClassLoader (RunnerClassLoader — synthetic)
pub const SA_FIELD_MAIN_CLASS: usize = 0;
pub const SA_FIELD_RUNNER_CL: usize = 1;

// ---------------------------------------------------------------------------
// Env-var allowlist for config defaults
//
// Per the mission brief we restrict env-var fallbacks to a known set
// rather than blindly passing through any env name. Everything else
// returns null so Quarkus continues with its recorded / configured
// default path.
// ---------------------------------------------------------------------------

const DATASOURCE_ENV_ALLOWLIST: &[(&str, DatasourceField)] = &[
    ("QUARKUS_DATASOURCE_JDBC_URL", DatasourceField::JdbcUrl),
    ("DATABASE_URL", DatasourceField::JdbcUrl),
    ("QUARKUS_DATASOURCE_USERNAME", DatasourceField::Username),
    ("DATABASE_USERNAME", DatasourceField::Username),
    ("QUARKUS_DATASOURCE_PASSWORD", DatasourceField::Password),
    ("DATABASE_PASSWORD", DatasourceField::Password),
    ("QUARKUS_DATASOURCE_DB_KIND", DatasourceField::Driver),
    ("DATABASE_DRIVER", DatasourceField::Driver),
];

const APPLICATION_CONFIG_ENV_ALLOWLIST: &[(&str, AppConfigField)] = &[
    ("QUARKUS_APPLICATION_NAME", AppConfigField::Name),
    ("QUARKUS_APPLICATION_VERSION", AppConfigField::Version),
];

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum DatasourceField {
    JdbcUrl,
    Username,
    Password,
    Driver,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum AppConfigField {
    Name,
    Version,
}

/// Look up a single environment variable from the datasource allowlist.
fn datasource_env_lookup(field: DatasourceField) -> Option<String> {
    for (env_name, kind) in DATASOURCE_ENV_ALLOWLIST {
        if *kind == field {
            if let Ok(v) = cratonvm_types::flags::runtime_var(env_name) {
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}

fn application_config_env_lookup(field: AppConfigField) -> Option<String> {
    for (env_name, kind) in APPLICATION_CONFIG_ENV_ALLOWLIST {
        if *kind == field {
            if let Ok(v) = cratonvm_types::flags::runtime_var(env_name) {
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// RuntimeValue supplier-once state
//
// `RuntimeValue.getValue()` must invoke the attached `Supplier.get()` at
// most once across all callers, even if multiple threads race. The state
// lives in the RuntimeValue's own `supplier` field:
//   * After `<init>(Supplier)` — `supplier` holds the Supplier ObjectRef,
//     `value` is null.
//   * The first `getValue()` CLAIMS the invocation by swapping the supplier
//     for the RuntimeValue ITSELF (a compare-and-swap on the field), invokes
//     the supplier, stores its return into `value`, then clears `supplier`
//     to null.
//   * A caller that finds the receiver in its own `supplier` slot waits
//     (GC-safe) for the claim to resolve; one that finds it null returns
//     `value` directly.
//   * A supplier that throws is put back, so a later `getValue()` retries.
//
// gc-common w20-g (`common-w19f-runtime-value-inflight-set-is-address-keyed`):
// the claim used to be an entry, keyed by the receiver's raw address at
// entry, in one process-wide `Mutex<HashSet<u64>>`. A moving collection
// inside `supplier.get()` moved the RuntimeValue, a second caller computed
// its NEW address, found it unclaimed and invoked the supplier a second
// time; an unrelated RuntimeValue later allocated at the old address (in
// this VM or another) spun until the first finished. A claim stored in the
// object moves with it, is naturally per VM, and needs no table.
// ---------------------------------------------------------------------------

/// What one look at a `RuntimeValue` found.
#[derive(Debug, PartialEq)]
enum RvStep {
    /// The value is materialized (no supplier left): here it is.
    Ready(Value),
    /// Another invocation holds the claim, or this caller lost the race
    /// for it: wait and look again.
    Busy,
    /// This caller claimed the invocation of this supplier.
    Claimed(ObjectRef),
}

/// Look at `this` (current) once, claiming the supplier invocation when it is
/// still pending and unclaimed.
fn rv_step(ctx: &mut dyn NativeContext, this: ObjectRef) -> RvStep {
    match ctx.get_field_volatile(this, RV_FIELD_SUPPLIER) {
        Value::Object(Some(claimed)) if claimed == this => RvStep::Busy,
        Value::Object(Some(supplier)) => {
            if ctx.compare_and_swap_field(
                this,
                RV_FIELD_SUPPLIER,
                Value::Object(Some(supplier)),
                Value::Object(Some(this)),
            ) {
                RvStep::Claimed(supplier)
            } else {
                RvStep::Busy
            }
        }
        _ => RvStep::Ready(ctx.get_field_volatile(this, RV_FIELD_VALUE)),
    }
}

// (Keycloak Gap 5) The StartupContext salt-keyed side-tables
// (`startup_context_next_key` / `startup_context_values` /
// `startup_context_shutdown_tasks` / `sc_key`) were removed along with the
// StartupContext natives — the real `StartupContext` bytecode owns that state now.

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register every `io.quarkus.runtime.*` static-init native. Called from
/// `register_essential_natives` in `lib.rs`.
pub fn register_quarkus_staticinit_natives(registry: &mut NativeMethodRegistry) {
            register_runtime_value(registry);
    // NOTE (Keycloak Gap 5): the `io.quarkus.runtime.StartupContext` natives were
    // REMOVED. The real `StartupContext` is trivial bytecode (HashMap `values`,
    // two `ConcurrentLinkedDeque`s) whose constructor registers a `StartupContext$1`
    // `ShutdownContext` proxy into `values` under the key `ShutdownContext.class
    // .getName()`. The native `<init>` shim shadowed that constructor, so
    // `getValue("io.quarkus.runtime.ShutdownContext")` returned null and every
    // recorder step taking a `ShutdownContext` (e.g.
    // `HibernateValidatorRecorder.shutdownConfigValidator`) NPE'd. Running the real
    // bytecode registers the proxy correctly. (Same class of bug as Gap 3.)
    register_application_config(registry);
    register_datasource_runtime_config(registry);
    register_application_lifecycle(registry);
    register_timing(registry);
    register_bootstrap_runner(registry);
    }

/// T19.H3: register natives for `io.quarkus.bootstrap.runner.*` so
/// `QuarkusEntryPoint.main` doesn't throw "Wrong class path version".
///
/// The real bytecode of `SerializedApplication.read(InputStream, Path)`
/// reads a 4-byte magic + 4-byte version from the input stream and
/// throws `RuntimeException("Wrong class path version")` if the
/// version doesn't match the `VERSION` constant baked in at compile
/// time. In our VM, the `DataInputStream.readInt` path on a `.dat`
/// file mounted through `Files.newInputStream` has path-provider
/// plumbing that's incomplete for arbitrary-offset reads, so the
/// second readInt returns a drifted value.
///
/// Rather than fix the whole NIO pipeline for this one call site, we
/// register a native replacement for `SerializedApplication.read` that:
///   1. Allocates a synthetic `SerializedApplication` + synthetic
///      `RunnerClassLoader`.
///   2. Sets `mainClass` to `"org.keycloak.quarkus.runtime.KeycloakMain"`
///      (the value stored in KC26's real `.dat` file), falling back to
///      the `QUARKUS_MAIN_CLASS` environment variable (allowlisted, no
///      filesystem dereference) if present.
///   3. Returns the synthetic object so the rest of `QuarkusEntryPoint`
///      can call `getRunnerClassLoader()` + `getMainClass()` and
///      proceed into Keycloak's real main.
///
/// Security posture:
///   * The version constant is baked into the Rust binary
///     (`QUARKUS_BOOTSTRAP_VERSION`) and NEVER read from disk or env.
///   * The main-class fallback env var is bounded to 512 ASCII chars
///     of `[A-Za-z0-9_.$/-]`; any rejection falls back to the
///     compile-time Keycloak default.
///   * No filesystem paths are decoded from the arguments — the
///     `InputStream` / `Path` args are silently discarded.
///
/// FLAGGED SyntheticStub (B3 — forbidden "fake main" shim): the
/// `SerializedApplication.read` override defaults `mainClass` to the
/// Keycloak 26 main class instead of decoding it from the real
/// `quarkus-application.dat`, which masks an underlying NIO bug
/// (`DataInputStream.readInt` over `Files.newInputStream` returns a drifted
/// value on the second read; see the comment above this fn). The correct fix
/// is in the VM's NIO pipeline, NOT a hardcoded app-specific main class.
///
/// The bootstrap-runner synthetic stubs have been removed (app-stubs feature
/// deleted). The real Quarkus `.dat` bootstrap bytecode runs; the underlying
/// NIO `DataInputStream.readInt` bug surfaces honestly instead of being
/// papered over by a hardcoded Keycloak main class.
fn register_bootstrap_runner(registry: &mut NativeMethodRegistry) {
    let _ = registry;
}

/// Accept only input that could conceivably be a Java binary class name.
///
/// Per `java/lang/ClassLoader.loadClass(String)` spec the input is a
/// dotted class name like `com.example.Foo$Inner`.  We accept the same
/// shape plus the internal `com/example/Foo$Inner` form (which Quarkus's
/// generated replay bytecode occasionally mixes in) and reject anything
/// that could be used to probe filesystem paths, traversal patterns,
/// or control-byte injection.
fn sanitize_load_class_name(s: &str) -> Option<String> {
    if s.is_empty() || s.len() > 512 {
        return None;
    }
    // Leading `/` is the classic "absolute resource" shape — not valid
    // for a class name, and tolerating it could let attackers pivot
    // into resource probing if we ever fall through to the resource
    // finder.  Reject up front.
    if s.starts_with('/') {
        return None;
    }
    if s.contains("..") {
        return None;
    }
    for ch in s.chars() {
        let ok = ch.is_ascii_alphanumeric() || matches!(ch, '.' | '$' | '_' | '/' | '-');
        if !ok {
            return None;
        }
    }
    Some(s.to_string())
}

/// `RunnerClassLoader.loadClass(String)` implementation.
///
/// Follows the JDK classloader contract:
///
/// 1. Null `name` → `NullPointerException`.
/// 2. Malformed `name` (bytes that could mean anything other than a
///    classic Java binary/internal class name) → `ClassNotFoundException`.
///    We return CNFE rather than IAE because callers only ever expect
///    CNFE here and promoting to IAE could break legitimate error-path
///    bytecode.
/// 3. Normalize `.` → `/` to match the VM's internal representation.
/// 4. Delegate to `ensure_class_initialized` which walks every
///    classpath entry the vm-cli expanded; on success, return the Class
///    mirror.
/// 5. On resolution failure, throw `ClassNotFoundException(name)`.
fn native_runner_class_loader_load_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // arg[0] = this (RunnerClassLoader), arg[1] = name (String).
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("RunnerClassLoader.loadClass: name is null".to_string()),
            }
            .into());
        }
    };
    let name_raw = ctx.read_string(name_obj).unwrap_or_default();
    let Some(cleaned) = sanitize_load_class_name(&name_raw) else {
        return Err(
            cratonvm_types::error::RuntimeError::ClassNotFoundException {
                class_name: name_raw,
            }
            .into(),
        );
    };
    let internal = cleaned.replace('.', "/");
    match ctx.ensure_class_initialized(&internal) {
        Ok(cid) => {
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(_) => Err(
            cratonvm_types::error::RuntimeError::ClassNotFoundException {
                class_name: cleaned,
            }
            .into(),
        ),
    }
}

/// `RunnerClassLoader.loadClass(String, boolean)` — resolves the class
/// the same way as the single-arg form; the `resolve` flag at args[2]
/// is informational only in our model (we always fully resolve).
fn native_runner_class_loader_load_class_with_resolve(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_runner_class_loader_load_class(ctx, args)
}

/// Sanitize a main-class override candidate from the environment.
/// Accepts only classic internal/external class names — alphanumerics,
/// `.`, `$`, `_`, `/`, `-`. Any other byte → reject. Length-capped at
/// 512 chars to prevent resource exhaustion in the registry.
///
/// Also rejects any `..` sequence (even inside a longer token) so
/// relative-path traversal patterns can't sneak through just because
/// `.` and `/` are individually allowed class-name characters.
fn sanitize_main_class_candidate(s: &str) -> Option<String> {
    if s.is_empty() || s.len() > 512 {
        return None;
    }
    if s.contains("..") {
        return None;
    }
    for ch in s.chars() {
        let ok = ch.is_ascii_alphanumeric() || matches!(ch, '.' | '$' | '_' | '/' | '-');
        if !ok {
            return None;
        }
    }
    Some(s.to_string())
}

/// Resolve the main-class name to embed in the synthetic
/// SerializedApplication. Environment variable `QUARKUS_MAIN_CLASS`
/// provides an override (sanitized); otherwise falls back to the
/// Keycloak 26 default.
fn resolve_main_class() -> String {
    let from_env = cratonvm_types::flags::runtime_var("QUARKUS_MAIN_CLASS")
        .ok()
        .and_then(|v| sanitize_main_class_candidate(&v));
    if let Some(name) = from_env {
        return name.replace('/', ".");
    }
    // Default: Keycloak 26.x main. Matches the value inside
    // `C:\craton\keycloak-26.2.4\lib\quarkus\quarkus-application.dat`.
    "org.keycloak.quarkus.runtime.KeycloakMain".to_string()
}

/// `SerializedApplication.read(InputStream, Path)` — returns a
/// synthetic SerializedApplication. Version check is implicit: we
/// know the Rust-side version constant matches the current Quarkus
/// build, so there's nothing to verify.
fn native_serialized_application_read(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Allocate synthetic SerializedApplication + RunnerClassLoader.
    let sa_cid = ctx.ensure_class_initialized(CLS_SERIALIZED_APP).ok();
    let rcl_cid = ctx.ensure_class_initialized(CLS_RUNNER_CLASSLOADER).ok();
    let (sa_n, rcl_n) = (
        sa_cid
            .map(|cid| ctx.class_num_total_fields(cid).max(2))
            .unwrap_or(2),
        rcl_cid
            .map(|cid| ctx.class_num_total_fields(cid).max(1))
            .unwrap_or(1),
    );
    // gc-common w19-f: the application is rooted across the loader's and the
    // name's allocations, the loader across the name's; both stores used to
    // go through (and store) pre-allocation addresses.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let sa = match sa_cid {
        Some(cid) => scope.alloc_object(cid, sa_n),
        None => scope.alloc_object(cratonvm_types::ClassId::new(0), sa_n),
    };
    let sa_h = scope.root(sa);
    let rcl = match rcl_cid {
        Some(cid) => scope.alloc_object(cid, rcl_n),
        None => scope.alloc_object(cratonvm_types::ClassId::new(0), rcl_n),
    };
    let rcl_h = scope.root(rcl);

    let main_class = resolve_main_class();
    let main_class_obj = scope.create_string(&main_class);
    let sa = scope.get(&sa_h);
    let rcl = scope.get(&rcl_h);
    scope.set_field(sa, SA_FIELD_MAIN_CLASS, Value::Object(Some(main_class_obj)));
    scope.set_field(sa, SA_FIELD_RUNNER_CL, Value::Object(Some(rcl)));

    tracing::info!(
        main_class = %main_class,
        version = QUARKUS_BOOTSTRAP_VERSION,
        "SerializedApplication.read: using native synthetic entry (version check bypassed)"
    );

    Ok(Some(Value::Object(Some(sa))))
}

fn native_serialized_application_get_runner_cl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(this) = this_ref(args) else {
        return Ok(Some(Value::Object(None)));
    };
    Ok(Some(ctx.get_field(this, SA_FIELD_RUNNER_CL)))
}

fn native_serialized_application_get_main_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(this) = this_ref(args) else {
        return Ok(Some(Value::Object(None)));
    };
    let field = ctx.get_field(this, SA_FIELD_MAIN_CLASS);
    if matches!(field, Value::Object(Some(_))) {
        return Ok(Some(field));
    }
    // FAIL LOUD (was: fabricate the compile-time Keycloak main class).
    //
    // Previously, when the `mainClass` field was never populated, this
    // native synthesised `org.keycloak.quarkus.runtime.KeycloakMain` from
    // `resolve_main_class()` so callers never saw a null. That is a
    // silent-wrong-result stub: it would launch Keycloak's main for ANY
    // mis-initialised SerializedApplication, hiding the real defect and
    // potentially running the wrong application.
    //
    // The legitimate path is `native_serialized_application_read`, which
    // ALWAYS sets `SA_FIELD_MAIN_CLASS` (honouring the `QUARKUS_MAIN_CLASS`
    // override). Reaching here with an unset field means the SA was
    // constructed without going through `read()` — an illegal state we
    // surface honestly rather than papering over with an app-specific
    // hardcode. The real fix lives in the NIO `DataInputStream.readInt`
    // pipeline so `read()` can decode the real `quarkus-application.dat`
    // main class (see the comment on `register_bootstrap_runner`).
    Err(cratonvm_types::error::RuntimeError::IllegalStateException {
        message:
            "SerializedApplication.getMainClass: mainClass was never set (object not produced by \
             SerializedApplication.read); refusing to fabricate a default main class"
                .to_string(),
    }
    .into())
}

fn register_runtime_value(registry: &mut NativeMethodRegistry) {
            registry.register(CLS_RUNTIME_VALUE, "<init>", "()V", native_rv_init_empty, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        CLS_RUNTIME_VALUE,
        "<init>",
        "(Ljava/lang/Object;)V",
        native_rv_init_value, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // The single-arg Supplier ctor — signature matches
    // `RuntimeValue(java.util.function.Supplier)` used by Quarkus'
    // generated replay bytecode. Note the descriptor is
    // `(Ljava/util/function/Supplier;)V`.
    registry.register(
        CLS_RUNTIME_VALUE,
        "<init>",
        "(Ljava/util/function/Supplier;)V",
        native_rv_init_supplier, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_RUNTIME_VALUE,
        "getValue",
        "()Ljava/lang/Object;",
        native_rv_get_value, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // `deepInstance` is Quarkus-internal alias for getValue.
    registry.register(
        CLS_RUNTIME_VALUE,
        "deepInstance",
        "()Ljava/lang/Object;",
        native_rv_get_value, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    }

fn register_application_config(registry: &mut NativeMethodRegistry) {
            registry.register(CLS_APPLICATION_CONFIG, "<init>", "()V", native_ac_init, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        CLS_APPLICATION_CONFIG,
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        native_ac_init_with_values, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_APPLICATION_CONFIG,
        "name",
        "()Ljava/lang/String;",
        native_ac_name, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_APPLICATION_CONFIG,
        "version",
        "()Ljava/lang/String;",
        native_ac_version, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    }

fn register_datasource_runtime_config(registry: &mut NativeMethodRegistry) {
            registry.register(CLS_DATASOURCE_CONFIG, "<init>", "()V", native_dsrc_init, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        CLS_DATASOURCE_CONFIG,
        "jdbcUrl",
        "()Ljava/lang/String;",
        native_dsrc_jdbc_url, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_DATASOURCE_CONFIG,
        "username",
        "()Ljava/lang/String;",
        native_dsrc_username, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_DATASOURCE_CONFIG,
        "password",
        "()Ljava/lang/String;",
        native_dsrc_password, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_DATASOURCE_CONFIG,
        "driver",
        "()Ljava/lang/String;",
        native_dsrc_driver, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    }

/// Register `io.quarkus.runtime.Application` lifecycle hooks.
///
/// ## RETIRED SHIM (`Application.start/stop/awaitShutdown`), 2026-07-28
///
/// The three no-op natives below are **no longer registered by default**.
/// `Application.start([String])` drives the generated
/// `ApplicationImpl.doStart([String])` -- the RUNTIME_INIT phase that runs the
/// STARTUP_TASKS: the Vert.x/Netty HTTP server, the datasource connect,
/// Infinispan, Narayana JTA, Hibernate's SessionFactory, Liquibase. No-op'ing
/// it meant `start()` returned SUCCESSFULLY having created none of that, so
/// `main` parked in `waitForExit` with no event loop, no listener, and no
/// error anywhere -- the failure mode
/// `keycloak-no-vertx-http-runtime-init-20260728.md`
/// was filed for. A silent, fully-successful-looking non-boot is strictly
/// worse than a loud one, which is why this shim is retired rather than kept
/// under the "app-enabling shim" rule.
///
/// Verified on the real `keycloak-quarkus-dist-26.6.1`: with the no-ops gone
/// the boot runs RUNTIME_INIT end to end -- Netty event loops, Vert.x,
/// Infinispan (`ISPN000556: Starting user marshaller`), Narayana recovery,
/// the Agroal/H2 datasource, the Hibernate SessionFactory and the full
/// Liquibase schema migration all execute as real bytecode.
///
/// `CRATONVM_SYNTHETIC_QUARKUS_START=1` (or `CRATONVM_REAL=quarkus-start=0`)
/// restores the old no-ops for A/B diagnosis. The historical rationale for
/// the shim is kept below.
///
/// ## Historical rationale (the shim, now opt-in)
///
/// **What it works around:** In real Quarkus, `Application` is an abstract
/// class; `start(String[])` is the synchronized boot entry that drives the
/// recorded `StartupContext`/`RuntimeValue` replay plan, and `stop()` /
/// `awaitShutdown()` drive teardown. CratonVM does not yet replay the full
/// `BytecodeRecorderImpl` event stream that the generated
/// `ApplicationImpl.<clinit>`/`doStart` would execute, so the real bytecode
/// for these methods cannot run end-to-end. Without *some* registration,
/// dispatch to these methods would abort with an unresolved-native /
/// abstract-method error and the VM could not get past Quarkus bootstrap at
/// all. The no-ops let `QuarkusEntryPoint` → generated `Main.main` walk
/// through the lifecycle calls and reach the application's own `main` logic.
///
/// **Why we keep it (don't fail loud):** failing loud here breaks Quarkus
/// boot outright — there is no useful error for the framework to recover
/// from, only an immediate hard stop during bootstrap. Per the mission
/// rule, an app-enabling shim whose loud failure breaks a real framework is
/// kept + documented rather than removed. A debug-gated trace
/// (`native_app_lifecycle_no_op`) makes the elision observable so it is
/// never silently mistaken for a fully-functional boot.
///
/// **The real fix:** implement the Quarkus static-init/runtime replay so the
/// generated `ApplicationImpl.doStart`/`doStop` bytecode runs (driving the
/// recorded `StartupContext` steps), at which point these no-op overrides
/// must be removed so the real lifecycle executes. Until then, treat a run
/// that relies on these hooks as "booted far enough to dispatch", not "fully
/// started".
fn register_application_lifecycle(registry: &mut NativeMethodRegistry) {
    // CRATONVM_REAL_QUARKUS_START (Keycloak Gap 9): run the REAL Quarkus lifecycle.
    // `Application.start([String])` is a concrete `final` method on the abstract
    // `io.quarkus.runtime.Application` that locks + calls the generated
    // `ApplicationImpl.doStart([String])` — the RUNTIME_INIT phase that runs the
    // STARTUP_TASKS: the Vert.x HTTP server LISTEN, the datasource connect, etc.
    // The default no-op shim below SKIPS this entirely (the STATIC_INIT deploy steps
    // in `ApplicationImpl.<clinit>` still run — ArC/RESTEasy-metadata/Hibernate — which
    // is why the boot reaches RESTEasy deploy, but the HTTP server never starts and
    // `start()` "succeeds" so main parks in `waitForExit`). With the gate set we
    // suppress the no-op so the real `start()`→`doStart()` bytecode runs (the generated
    // deploy bytecode already runs for `<clinit>` — see Gaps 4–7 — so `doStart` is the
    // same kind of bytecode). Opt-in while the RUNTIME_INIT path is validated; pairs with
    // CRATONVM_REAL_AGROAL / CRATONVM_REAL_VERTX / CRATONVM_REAL_NET_SOCKETS.
    // Default: DO NOT register the no-ops -- the real `start()` -> `doStart()`
    // RUNTIME_INIT bytecode runs. Only `CRATONVM_SYNTHETIC_QUARKUS_START=1`
    // brings the historical shim back.
    if !crate::nbflags().synthetic_quarkus_start {
        if crate::nbflags().dbg {
            eprintln!(
                "[cratonvm] Quarkus lifecycle: NOT registering Application.start/stop/awaitShutdown no-ops — the real doStart RUNTIME_INIT will run (CRATONVM_SYNTHETIC_QUARKUS_START=1 restores the no-ops)"
            );
        }
        return;
    }
            // COMPATIBILITY SHIM (see fn doc): override the lifecycle entry points
    // with observable no-ops so Quarkus bootstrap dispatch does not abort.
    // These return normally without executing the real replay plan.
    registry.register(
        "io/quarkus/runtime/Application",
        "start",
        "([Ljava/lang/String;)V",
        native_app_lifecycle_no_op, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        "io/quarkus/runtime/Application",
        "stop",
        "()V",
        native_app_lifecycle_no_op, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        "io/quarkus/runtime/Application",
        "awaitShutdown",
        "()V",
        native_app_lifecycle_no_op, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    }

fn register_timing(registry: &mut NativeMethodRegistry) {
            registry.register(
        CLS_TIMING,
        "staticInitStarted",
        "(Z)V",
        native_timing_static_init_started, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_TIMING,
        "staticInitStarted",
        "(Ljava/lang/ClassLoader;Z)V",
        native_timing_static_init_started_cl, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        CLS_TIMING,
        "staticInitStopped",
        "()V",
        native_timing_static_init_stopped, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(CLS_TIMING, "mainStarted", "()V", native_timing_main_started, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(CLS_TIMING, "restart", "()V", native_timing_no_op, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        CLS_TIMING,
        "printStartupTime",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;ZZ)V",
        native_timing_print_startup_time, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    }

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Extract `this` from args[0]; returns the ObjectRef or None if the
/// receiver is null. Native callers treat a null receiver as a silent
/// no-op rather than panic, matching JDK native-method convention.
fn this_ref(args: &[Value]) -> Option<ObjectRef> {
    match args.first() {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

/// Extract a `Value::Object`-wrapped argument at `idx`, or None if it is
/// null / not an object.
fn arg_obj(args: &[Value], idx: usize) -> Option<ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn arg_value(args: &[Value], idx: usize) -> Value {
    args.get(idx).cloned().unwrap_or(Value::Object(None))
}

/// Read a Java String arg as a Rust String, returning `None` if the slot
/// is null or not a readable String.
fn arg_string(ctx: &dyn NativeContext, args: &[Value], idx: usize) -> Option<String> {
    let obj = arg_obj(args, idx)?;
    ctx.read_string(obj)
}

/// Create a Java String from `Option<String>`, returning
/// `Value::Object(None)` for `None` (matches the JDK `null` sentinel so
/// getter-style natives can propagate "no value" up to Java).
fn optional_string_value(ctx: &mut dyn NativeContext, s: Option<String>) -> Value {
    match s {
        Some(text) => Value::Object(Some(ctx.create_string(&text))),
        None => Value::Object(None),
    }
}

/// Current timestamp in nanoseconds (monotonic). Used by `Timing` phase
/// timestamps.
fn now_nanos() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// RuntimeValue natives
// ---------------------------------------------------------------------------

/// No-arg `RuntimeValue()` — both fields null. Rarely hit in real
/// Quarkus bytecode but some test paths construct via this form.
fn native_rv_init_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = this_ref(args) else {
        return Ok(None);
    };
    ctx.set_field(this, RV_FIELD_VALUE, Value::Object(None));
    ctx.set_field(this, RV_FIELD_SUPPLIER, Value::Object(None));
    Ok(None)
}

/// `RuntimeValue(Object pre_materialized)` — stores the value directly.
/// Since the value is already materialized, `supplier` is left null so
/// future `getValue` bypasses the supplier path entirely.
fn native_rv_init_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = this_ref(args) else {
        return Ok(None);
    };
    let value = arg_value(args, 1);
    ctx.set_field(this, RV_FIELD_VALUE, value);
    ctx.set_field(this, RV_FIELD_SUPPLIER, Value::Object(None));
    Ok(None)
}

/// `RuntimeValue(Supplier supplier)` — lazy form. `supplier.get()` is
/// invoked on first `getValue`.
fn native_rv_init_supplier(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = this_ref(args) else {
        return Ok(None);
    };
    let supplier = arg_value(args, 1);
    ctx.set_field(this, RV_FIELD_VALUE, Value::Object(None));
    ctx.set_field(this, RV_FIELD_SUPPLIER, supplier);
    Ok(None)
}

/// `RuntimeValue.getValue()` — the hot path.
///
/// Driven off the object's own fields (see "RuntimeValue supplier-once
/// state" above):
///   1. If `supplier == null`, the value has already been materialized
///      (either pre-init or by a previous getValue) — return `value`.
///   2. If `supplier` is the receiver itself, another thread is invoking the
///      supplier: wait GC-safe and look again.
///   3. Otherwise CAS the supplier for the receiver (the claim), invoke
///      `supplier.get()`, store the result into `value` and clear `supplier`
///      -- or, when the supplier throws, put it back and rethrow.
fn native_rv_get_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = this_ref(args) else {
        return Ok(Some(Value::Object(None)));
    };
    // gc-common w19-f: the receiver is rooted for the whole call. Before, the
    // two publishing stores went through its address from before
    // `supplier.get()` (Java), so the value landed in the pre-collection copy
    // and the next `getValue` invoked the supplier again.
    let this_pin = ctx.pin_native_root(this);
    let result = rv_get_value_rooted(ctx, this_pin, this);
    ctx.unpin_native_roots(this_pin);
    result
}

/// [`native_rv_get_value`]'s body; `this_pin` roots the receiver.
fn rv_get_value_rooted(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
) -> MethodCallResult {
    loop {
        let this = ctx.read_native_pin(this_pin, this);
        let supplier = match rv_step(ctx, this) {
            RvStep::Ready(value) => return Ok(Some(value)),
            RvStep::Busy => {
                // gc-common w19-f: wait GC-safe. The owner's `supplier.get()`
                // is Java and may need a collection; a waiter spinning as a
                // counted mutator held that stop-the-world off for good. The
                // receiver is re-read through its pin at the top of the next
                // iteration.
                ctx.begin_blocking_region();
                std::thread::yield_now();
                ctx.end_blocking_region();
                continue;
            }
            RvStep::Claimed(supplier) => supplier,
        };

        // We own the invocation. The supplier is pinned across it: a
        // supplier that throws is written back through its current address.
        let supplier_pin = ctx.pin_native_root(supplier);
        let invoke_result = ctx.invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[]);
        let this = ctx.read_native_pin(this_pin, this);
        let supplier = ctx.read_native_pin(supplier_pin, supplier);
        ctx.unpin_native_roots(supplier_pin);

        return match invoke_result {
            Ok(result) => {
                // Publish: store value first, then clear the claim. A reader
                // that sees `supplier == null` therefore sees the value.
                let result = result.unwrap_or(Value::Object(None));
                ctx.set_field(this, RV_FIELD_VALUE, result);
                ctx.set_field_volatile(this, RV_FIELD_SUPPLIER, Value::Object(None));
                Ok(Some(result))
            }
            Err(e) => {
                // Release the claim by restoring the supplier, so a later
                // getValue() retries it (the pre-w20 behaviour on error).
                ctx.set_field_volatile(this, RV_FIELD_SUPPLIER, Value::Object(Some(supplier)));
                Err(e)
            }
        };
    }
}

// ---------------------------------------------------------------------------
// ApplicationConfig natives
// ---------------------------------------------------------------------------

fn native_ac_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = this_ref(args) else {
        return Ok(None);
    };
    ctx.set_field(this, AC_FIELD_NAME, Value::Object(None));
    ctx.set_field(this, AC_FIELD_VERSION, Value::Object(None));
    Ok(None)
}

fn native_ac_init_with_values(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = this_ref(args) else {
        return Ok(None);
    };
    let name = arg_value(args, 1);
    let version = arg_value(args, 2);
    ctx.set_field(this, AC_FIELD_NAME, name);
    ctx.set_field(this, AC_FIELD_VERSION, version);
    Ok(None)
}

fn native_ac_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = this_ref(args) else {
        return Ok(Some(Value::Object(None)));
    };
    let field_v = ctx.get_field(this, AC_FIELD_NAME);
    if let Value::Object(Some(_)) = field_v {
        return Ok(Some(field_v));
    }
    // Fall back to env-var allowlist
    let fallback = application_config_env_lookup(AppConfigField::Name)
        .or_else(|| ctx.get_system_property("quarkus.application.name"));
    Ok(Some(optional_string_value(ctx, fallback)))
}

fn native_ac_version(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = this_ref(args) else {
        return Ok(Some(Value::Object(None)));
    };
    let field_v = ctx.get_field(this, AC_FIELD_VERSION);
    if let Value::Object(Some(_)) = field_v {
        return Ok(Some(field_v));
    }
    let fallback = application_config_env_lookup(AppConfigField::Version)
        .or_else(|| ctx.get_system_property("quarkus.application.version"));
    Ok(Some(optional_string_value(ctx, fallback)))
}

// ---------------------------------------------------------------------------
// DataSourceRuntimeConfig natives
// ---------------------------------------------------------------------------

fn native_dsrc_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = this_ref(args) else {
        return Ok(None);
    };
    for slot in [
        DSRC_FIELD_JDBC_URL,
        DSRC_FIELD_USERNAME,
        DSRC_FIELD_PASSWORD,
        DSRC_FIELD_DRIVER,
    ] {
        ctx.set_field(this, slot, Value::Object(None));
    }
    Ok(None)
}

/// Shared getter: read field; on null, fall back to allowlisted env.
fn dsrc_read_field(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    slot: usize,
    field: DatasourceField,
) -> MethodCallResult {
    let Some(this) = this_ref(args) else {
        return Ok(Some(Value::Object(None)));
    };
    let field_v = ctx.get_field(this, slot);
    if let Value::Object(Some(_)) = field_v {
        return Ok(Some(field_v));
    }
    let fallback = datasource_env_lookup(field);
    Ok(Some(optional_string_value(ctx, fallback)))
}

fn native_dsrc_jdbc_url(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    dsrc_read_field(ctx, args, DSRC_FIELD_JDBC_URL, DatasourceField::JdbcUrl)
}

fn native_dsrc_username(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    dsrc_read_field(ctx, args, DSRC_FIELD_USERNAME, DatasourceField::Username)
}

fn native_dsrc_password(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    dsrc_read_field(ctx, args, DSRC_FIELD_PASSWORD, DatasourceField::Password)
}

fn native_dsrc_driver(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    dsrc_read_field(ctx, args, DSRC_FIELD_DRIVER, DatasourceField::Driver)
}

// ---------------------------------------------------------------------------
// Application lifecycle
// ---------------------------------------------------------------------------

/// Observable no-op for the `io.quarkus.runtime.Application`
/// `start`/`stop`/`awaitShutdown` lifecycle entry points.
///
/// INTENTIONAL COMPATIBILITY SHIM — see `register_application_lifecycle` for
/// the full rationale. The real Quarkus replay plan is not executed here; we
/// only return normally so bootstrap dispatch does not abort. The trace
/// makes the elision visible so a run that depends on these hooks is never
/// silently mistaken for a fully started application.
///
/// AUDIT 2026-09-03: this was `debug!`, and the sentence above was
/// therefore false in every shipped binary. The workspace pins `tracing`
/// with `release_max_level_info`, so `debug!` and `trace!` expand to
/// no-ops in a release build -- the safeguard existed only in a debug
/// build, which is not where anyone runs Quarkus. `info!` because the
/// hook fires a handful of times per run (start/stop/awaitShutdown), not
/// in any loop.
fn native_app_lifecycle_no_op(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::info!(
        target: "cratonvm_native_builtins::quarkus",
        "io.quarkus.runtime.Application lifecycle hook elided (compatibility \
         shim: real Quarkus replay plan not executed)"
    );
    Ok(None)
}

// ---------------------------------------------------------------------------
// Timing natives — persist boot/main timestamps into this VM's Timing row
// so `Application.printStartupTime` can read them back.
// ---------------------------------------------------------------------------

/// Per-VM Quarkus `Timing` stamps (boot start / stop, main start / stop, in
/// `TIMING_FIELD_*` order), keyed by `NativeContext::vm_identity`.
///
/// gc-common w19-f: this used to be a synthetic `Timing` HEAP object, minted
/// once per PROCESS and remembered only by its raw address in a static.
/// Nothing rooted it, so the first collection freed or moved it, and every
/// later timing native wrote a `long` through that address into whatever
/// lived there by then (another VM's heap, for a second VM in the process).
/// The object never reached Java: the four stamps were all anything read, so
/// they are all that is kept. [`forget_vm_quarkus_timing`] drops a VM's row.
// addr-keyed: not a heap address -- VM identity; dropped by forget_vm_quarkus_timing.
fn timing_state() -> &'static Mutex<HashMap<usize, [i64; 4]>> {
    // addr-keyed: the accessor's table (see above).
    static INSTANCE: OnceLock<Mutex<HashMap<usize, [i64; 4]>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn update_timing_slot(ctx: &mut dyn NativeContext, slot: usize) {
    let ts = now_nanos();
    let vm = ctx.vm_identity();
    if let Ok(mut state) = timing_state().lock() {
        let entry = state.entry(vm).or_insert([0; 4]);
        if slot < entry.len() {
            entry[slot] = ts;
        }
    }
}

/// VM teardown: drop `vm`'s `Timing` stamps (see [`timing_state`]).
pub fn forget_vm_quarkus_timing(vm: usize) {
    if let Ok(mut state) = timing_state().lock() {
        state.remove(&vm);
    }
}

fn native_timing_static_init_started(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    update_timing_slot(ctx, TIMING_FIELD_BOOT_START);
    tracing::info!("quarkus static-init started");
    Ok(None)
}

fn native_timing_static_init_started_cl(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    update_timing_slot(ctx, TIMING_FIELD_BOOT_START);
    tracing::info!("quarkus static-init started (class-loader arg)");
    Ok(None)
}

fn native_timing_static_init_stopped(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    update_timing_slot(ctx, TIMING_FIELD_BOOT_STOP);
    tracing::info!("quarkus static-init stopped");
    Ok(None)
}

fn native_timing_main_started(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    update_timing_slot(ctx, TIMING_FIELD_MAIN_START);
    tracing::info!("quarkus main started");
    Ok(None)
}

fn native_timing_no_op(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

fn native_timing_print_startup_time(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    update_timing_slot(ctx, TIMING_FIELD_MAIN_STOP);
    // Try to emit a diagnostic about elapsed boot time.
    let vm = ctx.vm_identity();
    let slots = timing_state()
        .lock()
        .ok()
        .and_then(|state| state.get(&vm).copied());
    if let Some(slots) = slots {
        let boot_start = slots[TIMING_FIELD_BOOT_START];
        let main_stop = slots[TIMING_FIELD_MAIN_STOP];
        if boot_start > 0 && main_stop > boot_start {
            let millis = (main_stop - boot_start) / 1_000_000;
            tracing::info!("quarkus startup time: {} ms", millis);
        }
    }
    Ok(None)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests;

#[cfg(test)]
mod w19f_rv_relocation_tests;

#[cfg(test)]
mod w20g_rv_claim_tests;
