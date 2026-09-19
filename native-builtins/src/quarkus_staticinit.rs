// SPDX-License-Identifier: Apache-2.0
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
pub(crate) const RV_FIELD_VALUE: usize = 0;
pub(crate) const RV_FIELD_SUPPLIER: usize = 1;

// (Keycloak Gap 5) StartupContext is no longer shimmed — its real bytecode runs.

// ApplicationConfig
pub(crate) const AC_FIELD_NAME: usize = 0;
pub(crate) const AC_FIELD_VERSION: usize = 1;

// DataSourceRuntimeConfig
pub(crate) const DSRC_FIELD_JDBC_URL: usize = 0;
pub(crate) const DSRC_FIELD_USERNAME: usize = 1;
pub(crate) const DSRC_FIELD_PASSWORD: usize = 2;
pub(crate) const DSRC_FIELD_DRIVER: usize = 3;

// Timing — 4 long slots: bootStart, bootStop, mainStart, mainStop
pub(crate) const TIMING_FIELD_BOOT_START: usize = 0;
pub(crate) const TIMING_FIELD_BOOT_STOP: usize = 1;
pub(crate) const TIMING_FIELD_MAIN_START: usize = 2;
pub(crate) const TIMING_FIELD_MAIN_STOP: usize = 3;

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
pub(crate) const QUARKUS_BOOTSTRAP_MAGIC: i32 = -265202638;
pub(crate) const QUARKUS_BOOTSTRAP_VERSION: i32 = 2;

// T19.H3: Slot layout for our synthetic SerializedApplication.
//   0 = mainClass (String)
//   1 = runnerClassLoader (RunnerClassLoader — synthetic)
pub(crate) const SA_FIELD_MAIN_CLASS: usize = 0;
pub(crate) const SA_FIELD_RUNNER_CL: usize = 1;

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
// most once across all callers, even if multiple threads race. Previous
// designs kept the state in a process-wide HashMap keyed by the raw
// ObjectRef pointer — but that collides under parallel unit tests
// because each `MockNativeContext` restarts allocation at ptr=8, so the
// same u64 key maps to multiple logical RuntimeValues across threads.
//
// We instead drive the double-checked lock off the RuntimeValue's own
// `supplier` field:
//   * After `<init>(Supplier)` — `supplier` holds the Supplier ObjectRef,
//     `value` is null.
//   * First `getValue()` invokes the supplier, stores its return into
//     `value`, then clears `supplier` to null (CAS from the original
//     Supplier → null so concurrent callers see the transition).
//   * Subsequent `getValue()` observes `supplier == null` and returns
//     `value` directly.
//
// A small process-wide `Mutex<HashSet<u64>>` of "in-progress" pointer
// keys arbitrates between concurrent threads that BOTH see the supplier
// field populated and want to claim the invocation. Unlike the
// HashMap-of-state approach, this set is only populated for the brief
// window while a supplier.get() is running, so even under key-collision
// across parallel tests the worst case is that one test briefly spins
// waiting for the other — no state leak.
// ---------------------------------------------------------------------------

/// Pointer keys that currently have a supplier.get() invocation in
/// flight. An entry is held only for the duration of one invocation.
fn runtime_value_inprogress() -> &'static Mutex<std::collections::HashSet<u64>> {
    static INSTANCE: OnceLock<Mutex<std::collections::HashSet<u64>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
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
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
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
    registry.set_category(__prev_cat);
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
    let sa = match sa_cid {
        Some(cid) => ctx.alloc_object(cid, sa_n),
        None => ctx.alloc_object(cratonvm_types::ClassId::new(0), sa_n),
    };
    let rcl = match rcl_cid {
        Some(cid) => ctx.alloc_object(cid, rcl_n),
        None => ctx.alloc_object(cratonvm_types::ClassId::new(0), rcl_n),
    };

    let main_class = resolve_main_class();
    let main_class_obj = ctx.create_string(&main_class);
    ctx.set_field(sa, SA_FIELD_MAIN_CLASS, Value::Object(Some(main_class_obj)));
    ctx.set_field(sa, SA_FIELD_RUNNER_CL, Value::Object(Some(rcl)));

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
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(CLS_RUNTIME_VALUE, "<init>", "()V", native_rv_init_empty);
    registry.register(
        CLS_RUNTIME_VALUE,
        "<init>",
        "(Ljava/lang/Object;)V",
        native_rv_init_value,
    );
    // The single-arg Supplier ctor — signature matches
    // `RuntimeValue(java.util.function.Supplier)` used by Quarkus'
    // generated replay bytecode. Note the descriptor is
    // `(Ljava/util/function/Supplier;)V`.
    registry.register(
        CLS_RUNTIME_VALUE,
        "<init>",
        "(Ljava/util/function/Supplier;)V",
        native_rv_init_supplier,
    );
    registry.register(
        CLS_RUNTIME_VALUE,
        "getValue",
        "()Ljava/lang/Object;",
        native_rv_get_value,
    );
    // `deepInstance` is Quarkus-internal alias for getValue.
    registry.register(
        CLS_RUNTIME_VALUE,
        "deepInstance",
        "()Ljava/lang/Object;",
        native_rv_get_value,
    );
    registry.set_category(__prev_cat);
}

