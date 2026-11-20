// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! WP2.4-C — `-javaagent:` JAR loader and `Premain-Class` dispatcher.
//!
//! When the user passes `-javaagent:foo.jar=opts` on the command line, the
//! JVM is required to:
//!
//!   1. Open the JAR and parse `META-INF/MANIFEST.MF` for the
//!      `Premain-Class`, `Boot-Class-Path`, and `Can-*` capability flags.
//!   2. Append any `Boot-Class-Path` entries to the bootstrap class search
//!      (resolved relative to the JAR's parent directory).
//!   3. Append the JAR itself to the system classpath so the agent's
//!      classes are loadable.
//!   4. Resolve and call `Premain-Class.premain(String, Instrumentation)`
//!      *before* invoking the application's `main(String[])`, falling
//!      back to `premain(String)` if the two-arg form is unavailable.
//!   5. Fail closed if the agent cannot be loaded, its `premain` cannot be
//!      resolved, or `premain` throws. HotSpot aborts startup when a requested
//!      startup instrumentation agent fails.
//!
//! This module is the **JAR-side** loader: it does not own the
//! `Instrumentation` Rust state (Owner A, see `runtime/instrument.rs`),
//! it only constructs the Java mirror via
//! `sun.instrument.InstrumentationImpl` and threads it into `premain`.
//!
//! The native `-agentlib:`/`-agentpath:` JVMTI agent loader lives in
//! `vm/src/jvmti/agent.rs` and is a separate concern: that loader is
//! about loading dynamic libraries (`.so`/`.dll`) and calling C
//! `Agent_OnLoad` symbols, while this loader is about loading Java
//! agents from JAR manifests and calling Java `premain` methods.
//!
//! # Public API
//!
//! ```ignore
//! use cratonvm_vm::runtime::agent_loader::{parse_javaagent_spec, invoke_premains};
//!
//! let agent = parse_javaagent_spec("-javaagent:my-agent.jar=opt1,opt2")?;
//! invoke_premains(&vm.shared, &mut vm.main_thread, &[agent])?;
//! ```

use std::fmt;
use std::path::{Path, PathBuf};

use crate::classloading::ManifestInfo;
use crate::error::MethodCallFailed;
use crate::threading::JvmThread;
use crate::types::Value;
use crate::vm::SharedVm;

// ---------------------------------------------------------------------------
// LoadedAgent — parsed metadata about a single -javaagent: entry.
// ---------------------------------------------------------------------------