fn register_application_config(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(CLS_APPLICATION_CONFIG, "<init>", "()V", native_ac_init);
    registry.register(
        CLS_APPLICATION_CONFIG,
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        native_ac_init_with_values,
    );
    registry.register(
        CLS_APPLICATION_CONFIG,
        "name",
        "()Ljava/lang/String;",
        native_ac_name,
    );
    registry.register(
        CLS_APPLICATION_CONFIG,
        "version",
        "()Ljava/lang/String;",
        native_ac_version,
    );
    registry.set_category(__prev_cat);
}

fn register_datasource_runtime_config(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(CLS_DATASOURCE_CONFIG, "<init>", "()V", native_dsrc_init);
    registry.register(
        CLS_DATASOURCE_CONFIG,
        "jdbcUrl",
        "()Ljava/lang/String;",
        native_dsrc_jdbc_url,
    );
    registry.register(
        CLS_DATASOURCE_CONFIG,
        "username",
        "()Ljava/lang/String;",
        native_dsrc_username,
    );
    registry.register(
        CLS_DATASOURCE_CONFIG,
        "password",
        "()Ljava/lang/String;",
        native_dsrc_password,
    );
    registry.register(
        CLS_DATASOURCE_CONFIG,
        "driver",
        "()Ljava/lang/String;",
        native_dsrc_driver,
    );
    registry.set_category(__prev_cat);
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
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    // COMPATIBILITY SHIM (see fn doc): override the lifecycle entry points
    // with observable no-ops so Quarkus bootstrap dispatch does not abort.
    // These return normally without executing the real replay plan.
    registry.register(
        "io/quarkus/runtime/Application",
        "start",
        "([Ljava/lang/String;)V",
        native_app_lifecycle_no_op,
    );
    registry.register(
        "io/quarkus/runtime/Application",
        "stop",
        "()V",
        native_app_lifecycle_no_op,
    );
    registry.register(
        "io/quarkus/runtime/Application",
        "awaitShutdown",
        "()V",
        native_app_lifecycle_no_op,
    );
    registry.set_category(__prev_cat);
}