/// Parsed metadata for a single `-javaagent:` JAR ready for dispatch.
///
/// The fields are populated from the JAR's `META-INF/MANIFEST.MF`. Once
/// constructed, [`invoke_premains`] consumes a `&[LoadedAgent]` and runs
/// the dispatch sequence in declaration order.
#[derive(Debug, Clone)]
pub struct LoadedAgent {
    /// Absolute or relative path to the agent JAR.
    pub jar_path: PathBuf,
    /// The text after the optional `=` in `-javaagent:foo.jar=opts`.
    /// Passed to `premain(String, Instrumentation)` as its first arg.
    pub agent_args: Option<String>,
    /// `Premain-Class` manifest attribute — the FQCN whose `premain`
    /// method we will invoke. Stored in `'.'`-separated form (the form
    /// the manifest uses); we translate to `'/'` slashes when calling
    /// the VM's class loader.
    pub premain_class: String,
    /// `Agent-Class` attribute — the FQCN for runtime attach. Captured
    /// here for symmetry / tooling completeness; runtime Attach is out
    /// of scope for this WP.
    pub agent_class: Option<String>,
    /// `Can-Redefine-Classes: true|false` capability flag.
    pub can_redefine: bool,
    /// `Can-Retransform-Classes: true|false` capability flag.
    pub can_retransform: bool,
    /// `Can-Set-Native-Method-Prefix: true|false` capability flag.
    pub can_set_native_method_prefix: bool,
    /// Each entry of the `Boot-Class-Path` attribute resolved against
    /// the JAR's parent directory.
    pub boot_class_path: Vec<PathBuf>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure modes for agent loading. CLI / VM startup must treat every variant
/// as fatal for a requested startup `-javaagent`.
#[derive(Debug)]
pub enum AgentLoadError {
    /// The `-javaagent:` argument did not start with the expected prefix.
    BadPrefix(String),
    /// The JAR file does not exist or could not be opened / read.
    JarNotFound { path: PathBuf, source: String },
    /// The JAR has no `META-INF/MANIFEST.MF`, or the manifest does not
    /// contain a `Premain-Class` attribute.
    MissingPremainClass { jar: PathBuf },
    /// `Premain-Class` resolved to a class that has no `premain` method
    /// with either the two-arg or one-arg signature.
    NoPremainMethod {
        class: String,
        descriptors_tried: Vec<String>,
    },
    /// The agent's `premain` threw any Java `Throwable`; startup must abort.
    PremainFatalError { class: String, message: String },
    /// The VM could not create the Instrumentation implementation object.
    InstrumentationUnavailable { class: String, cause: String },
    /// The Premain-Class itself failed to load (e.g. ClassNotFoundException).
    PremainClassNotFound { class: String, cause: String },
    /// Internal VM error (linkage failure, out-of-memory during mirror
    /// construction, etc.) that escaped the normal premain-exception
    /// catch path.
    Internal(String),
    /// The agent's start threw and JPLIS's stderr (the throwable's trace and
    /// its assert lines, `instrument::print_jplis_premain_failure`) is
    /// already printed: the launcher ends the process as JPLIS's
    /// `abortJVM` does (interpreter round i4 wave 84, lane L1).
    JplisAbort { class: String },
}

impl fmt::Display for AgentLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentLoadError::BadPrefix(arg) => {
                write!(f, "expected `-javaagent:` prefix, got `{arg}`")
            }
            AgentLoadError::JarNotFound { path, source } => {
                write!(f, "cannot read agent JAR `{}`: {source}", path.display())
            }
            AgentLoadError::MissingPremainClass { jar } => {
                write!(
                    f,
                    "agent JAR `{}` has no `Premain-Class` attribute in MANIFEST.MF",
                    jar.display()
                )
            }
            AgentLoadError::NoPremainMethod {
                class,
                descriptors_tried,
            } => {
                write!(
                    f,
                    "Premain-Class `{class}` has no `premain` method matching any of: {}",
                    descriptors_tried.join(", ")
                )
            }
            AgentLoadError::PremainFatalError { class, message } => {
                write!(f, "premain of `{class}` threw: {message}")
            }
            AgentLoadError::InstrumentationUnavailable { class, cause } => {
                write!(
                    f,
                    "could not initialize Instrumentation for `{class}`: {cause}"
                )
            }
            AgentLoadError::PremainClassNotFound { class, cause } => {
                write!(f, "Premain-Class `{class}` failed to load: {cause}")
            }
            AgentLoadError::Internal(msg) => write!(f, "internal agent-loader error: {msg}"),
            AgentLoadError::JplisAbort { class } => {
                write!(f, "the start of agent `{class}` failed")
            }
        }
    }
}

impl std::error::Error for AgentLoadError {}

// ---------------------------------------------------------------------------
// parse_javaagent_spec — argv → LoadedAgent
// ---------------------------------------------------------------------------

/// Parse a single `-javaagent:<path>[=<options>]` argument and resolve
/// the JAR's manifest into a [`LoadedAgent`] descriptor.
///
/// The descriptor captures everything we need to run the dispatch later
/// — agent JAR I/O is done up-front so misconfigured agents fail before
/// we touch the VM's class loader.
///
/// Behaviour matches HotSpot's `JvmtiAgentList::add_javaagent`:
///   * `Premain-Class` MUST be present; missing → `MissingPremainClass`.
///   * `Boot-Class-Path` is split on whitespace per the JAR spec and
///     each entry is resolved relative to the JAR's parent directory.
///   * `Can-*` flags default to `false` if absent or non-`true`.
///   * `Agent-Class` is captured but only used for runtime attach,
///     which is out of scope for this WP.
pub fn parse_javaagent_spec(spec: &str) -> Result<LoadedAgent, AgentLoadError> {
    // Strip the `-javaagent:` prefix; bail if missing.
    let rest = spec
        .strip_prefix("-javaagent:")
        .ok_or_else(|| AgentLoadError::BadPrefix(spec.to_string()))?;

    // The agent args (after the first `=`) may themselves contain `=` —
    // e.g. `-javaagent:agent.jar=opt1=foo,opt2=bar`. So we split only on
    // the FIRST `=`.
    let (jar_path_str, agent_args) = match rest.split_once('=') {
        Some((p, args)) => (p, Some(args.to_string())),
        None => (rest, None),
    };
    let jar_path = PathBuf::from(jar_path_str);

    // Parse the manifest. We use cratonvm-classloading's `read_jar_manifest`
    // which already opens the JAR via the `zip` crate and runs
    // `ManifestInfo::parse` over `META-INF/MANIFEST.MF`.
    let manifest =
        cratonvm_classloading::ClassPath::read_jar_manifest(&jar_path).ok_or_else(|| {
            AgentLoadError::JarNotFound {
                path: jar_path.clone(),
                source: "open or parse failed".to_string(),
            }
        })?;

    parse_manifest_attributes(&jar_path, &manifest, agent_args)
}

/// Internal helper: turn a parsed `ManifestInfo` + JAR path into a
/// `LoadedAgent`. Split out so unit tests can synthesise a manifest
/// without touching the file system.
pub(crate) fn parse_manifest_attributes(
    jar_path: &Path,
    manifest: &ManifestInfo,
    agent_args: Option<String>,
) -> Result<LoadedAgent, AgentLoadError> {
    let premain_class = manifest
        .attributes
        .get("Premain-Class")
        .map(|s| s.trim().to_string())
        .ok_or_else(|| AgentLoadError::MissingPremainClass {
            jar: jar_path.to_path_buf(),
        })?;
    if premain_class.is_empty() {
        return Err(AgentLoadError::MissingPremainClass {
            jar: jar_path.to_path_buf(),
        });
    }

    let agent_class = manifest
        .attributes
        .get("Agent-Class")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let can_redefine = boolean_attribute(&manifest.attributes, "Can-Redefine-Classes");
    let can_retransform = boolean_attribute(&manifest.attributes, "Can-Retransform-Classes");
    let can_set_native_method_prefix =
        boolean_attribute(&manifest.attributes, "Can-Set-Native-Method-Prefix");

    // `Boot-Class-Path` entries are whitespace-separated relative URIs
    // resolved against the JAR's parent directory (per the JAR spec
    // and the `java.lang.instrument` package documentation). We
    // produce absolute `PathBuf`s where possible so the caller can
    // hand them straight to the bootstrap classpath.
    let jar_dir = jar_path.parent().unwrap_or_else(|| Path::new("."));
    let boot_class_path = manifest
        .attributes
        .get("Boot-Class-Path")
        .map(|raw| {
            raw.split_whitespace()
                .map(|entry| jar_dir.join(entry))
                .collect::<Vec<PathBuf>>()
        })
        .unwrap_or_default();

    Ok(LoadedAgent {
        jar_path: jar_path.to_path_buf(),
        agent_args,
        premain_class,
        agent_class,
        can_redefine,
        can_retransform,
        can_set_native_method_prefix,
        boot_class_path,
    })
}