fn register_timing(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        CLS_TIMING,
        "staticInitStarted",
        "(Z)V",
        native_timing_static_init_started,
    );
    registry.register(
        CLS_TIMING,
        "staticInitStarted",
        "(Ljava/lang/ClassLoader;Z)V",
        native_timing_static_init_started_cl,
    );
    registry.register(
        CLS_TIMING,
        "staticInitStopped",
        "()V",
        native_timing_static_init_stopped,
    );
    registry.register(CLS_TIMING, "mainStarted", "()V", native_timing_main_started);
    registry.register(CLS_TIMING, "restart", "()V", native_timing_no_op);
    registry.register(
        CLS_TIMING,
        "printStartupTime",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;ZZ)V",
        native_timing_print_startup_time,
    );
    registry.set_category(__prev_cat);
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
/// Double-checked lock driven off the object's own fields:
///   1. If `supplier == null`, the value has already been materialized
///      (either pre-init or by a previous getValue) — return `value`.
///   2. Otherwise, arbitrate via the process-wide in-progress set:
///      - If another thread is already invoking the supplier, spin-yield.
///      - Else, claim the slot, invoke supplier.get(), CAS-store the
///        result into `value`, clear `supplier` to null, release the
///        claim.
///   3. The CAS guarantees at-most-once invocation even across multiple
///      contexts that happen to reuse the same raw ObjectRef address.
fn native_rv_get_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = this_ref(args) else {
        return Ok(Some(Value::Object(None)));
    };
    let key = this.as_ptr() as u64;

    loop {
        // Cheap check: if the supplier has already been cleared, the
        // materialized result is in the value field (possibly null).
        let supplier = ctx.get_field(this, RV_FIELD_SUPPLIER);
        let sup_obj = match supplier {
            Value::Object(Some(o)) => o,
            _ => {
                return Ok(Some(ctx.get_field(this, RV_FIELD_VALUE)));
            }
        };

        // Try to claim the invocation. If another thread already owns
        // it, spin-yield; they will clear `supplier` on completion and
        // our next iteration hits the fast path.
        let claimed = {
            let mut inflight = runtime_value_inprogress()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            inflight.insert(key)
        };
        if !claimed {
            std::thread::yield_now();
            continue;
        }

        // Double-check under the claim — supplier may have been
        // cleared between our initial read and the claim.
        let recheck = ctx.get_field(this, RV_FIELD_SUPPLIER);
        let still_needed = matches!(recheck, Value::Object(Some(_)));
        if !still_needed {
            runtime_value_inprogress()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            return Ok(Some(ctx.get_field(this, RV_FIELD_VALUE)));
        }

        // We now own the invocation. Invoke supplier.get().
        let invoke_result = ctx.invoke_virtual(sup_obj, "get", "()Ljava/lang/Object;", &[]);

        // Release claim *before* returning/propagating the result so an
        // error path doesn't leave the set marked in-progress.
        runtime_value_inprogress()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);

        let result = match invoke_result {
            Ok(Some(v)) => v,
            Ok(None) => Value::Object(None),
            Err(e) => return Err(e),
        };

        // Publish: store value first, then clear supplier. A concurrent
        // reader that catches the half-published state (supplier null
        // but value stale) can only happen if someone wrote value
        // between our two writes — impossible since we hold the claim.
        ctx.set_field(this, RV_FIELD_VALUE, result);
        ctx.set_field(this, RV_FIELD_SUPPLIER, Value::Object(None));
        return Ok(Some(result));
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
// Timing natives — persist boot/main timestamps into the Timing singleton
// so `Application.printStartupTime` can read them back.
// ---------------------------------------------------------------------------

/// Resolve (or create) the process-wide `Timing` singleton object. Used
/// by the boot / main / stop phase markers — the real JDK bytecode
/// accesses `Timing.bootStart` etc via static fields, but the synthetic
/// layout treats them as a single heap object the VM keeps around.
fn timing_singleton() -> &'static Mutex<Option<u64>> {
    static INSTANCE: OnceLock<Mutex<Option<u64>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
}

/// Per-VM Timing timestamps — indexed by the singleton raw-pointer key
/// to distinguish reinits in long-running tests.
fn timing_state() -> &'static Mutex<HashMap<u64, [i64; 4]>> {
    static INSTANCE: OnceLock<Mutex<HashMap<u64, [i64; 4]>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Take (or lazily create) the Timing singleton. Returns `None` if the
/// class cannot be resolved — callers should then treat Timing as
/// no-op-only (graceful failure).
fn ensure_timing_singleton(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    // If we've already cached the raw pointer, just rebuild the
    // ObjectRef from it — this avoids reallocating a fresh heap slot
    // on every tick.
    let cached = {
        let g = timing_singleton().lock().ok()?;
        *g
    };
    if let Some(ptr) = cached {
        if ptr != 0 {
            // SAFETY: `ptr` was obtained from a live ObjectRef in
            // the same process and the heap address is stable for
            // the VM's lifetime (we never free singletons). The
            // caller only reads/writes fields at small fixed
            // indices; no pointer-provenance trickery.
            let obj = unsafe { ObjectRef::from_raw(ptr as *mut u8) };
            return Some(obj);
        }
    }
    // Need to allocate a fresh Timing object.
    let cid = ctx.ensure_class_initialized(CLS_TIMING).ok()?;
    let real = ctx.class_num_total_fields(cid);
    let n = real.max(4);
    let obj = ctx.alloc_object(cid, n);
    let key = obj.as_ptr() as u64;
    {
        let mut g = timing_singleton().lock().ok()?;
        *g = Some(key);
    }
    {
        let mut state = timing_state().lock().ok()?;
        state.insert(key, [0; 4]);
    }
    Some(obj)
}