/// Read a manifest attribute as a boolean per the spec rules:
/// only the literal `true` (case-insensitive) is `true`; anything else
/// (missing, empty, `false`, garbage) is `false`.
fn boolean_attribute(attrs: &std::collections::HashMap<String, String>, key: &str) -> bool {
    attrs
        .get(key)
        .map(|v| v.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// invoke_premains — dispatch sequence
// ---------------------------------------------------------------------------

/// Invoke `premain(...)` for every agent in `agents`, in the order the
/// CLI parsed them.
///
/// Called from `vm-cli` AFTER `Vm::new` has finished VM init (so
/// `java.lang.String`, `java.lang.Class`, and the synthetic / real-JDK
/// bootstrap subsystems are alive) but BEFORE invoking the
/// application's `main`.
///
/// On a per-agent failure, the first error is returned immediately.
/// Any requested agent failure aborts startup, matching HotSpot's
/// fail-closed handling for startup instrumentation agents.
pub fn invoke_premains(
    shared: &SharedVm,
    thread: &mut JvmThread,
    agents: &[LoadedAgent],
) -> Result<(), AgentLoadError> {
    // An `Instrumentation` exists from here on, before the application's
    // first class: record the JDK classes each loader initiates for its
    // `getInitiatedClasses` from now (`ClassRealm::jdk_names_initiated`;
    // interpreter round i1 wave 46, lane L5). Both modes since wave 47 (lane
    // L5): `--compatible` is held to HotSpot too, and kept the wave-44
    // constant-pool scan, which lists constants never resolved. With the
    // resolved class-entry table on (`ClassRealm::resolved_entry_table`) the
    // record is armed from VM creation already.
    if !agents.is_empty() {
        shared
            .classes
            .jdk_names_initiated_armed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    for agent in agents {
        invoke_one_premain(shared, thread, agent)?;
        tracing::info!("javaagent: {} premain completed", agent.premain_class);
    }
    Ok(())
}

/// Run the dispatch sequence for one agent.
fn invoke_one_premain(
    shared: &SharedVm,
    thread: &mut JvmThread,
    agent: &LoadedAgent,
) -> Result<(), AgentLoadError> {
    // Step 3: `Boot-Class-Path` entries are "searched by the bootstrap class
    // loader" (`java.lang.instrument`): HotSpot appends them, in manifest
    // order, after the runtime image -- the same search
    // `Instrumentation.appendToBootstrapClassLoaderSearch` and
    // `-Xbootclasspath/a:` extend -- so their classes report a null
    // `getClassLoader()` and are visible from instrumented `java.*` code
    // (the Mockito inline-mock / Jacoco / Byte Buddy `Advice` dispatcher
    // pattern). Step 4: the agent JAR itself goes on the system class path.
    //
    // Round 13 wave 11 (lane misc11): these entries used to go on the
    // APPLICATION class path with the agent JAR.
    // `CRATONVM_AGENT_BOOT_CLASS_PATH_BOOTSTRAP=0` restores that.
    {
        let (boot_paths, app_paths) = agent_search_paths(
            agent,
            true,
        );
        let mut cm = shared.classes.class_manager_write();
        if !boot_paths.is_empty() {
            cm.extend_bootstrap_classpath(&boot_paths);
        }
        drop(cm);
        for path in app_paths {
            // gci i17/na: not under `safe_native_call_impl`; the path
            // `String` (`create_string`) took the allocators' infallible arm
            // and aborted on a heap an earlier agent's `premain` filled. A
            // refusal is now a start-up failure, as the agent arguments'
            // `String` below already is.
            crate::runtime::native_oom::catch_alloc_oom_on(thread, |thread| {
                let mut ctx = crate::vm::NativeContextImpl { shared, thread };
                crate::runtime::instrument::append_system_search(&mut ctx, &path)
            })
            .map_err(|oom| {
                AgentLoadError::Internal(format!(
                    "agent system search: java.lang.OutOfMemoryError: {}",
                    oom.java_message()
                ))
            })?
            .map_err(|error| AgentLoadError::Internal(format!("agent system search: {error:?}")))?;
        }
    }

    // Step 5: resolve the Premain-Class.
    // Manifest spelling is `.`-separated; the VM's class loader uses
    // `/`-separated internal names.
    let premain_internal = agent.premain_class.replace('.', "/");

    // Interpreter round i4 wave 84, lane L1: the agent starts as JPLIS's
    // `processJavaStart` starts it, through the JDK's
    // `InstrumentationImpl.loadClassAndCallPremain` (the system loader's
    // `loadClass`, the declared `premain(String, Instrumentation)` else
    // `premain(String)`, the public check, `Method.invoke`, so a throwing
    // `premain` arrives wrapped in `InvocationTargetException`), whenever
    // the JDK has it; a failure prints JPLIS's stderr and is
    // `AgentLoadError::JplisAbort`. Rides the dynamic path's switch
    // (`VmConfig::attach_agentmain_jdk`, `CRATONVM_ATTACH_AGENTMAIN_JDK=0`
    // restores the Rust dispatch below for both).
    let jdk_start = shared.config.attach_agentmain_jdk && jdk_premain_caller_present(shared);

    // Eagerly load + verify so a `ClassNotFoundException` shows up as
    // `PremainClassNotFound` rather than a generic `NoPremainMethod`. With
    // `jdk_start` a class that does not load is the JDK's
    // `ClassNotFoundException`, printed as JPLIS prints it.
    let premain_class_id = match shared.load_class_concurrent(&premain_internal) {
        Ok(class_id) => Some(class_id),
        Err(_) if jdk_start => None,
        Err(e) => {
            return Err(AgentLoadError::PremainClassNotFound {
                class: agent.premain_class.clone(),
                cause: format!("{e}"),
            });
        }
    };
    // What HotSpot's `InstrumentationImpl.loadClassAndStartAgent` (its
    // `getDeclaredMethod` of `premain`) and the JPLIS library resolve through
    // loaders on the way, for `Instrumentation.getInitiatedClasses`
    // (interpreter round i1 wave 47, lane L5). One relaxed load while no
    // record is armed.
    crate::runtime::resolve::initiating_records::note_agent_start(shared, premain_class_id);

    if jdk_start {
        let inst = build_instrumentation_mirror(shared, thread, agent)?;
        return start_premain_through_jdk(shared, thread, agent, inst);
    }

    // Step 8: pick a method signature and invoke.
    //
    // The spec says we MUST first try the two-arg form
    // `premain(String, Instrumentation)` and fall back to the one-arg
    // form `premain(String)` if not present.
    let two_arg_desc = "(Ljava/lang/String;Ljava/lang/instrument/Instrumentation;)V";
    let one_arg_desc = "(Ljava/lang/String;)V";

    let has_two_arg = method_present(shared, &premain_internal, "premain", two_arg_desc);
    let has_one_arg = method_present(shared, &premain_internal, "premain", one_arg_desc);

    let agent_args_str = agent.agent_args.clone().unwrap_or_default();
    // Pooled, as before, but under `new`'s heap-exhaustion contract (gc-common
    // w9-c): an earlier agent's `premain` may have filled the heap, and the
    // infallible `create_java_string` aborted the process there instead of
    // reporting a start-up failure. The reference is held raw up to the
    // invoke, as before: `build_instrumentation_mirror` allocates without
    // collecting.
    let agent_args_ref =
        crate::runtime::interpreter::intern_string_literal_or_oom(shared, thread, &agent_args_str)
            .map_err(|e| {
                AgentLoadError::Internal(format!(
                    "premain of {}: cannot allocate the agent arguments String: {e:?}",
                    agent.premain_class
                ))
            })?;
    let agent_args_obj = Value::Object(Some(agent_args_ref));

    // `premain` is static: the static entry initializes the Premain-Class
    // (JVMS §5.5) even if a registered native were to answer it.
    let result = if has_two_arg {
        // Interpreter round i6 wave 118, lane L3 (`CRATONVM_INSTRUMENT_JDK_CONSTRUCTOR`):
        // the mirror's build may then run the JDK constructor (Java, a GC
        // point), so the agent arguments' `String` is pinned across it and
        // read back.
        let (instrumentation_ref, agent_args_obj) = {
            let pin = pin_agent_ref(shared, thread, agent_args_ref);
            let built = build_instrumentation_mirror(shared, thread, agent);
            let args_now = unpin_agent_ref(shared, thread, pin, agent_args_ref);
            (built?, Value::Object(Some(args_now)))
        };
        crate::vm::invoke_static_or_native(
            shared,
            thread,
            &premain_internal,
            "premain",
            two_arg_desc,
            &[agent_args_obj, Value::Object(Some(instrumentation_ref))],
            "agent_premain",
        )
    } else if has_one_arg {
        crate::vm::invoke_static_or_native(
            shared,
            thread,
            &premain_internal,
            "premain",
            one_arg_desc,
            &[agent_args_obj],
            "agent_premain",
        )
    } else {
        return Err(AgentLoadError::NoPremainMethod {
            class: agent.premain_class.clone(),
            descriptors_tried: vec![two_arg_desc.to_string(), one_arg_desc.to_string()],
        });
    };

    match result {
        Ok(_) => Ok(()),
        Err(MethodCallFailed::ExceptionThrown(exc_ref)) => {
            if shared.config.attach_agentmain_jdk {
                // Wave 84 (lane L1): JPLIS's stderr and abort, as on the JDK
                // start above, here for a JDK without
                // `loadClassAndCallPremain` (the throwable is not wrapped in
                // `InvocationTargetException` on this road).
                let mut ctx = crate::vm::NativeContextImpl { shared, thread };
                crate::runtime::instrument::print_jplis_premain_failure(&mut ctx, exc_ref);
                return Err(AgentLoadError::JplisAbort {
                    class: agent.premain_class.clone(),
                });
            }
            let exc_class_name = {
                let cm = shared.classes.class_manager.read();
                let cid = shared.mem.heap.class_id_of(exc_ref);
                cm.get_class(cid)
                    .map(|c| c.name.to_string())
                    .unwrap_or_else(|| format!("class#{cid}"))
            };
            Err(AgentLoadError::PremainFatalError {
                class: agent.premain_class.clone(),
                message: exc_class_name,
            })
        }
        Err(MethodCallFailed::InternalError(err)) => Err(AgentLoadError::Internal(format!(
            "premain of {}: {err}",
            agent.premain_class
        ))),
    }
}

/// The `(bootstrap, application)` search entries one agent adds: its
/// `Boot-Class-Path` entries on the bootstrap search, in manifest order, and
/// the agent JAR on the application class path. With `boot_class_path_bootstrap`
/// false (`CRATONVM_AGENT_BOOT_CLASS_PATH_BOOTSTRAP=0`) everything goes on the
/// application class path, `Boot-Class-Path` first, as before round 13 wave 11.
fn agent_search_paths(
    agent: &LoadedAgent,
    boot_class_path_bootstrap: bool,
) -> (Vec<String>, Vec<String>) {
    let mut boot: Vec<String> = agent
        .boot_class_path
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let mut app = if boot_class_path_bootstrap {
        Vec::new()
    } else {
        std::mem::take(&mut boot)
    };
    app.push(agent.jar_path.to_string_lossy().into_owned());
    (boot, app)
}

/// Try to construct a `sun.instrument.InstrumentationImpl` mirror.
///
/// Falls back to a bare allocation when the class is loadable but its
/// `<init>` is not registered, and to `None` when the class itself
/// cannot be loaded. Either fallback satisfies practical agents — the
/// real-JDK agents check for `null` before calling `addTransformer`.
fn build_instrumentation_mirror(
    shared: &SharedVm,
    thread: &mut JvmThread,
    agent: &LoadedAgent,
) -> Result<cratonvm_types::ObjectRef, AgentLoadError> {
    let class_internal = "sun/instrument/InstrumentationImpl";
    let class_id = shared
        .load_class_concurrent(class_internal)
        .map_err(|err| AgentLoadError::InstrumentationUnavailable {
            class: agent.premain_class.clone(),
            cause: format!("failed to load {class_internal}: {err}"),
        })?;

    // Pull num_total_fields so we allocate the right object size.
    let num_fields = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(class_id)
            .map(|c| c.num_total_fields)
            .unwrap_or(0)
    };
    // Fallible (gc-common w5-c, `handoff-w4c-infallible-callers-residue.md`
    // §2): an agent loaded late (dynamic attach) can meet a full heap, and the
    // infallible `alloc_object` aborts the process there. This function
    // already has a failure channel, so a refusal is reported through it.
    let inst_obj = shared
        .mem
        .heap
        .try_alloc_object_full(class_id, num_fields)
        .ok_or_else(|| AgentLoadError::InstrumentationUnavailable {
            class: agent.premain_class.clone(),
            cause: format!("Java heap space allocating {class_internal}"),
        })?;

    // The real-JDK constructor enters VM-private instrumentation setup.  The
    // mirror is instead backed by CratonVM's Instrumentation operations, so it
    // must remain a bare allocation: executing the JDK constructor can neither
    // initialize a real JVMTI environment nor establish its native state.
    //
    // Interpreter round i6 wave 118, lane L3 (step 1 of
    // `i42-L3-proposal-run-the-jdks-own-instrumentationimpl`;
    // `CRATONVM_INSTRUMENT_JDK_CONSTRUCTOR`, default on since wave 133): JDK 25's private
    // constructor, `InstrumentationImpl(long nativeAgent, boolean
    // environmentSupportsRedefineClasses, boolean
    // environmentSupportsNativeMethodPrefix, boolean printWarning)`, is pure
    // Java (its `TransformerManager`, the capability fields; the warning
    // branch only when `printWarning`, which a `-javaagent:` agent passes as
    // `false`). Running it gives the object the JDK's own state, which the
    // later steps (the public methods' JDK bodies) need; while the public
    // methods keep their natives nothing reads it. `nativeAgent` is 0: no
    // native leaf reads it yet. A failure keeps the bare object.
    let inst_obj = {
        run_jdk_constructor(shared, thread, agent, inst_obj)
    };
    // The agent's manifest capabilities, which the natives answering the
    // public `Instrumentation` methods read (interpreter round i1 wave 42,
    // lane L3; `instrument::agent_capability`). Nothing allocates here.
    crate::runtime::instrument::stamp_agent_capabilities(
        shared,
        thread,
        inst_obj,
        agent.can_redefine,
        agent.can_retransform,
        agent.can_set_native_method_prefix,
    );

    Ok(inst_obj)
}



/// The JDK 25 `InstrumentationImpl(JZZZ)V` constructor on the bare `inst`,
/// which is pinned across it; answers `inst` where it is now. A refusal is
/// logged under `CRATONVM_DBG_RETRANSFORM` and leaves the object as allocated
/// (its fields zero, as before).
fn run_jdk_constructor(
    shared: &SharedVm,
    thread: &mut JvmThread,
    agent: &LoadedAgent,
    inst: cratonvm_types::ObjectRef,
) -> cratonvm_types::ObjectRef {
    let mut ctx_impl = crate::vm::NativeContextImpl { shared, thread };
    let ctx: &mut dyn cratonvm_native_api::NativeContext = &mut ctx_impl;
    let pin = ctx.pin_native_root(inst);
    let ran = ctx.invoke_special(
        "sun/instrument/InstrumentationImpl",
        "<init>",
        "(JZZZ)V",
        &[
            Value::Object(Some(inst)),
            Value::Long(0),
            Value::Int(i32::from(agent.can_redefine)),
            Value::Int(i32::from(agent.can_set_native_method_prefix)),
            Value::Int(0),
        ],
    );
    let inst = ctx.read_native_pin(pin, inst);
    ctx.unpin_native_roots(pin);
    if let Err(e) = ran {
        if Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_ok() {
            eprintln!(
                "[instrument] InstrumentationImpl(JZZZ)V for {} refused: {e:?}",
                agent.premain_class
            );
        }
    }
    inst
}

/// Pin `obj` on `thread` (the agent-start path holds no native context of
/// its own); [`unpin_agent_ref`] reads it back and releases it.
fn pin_agent_ref(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: cratonvm_types::ObjectRef,
) -> usize {
    let mut ctx_impl = crate::vm::NativeContextImpl { shared, thread };
    let ctx: &mut dyn cratonvm_native_api::NativeContext = &mut ctx_impl;
    ctx.pin_native_root(obj)
}

/// `obj` where the pin [`pin_agent_ref`] took says it is now; the pin (and
/// any taken after it) is released.
fn unpin_agent_ref(
    shared: &SharedVm,
    thread: &mut JvmThread,
    pin: usize,
    obj: cratonvm_types::ObjectRef,
) -> cratonvm_types::ObjectRef {
    let mut ctx_impl = crate::vm::NativeContextImpl { shared, thread };
    let ctx: &mut dyn cratonvm_native_api::NativeContext = &mut ctx_impl;
    let now = ctx.read_native_pin(pin, obj);
    ctx.unpin_native_roots(pin);
    now
}

/// `InstrumentationImpl.loadClassAndCallPremain(String, String)`, the
/// method JPLIS's `processJavaStart` calls (`mPremainCaller`).
const PREMAIN_CALLER: (&str, &str, &str) = (
    "sun/instrument/InstrumentationImpl",
    "loadClassAndCallPremain",
    "(Ljava/lang/String;Ljava/lang/String;)V",
);

/// Does the class library carry JPLIS's premain caller
/// ([`PREMAIN_CALLER`])? Loads `InstrumentationImpl` to ask.
fn jdk_premain_caller_present(shared: &SharedVm) -> bool {
    let (class, name, descriptor) = PREMAIN_CALLER;
    shared.load_class_concurrent(class).is_ok() && method_present(shared, class, name, descriptor)
}

/// Start `agent` as JPLIS's `processJavaStart` does (interpreter round i4
/// wave 84, lane L1): `inst.loadClassAndCallPremain(premainClass,
/// options)`, `options` null when the command line gave none (JPLIS's
/// `commandStringIntoJavaStrings`: `-javaagent:a.jar` hands `premain` a null
/// `agentArgs`). A throwable out of it is what `invokeJavaAgentMainMethod`
/// describes and the three asserts follow
/// (`instrument::print_jplis_premain_failure`); the answer is then
/// [`AgentLoadError::JplisAbort`].
fn start_premain_through_jdk(
    shared: &SharedVm,
    thread: &mut JvmThread,
    agent: &LoadedAgent,
    inst: cratonvm_types::ObjectRef,
) -> Result<(), AgentLoadError> {
    let mut ctx_impl = crate::vm::NativeContextImpl { shared, thread };
    // Through the trait object, as `instrument.rs` calls these.
    let ctx: &mut dyn cratonvm_native_api::NativeContext = &mut ctx_impl;
    let pin = ctx.pin_native_root(inst);
    let class_name = ctx.create_string(&agent.premain_class);
    let class_pin = ctx.pin_native_root(class_name);
    let options = agent.agent_args.as_deref().map(|o| ctx.create_string(o));
    let class_name = ctx.read_native_pin(class_pin, class_name);
    let inst_now = ctx.read_native_pin(pin, inst);
    let (class, name, descriptor) = PREMAIN_CALLER;
    let result = ctx.invoke(
        class,
        name,
        descriptor,
        &[
            Value::Object(Some(inst_now)),
            Value::Object(Some(class_name)),
            Value::Object(options),
        ],
    );
    ctx.unpin_native_roots(pin);
    match result {
        Ok(_) => Ok(()),
        Err(MethodCallFailed::ExceptionThrown(thrown)) => {
            crate::runtime::instrument::print_jplis_premain_failure(ctx, thrown);
            Err(AgentLoadError::JplisAbort {
                class: agent.premain_class.clone(),
            })
        }
        Err(MethodCallFailed::InternalError(err)) => Err(AgentLoadError::Internal(format!(
            "premain of {}: {err}",
            agent.premain_class
        ))),
    }
}

/// Returns `true` iff a class declares (or inherits) a method with the
/// given name and descriptor. Used to pick the two-arg-vs-one-arg
/// `premain` overload.
fn method_present(shared: &SharedVm, class_internal: &str, name: &str, descriptor: &str) -> bool {
    let cm = shared.classes.class_manager.read();
    let cid = match cm.get_loaded_class_id(class_internal) {
        Some(id) => id,
        None => return false,
    };
    cm.get_class(cid)
        .and_then(|c| c.find_method(name, descriptor))
        .is_some()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