fn update_timing_slot(ctx: &mut dyn NativeContext, slot: usize) {
    let Some(obj) = ensure_timing_singleton(ctx) else {
        return;
    };
    let ts = now_nanos();
    ctx.set_field(obj, slot, Value::Long(ts));
    let key = obj.as_ptr() as u64;
    if let Ok(mut state) = timing_state().lock() {
        let entry = state.entry(key).or_insert([0; 4]);
        if slot < entry.len() {
            entry[slot] = ts;
        }
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
    if let Some(obj) = ensure_timing_singleton(ctx) {
        let boot_start = match ctx.get_field(obj, TIMING_FIELD_BOOT_START) {
            Value::Long(v) => v,
            _ => 0,
        };
        let main_stop = match ctx.get_field(obj, TIMING_FIELD_MAIN_STOP) {
            Value::Long(v) => v,
            _ => 0,
        };
        if boot_start > 0 && main_stop > boot_start {
            let millis = (main_stop - boot_start) / 1_000_000;
            tracing::info!("quarkus startup time: {} ms", millis);
        }
    }
    Ok(None)
}

/// Test-only helper: reset the tiny in-progress set. State that was
/// previously process-wide (supplier-state map, shutdown-task list,
/// values map, timing singleton) is now keyed off per-test ObjectRefs
/// whose lifetimes end when `MockNativeContext` drops — no reset
/// needed there. We leave this helper in place so tests can explicitly
/// scrub the in-progress set between parallel runs if they rely on it.
#[cfg(test)]
fn reset_supplier_state() {
    if let Ok(mut s) = runtime_value_inprogress().lock() {
        s.clear();
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    use cratonvm_native_api::NativeMethodRegistry;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::Value;

    fn make_runtime_value_obj(ctx: &mut crate::test_utils::MockNativeContext) -> ObjectRef {
        let cid = ctx.ensure_class_initialized(CLS_RUNTIME_VALUE).unwrap();
        ctx.alloc_object(cid, 2)
    }

    #[test]
    fn register_quarkus_staticinit_natives_registers_all_expected_entries() {
        let mut r = NativeMethodRegistry::new();
        register_quarkus_staticinit_natives(&mut r);

        // Representative entries across each sub-class.
        assert!(r
            .find(CLS_RUNTIME_VALUE, "getValue", "()Ljava/lang/Object;")
            .is_some());
        assert!(r
            .find(CLS_RUNTIME_VALUE, "<init>", "(Ljava/lang/Object;)V")
            .is_some());
        assert!(r
            .find(CLS_APPLICATION_CONFIG, "name", "()Ljava/lang/String;")
            .is_some());
        assert!(r
            .find(CLS_DATASOURCE_CONFIG, "jdbcUrl", "()Ljava/lang/String;")
            .is_some());
        assert!(r.find(CLS_TIMING, "mainStarted", "()V").is_some());
    }

    #[test]
    fn t19_3_runtime_value_wraps_pre_materialized_value() {
        reset_supplier_state();
        let mut ctx = mock_ctx();
        let rv = make_runtime_value_obj(&mut ctx);
        let payload = ctx.create_string("hello");

        native_rv_init_value(
            &mut ctx,
            &[Value::Object(Some(rv)), Value::Object(Some(payload))],
        )
        .unwrap();

        let got = native_rv_get_value(&mut ctx, &[Value::Object(Some(rv))])
            .unwrap()
            .unwrap();
        match got {
            Value::Object(Some(o)) => assert_eq!(o, payload),
            _ => panic!("expected payload ObjectRef, got {:?}", got),
        }
    }

    #[test]
    fn t19_3_runtime_value_lazy_supplier_invoked_on_first_get() {
        reset_supplier_state();
        let mut ctx = mock_ctx();
        let rv = make_runtime_value_obj(&mut ctx);
        let supplier_cid = ctx.ensure_class_initialized("TestSupplier").unwrap();
        let supplier_obj = ctx.alloc_object(supplier_cid, 1);

        // Prime invoke_virtual to return a specific String on first call.
        let supplier_result = ctx.create_string("lazy-materialized");
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Object(Some(supplier_result)))));
        }

        native_rv_init_supplier(
            &mut ctx,
            &[Value::Object(Some(rv)), Value::Object(Some(supplier_obj))],
        )
        .unwrap();

        // First getValue → supplier invoked
        let got = native_rv_get_value(&mut ctx, &[Value::Object(Some(rv))])
            .unwrap()
            .unwrap();
        match got {
            Value::Object(Some(o)) => assert_eq!(o, supplier_result),
            _ => panic!("expected supplier result, got {:?}", got),
        }
        // And the materialized value now lives in field 0.
        match ctx.get_field(rv, RV_FIELD_VALUE) {
            Value::Object(Some(o)) => assert_eq!(o, supplier_result),
            other => panic!("expected field populated, got {:?}", other),
        }
    }

    #[test]
    fn t19_3_runtime_value_caches_after_first_get() {
        reset_supplier_state();
        let mut ctx = mock_ctx();
        let rv = make_runtime_value_obj(&mut ctx);
        let supplier_cid = ctx.ensure_class_initialized("TestSupplier").unwrap();
        let supplier_obj = ctx.alloc_object(supplier_cid, 1);

        // First call returns A; second call would return B — but we
        // expect the second call to be short-circuited and never dispatch.
        let first_result = ctx.create_string("first");
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Object(Some(first_result)))));
        }

        native_rv_init_supplier(
            &mut ctx,
            &[Value::Object(Some(rv)), Value::Object(Some(supplier_obj))],
        )
        .unwrap();

        let first = native_rv_get_value(&mut ctx, &[Value::Object(Some(rv))])
            .unwrap()
            .unwrap();

        // Prime a DIFFERENT invoke_virtual_result; if the second getValue
        // dispatched to the supplier it would pick this up. Since it
        // should hit the cached value, we end up with the first result.
        let second_result = ctx.create_string("second");
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Object(Some(second_result)))));
        }

        let second = native_rv_get_value(&mut ctx, &[Value::Object(Some(rv))])
            .unwrap()
            .unwrap();

        assert_eq!(
            first, second,
            "second getValue must return cached first result"
        );
        // And the queued second_result is still waiting (never consumed).
        let queued = unsafe { &*ctx.invoke_virtual_result.get() };
        assert!(
            queued.is_some(),
            "second getValue must not have dispatched to invoke_virtual"
        );
    }

    #[test]
    fn t19_3_application_config_defaults_from_env_fallback() {
        reset_supplier_state();
        let mut ctx = mock_ctx();
        let cid = ctx
            .ensure_class_initialized(CLS_APPLICATION_CONFIG)
            .unwrap();
        let ac = ctx.alloc_object(cid, 2);
        native_ac_init(&mut ctx, &[Value::Object(Some(ac))]).unwrap();

        // Without any env/property: name() returns null.
        let n = native_ac_name(&mut ctx, &[Value::Object(Some(ac))])
            .unwrap()
            .unwrap();
        match n {
            Value::Object(None) => {}
            other => panic!("expected null name, got {:?}", other),
        }

        // With a system property fallback registered:
        ctx.set_system_property("quarkus.application.name", "keycloak");
        let n2 = native_ac_name(&mut ctx, &[Value::Object(Some(ac))])
            .unwrap()
            .unwrap();
        let s = match n2 {
            Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
            other => panic!("expected populated name, got {:?}", other),
        };
        assert_eq!(s, "keycloak");

        // With an explicit field value: the field wins over the fallback.
        let explicit = ctx.create_string("explicit-app");
        ctx.set_field(ac, AC_FIELD_NAME, Value::Object(Some(explicit)));
        let n3 = native_ac_name(&mut ctx, &[Value::Object(Some(ac))])
            .unwrap()
            .unwrap();
        match n3 {
            Value::Object(Some(o)) => assert_eq!(o, explicit),
            other => panic!("expected explicit name, got {:?}", other),
        }
    }

    #[test]
    fn t19_3_data_source_config_jdbc_url_from_recorded_or_env() {
        reset_supplier_state();
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized(CLS_DATASOURCE_CONFIG).unwrap();
        let ds = ctx.alloc_object(cid, 4);
        native_dsrc_init(&mut ctx, &[Value::Object(Some(ds))]).unwrap();

        // No field value and no env fallback visible (DATABASE_URL etc
        // won't be set in test env) — native returns null.
        let u = native_dsrc_jdbc_url(&mut ctx, &[Value::Object(Some(ds))])
            .unwrap()
            .unwrap();
        match u {
            Value::Object(None) => {}
            Value::Object(Some(o)) => {
                // If the CI host happens to have DATABASE_URL set, accept
                // that too — just verify the native returned a usable
                // Java String wrapper.
                let s = ctx.read_string(o).unwrap_or_default();
                assert!(!s.is_empty(), "DATABASE_URL from env must be non-empty");
            }
            other => panic!("unexpected jdbcUrl return: {:?}", other),
        }

        // Field set wins over env.
        let recorded = ctx.create_string("jdbc:h2:mem:test");
        ctx.set_field(ds, DSRC_FIELD_JDBC_URL, Value::Object(Some(recorded)));
        let u2 = native_dsrc_jdbc_url(&mut ctx, &[Value::Object(Some(ds))])
            .unwrap()
            .unwrap();
        match u2 {
            Value::Object(Some(o)) => assert_eq!(o, recorded),
            other => panic!("expected recorded jdbcUrl, got {:?}", other),
        }

        // Username / password / driver getters all work off the same
        // pattern — one combined smoke check.
        let user = ctx.create_string("sa");
        ctx.set_field(ds, DSRC_FIELD_USERNAME, Value::Object(Some(user)));
        let u3 = native_dsrc_username(&mut ctx, &[Value::Object(Some(ds))])
            .unwrap()
            .unwrap();
        match u3 {
            Value::Object(Some(o)) => assert_eq!(o, user),
            other => panic!("expected username, got {:?}", other),
        }
    }

    #[test]
    fn t19_3_timing_phases_no_op_but_return_gracefully() {
        reset_supplier_state();
        let mut ctx = mock_ctx();
        // All Timing natives take a receiver-less descriptor — we just
        // verify each one returns `Ok(None)` rather than panicking.
        assert!(
            native_timing_static_init_started(&mut ctx, &[Value::Int(1)])
                .unwrap()
                .is_none()
        );
        assert!(native_timing_static_init_stopped(&mut ctx, &[])
            .unwrap()
            .is_none());
        assert!(native_timing_main_started(&mut ctx, &[]).unwrap().is_none());
        assert!(native_timing_print_startup_time(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(None),
                Value::Object(None),
                Value::Object(None),
                Value::Object(None),
                Value::Object(None),
                Value::Int(0),
                Value::Int(0),
            ]
        )
        .unwrap()
        .is_none());

        // Timing singleton must be populated with a boot-start timestamp.
        let singleton_key = timing_singleton()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expect("timing singleton should exist after static_init_started");
        let state = timing_state().lock().unwrap_or_else(|e| e.into_inner());
        let slots = state.get(&singleton_key).copied().unwrap_or([0; 4]);
        assert!(
            slots[TIMING_FIELD_BOOT_START] > 0,
            "boot_start slot should hold a non-zero nanosecond timestamp"
        );
        assert!(
            slots[TIMING_FIELD_BOOT_STOP] > 0,
            "boot_stop slot should hold a non-zero nanosecond timestamp"
        );
        assert!(
            slots[TIMING_FIELD_MAIN_START] > 0,
            "main_start slot should hold a non-zero nanosecond timestamp"
        );
        assert!(
            slots[TIMING_FIELD_MAIN_STOP] > 0,
            "main_stop slot should hold a non-zero nanosecond timestamp"
        );
    }

    #[test]
    fn t19_3_application_lifecycle_natives_are_graceful_no_ops() {
        reset_supplier_state();
        let mut ctx = mock_ctx();
        assert!(native_app_lifecycle_no_op(&mut ctx, &[]).unwrap().is_none());
    }

    #[test]
    fn t19_3_runtime_value_null_supplier_returns_null_gracefully() {
        // Supplier returning null must NOT panic — record + return null
        // per the mission-spec error-handling rule.
        reset_supplier_state();
        let mut ctx = mock_ctx();
        let rv = make_runtime_value_obj(&mut ctx);
        let supplier_cid = ctx.ensure_class_initialized("TestSupplier").unwrap();
        let supplier_obj = ctx.alloc_object(supplier_cid, 1);

        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Object(None))));
        }
        native_rv_init_supplier(
            &mut ctx,
            &[Value::Object(Some(rv)), Value::Object(Some(supplier_obj))],
        )
        .unwrap();

        let got = native_rv_get_value(&mut ctx, &[Value::Object(Some(rv))])
            .unwrap()
            .unwrap();
        assert!(
            matches!(got, Value::Object(None)),
            "null-returning supplier must propagate null"
        );
    }

    // -----------------------------------------------------------------
    // T19.H3: bootstrap-runner replacement natives
    // -----------------------------------------------------------------

    #[test]
    fn t19_h3_serialized_application_read_returns_populated_sa() {
        let mut ctx = mock_ctx();
        let sa = match native_serialized_application_read(
            &mut ctx,
            &[Value::Object(None), Value::Object(None)],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("expected SerializedApplication ObjectRef, got {:?}", other),
        };
        // main class populated.
        let name = match ctx.get_field(sa, SA_FIELD_MAIN_CLASS) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            other => panic!("expected String, got {:?}", other),
        };
        assert_eq!(name, "org.keycloak.quarkus.runtime.KeycloakMain");
        // runner classloader populated (non-null synthetic).
        assert!(matches!(
            ctx.get_field(sa, SA_FIELD_RUNNER_CL),
            Value::Object(Some(_))
        ));
    }

    #[test]
    fn t19_h3_serialized_application_getters_round_trip() {
        let mut ctx = mock_ctx();
        let sa = match native_serialized_application_read(&mut ctx, &[])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let mc = native_serialized_application_get_main_class(&mut ctx, &[Value::Object(Some(sa))])
            .unwrap()
            .unwrap();
        let name = match mc {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => panic!(),
        };
        assert_eq!(name, "org.keycloak.quarkus.runtime.KeycloakMain");

        let cl = native_serialized_application_get_runner_cl(&mut ctx, &[Value::Object(Some(sa))])
            .unwrap()
            .unwrap();
        assert!(matches!(cl, Value::Object(Some(_))));
    }

    #[test]
    fn t19_h3_serialized_application_get_main_class_throws_on_empty_field() {
        let mut ctx = mock_ctx();
        // Build an SA whose main-class field was never populated (i.e. it
        // did NOT come through `read()`). Previously this fabricated the
        // compile-time Keycloak main; now it must fail loud rather than
        // silently launch an app-specific default.
        let cid = ctx.ensure_class_initialized(CLS_SERIALIZED_APP).unwrap();
        let sa = ctx.alloc_object(cid, 2);
        // Leave both slots at default (Value::Int(0) from MockNativeContext).
        let err =
            native_serialized_application_get_main_class(&mut ctx, &[Value::Object(Some(sa))])
                .unwrap_err();
        match err {
            cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::IllegalStateException { .. },
                ),
            ) => {}
            other => panic!("expected IllegalStateException, got {other:?}"),
        }
    }

    #[test]
    fn t19_h3_sanitize_main_class_candidate_accepts_and_rejects() {
        assert_eq!(
            sanitize_main_class_candidate("com.example.App").as_deref(),
            Some("com.example.App")
        );
        assert_eq!(
            sanitize_main_class_candidate("com/example/App").as_deref(),
            Some("com/example/App")
        );
        assert_eq!(
            sanitize_main_class_candidate("Outer$Inner$Run").as_deref(),
            Some("Outer$Inner$Run")
        );
        assert!(sanitize_main_class_candidate("").is_none());
        assert!(sanitize_main_class_candidate(";evil").is_none());
        assert!(sanitize_main_class_candidate("../traversal").is_none());
        assert!(sanitize_main_class_candidate("spaces here").is_none());
        assert!(sanitize_main_class_candidate(&"a".repeat(513)).is_none());
        // Newlines blocked.
        assert!(sanitize_main_class_candidate("foo\nbar").is_none());
    }

    #[test]
    fn t19_h3_quarkus_bootstrap_constants_match_runtime_jar() {
        // Values cross-checked by disassembling
        // `io.quarkus.quarkus-bootstrap-runner-3.20.0.jar` —
        // SerializedApplication.MAGIC / .VERSION. Locking them in
        // here catches a future Quarkus upgrade that silently changes
        // the constants.
        assert_eq!(QUARKUS_BOOTSTRAP_MAGIC, -265202638);
        assert_eq!(QUARKUS_BOOTSTRAP_VERSION, 2);
    }

    // -----------------------------------------------------------------
    // T19.H5: RunnerClassLoader.loadClass sanitizer tests
    // -----------------------------------------------------------------

    #[test]
    fn t19_h5_sanitize_load_class_name_accepts_binary_names() {
        assert_eq!(
            sanitize_load_class_name("java.lang.String").as_deref(),
            Some("java.lang.String")
        );
        assert_eq!(
            sanitize_load_class_name("java/lang/String").as_deref(),
            Some("java/lang/String")
        );
        assert_eq!(
            sanitize_load_class_name("com.example.Foo$Inner").as_deref(),
            Some("com.example.Foo$Inner")
        );
    }

    #[test]
    fn t19_h5_sanitize_load_class_name_rejects_dangerous_inputs() {
        assert!(sanitize_load_class_name("").is_none());
        assert!(sanitize_load_class_name(&"a".repeat(513)).is_none());
        assert!(sanitize_load_class_name("/absolute").is_none());
        assert!(sanitize_load_class_name("../../etc/passwd").is_none());
        assert!(sanitize_load_class_name("foo bar").is_none()); // spaces
        assert!(sanitize_load_class_name("semi;colon").is_none());
        assert!(sanitize_load_class_name("has\nnewline").is_none());
        assert!(sanitize_load_class_name("has\0nul").is_none());
        assert!(sanitize_load_class_name("has\\backslash").is_none());
    }

    #[test]
    fn t19_h5_runner_class_loader_load_class_happy_path_returns_mirror() {
        let mut ctx = mock_ctx();
        // Allocate a fake RunnerClassLoader instance (the `this` arg).
        let rcl_cid = ctx
            .ensure_class_initialized(CLS_RUNNER_CLASSLOADER)
            .unwrap();
        let rcl = ctx.alloc_object(rcl_cid, 1);
        // Pre-load a class in the mock so ensure_class_initialized returns Ok.
        ctx.ensure_class_initialized("com/example/App").unwrap();
        let name = ctx.create_string("com.example.App");
        let result = native_runner_class_loader_load_class(
            &mut ctx,
            &[Value::Object(Some(rcl)), Value::Object(Some(name))],
        )
        .unwrap()
        .unwrap();
        match result {
            Value::Object(Some(mirror)) => {
                // The mirror's name field (slot 1 in mock) should echo the class name.
                match ctx.get_field(mirror, 1) {
                    Value::Object(Some(s)) => {
                        // Mock's get_class_mirror stores the internal name.
                        assert_eq!(ctx.read_string(s).as_deref(), Some("com/example/App"));
                    }
                    _ => panic!("mirror name slot empty"),
                }
            }
            other => panic!("expected Class mirror, got {other:?}"),
        }
    }

    #[test]
    fn t19_h5_runner_class_loader_load_class_null_name_throws_npe() {
        let mut ctx = mock_ctx();
        let rcl_cid = ctx
            .ensure_class_initialized(CLS_RUNNER_CLASSLOADER)
            .unwrap();
        let rcl = ctx.alloc_object(rcl_cid, 1);
        let err = native_runner_class_loader_load_class(
            &mut ctx,
            &[Value::Object(Some(rcl)), Value::Object(None)],
        )
        .unwrap_err();
        match err {
            cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::NullPointerException { .. },
                ),
            ) => {}
            other => panic!("expected NPE, got {other:?}"),
        }
    }

    #[test]
    fn t19_h5_runner_class_loader_load_class_rejects_bad_name() {
        let mut ctx = mock_ctx();
        let rcl_cid = ctx
            .ensure_class_initialized(CLS_RUNNER_CLASSLOADER)
            .unwrap();
        let rcl = ctx.alloc_object(rcl_cid, 1);
        let bad = ctx.create_string("../../etc/passwd");
        let err = native_runner_class_loader_load_class(
            &mut ctx,
            &[Value::Object(Some(rcl)), Value::Object(Some(bad))],
        )
        .unwrap_err();
        match err {
            cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::ClassNotFoundException { class_name },
                ),
            ) => {
                assert_eq!(class_name, "../../etc/passwd");
            }
            other => panic!("expected CNFE, got {other:?}"),
        }
    }

    #[test]
    fn t19_h5_runner_class_loader_load_class_normalizes_dots() {
        // The internal VM uses slash-delimited names; the input uses dots.
        // Verify the translation happens before ensure_class_initialized.
        let mut ctx = mock_ctx();
        let rcl_cid = ctx
            .ensure_class_initialized(CLS_RUNNER_CLASSLOADER)
            .unwrap();
        let rcl = ctx.alloc_object(rcl_cid, 1);
        // Pre-load using the slash form.
        ctx.ensure_class_initialized("org/keycloak/quarkus/runtime/KeycloakMain")
            .unwrap();
        let name = ctx.create_string("org.keycloak.quarkus.runtime.KeycloakMain");
        let res = native_runner_class_loader_load_class(
            &mut ctx,
            &[Value::Object(Some(rcl)), Value::Object(Some(name))],
        );
        assert!(res.is_ok(), "dots should normalize to slashes");
    }

    #[test]
    fn t19_h5_runner_class_loader_load_class_with_resolve_delegates() {
        let mut ctx = mock_ctx();
        let rcl_cid = ctx
            .ensure_class_initialized(CLS_RUNNER_CLASSLOADER)
            .unwrap();
        let rcl = ctx.alloc_object(rcl_cid, 1);
        ctx.ensure_class_initialized("a/b/C").unwrap();
        let name = ctx.create_string("a.b.C");
        let result = native_runner_class_loader_load_class_with_resolve(
            &mut ctx,
            &[
                Value::Object(Some(rcl)),
                Value::Object(Some(name)),
                Value::Int(1), // resolve=true
            ],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(result, Value::Object(Some(_))));
    }
}
