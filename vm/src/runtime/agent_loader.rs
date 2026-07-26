// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP2.4-C — `-javaagent:` JAR loader and `Premain-Class` dispatcher.
//!
//! When the user passes `-javaagent:foo.jar=opts` on the command line, the
//! JVM is required to:
//!
//!   1. Open the JAR and parse `META-INF/MANIFEST.MF` for the
//!      `Premain-Class`, `Boot-Class-Path`, and `Can-*` capability flags.
//!   2. Prepend any `Boot-Class-Path` entries to the bootstrap classpath
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
    // Step 3: Boot-Class-Path entries get prepended to the bootstrap
    // classpath. The classloader exposes `extend_application_classpath`;
    // there is no public bootstrap-prepend at the time of writing, so
    // we add Boot-Class-Path entries to the application classpath as
    // well — this is strictly weaker than HotSpot but keeps the agent's
    // helper classes loadable, which is the practical effect that
    // Mockito / Jacoco rely on.
    {
        let mut cm = shared.classes.class_manager_write();
        let mut paths: Vec<String> = agent
            .boot_class_path
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();

        // Step 4: also add the agent JAR itself.
        paths.push(agent.jar_path.to_string_lossy().into_owned());

        cm.extend_application_classpath(&paths);
    }

    // Step 5: resolve the Premain-Class.
    // Manifest spelling is `.`-separated; the VM's class loader uses
    // `/`-separated internal names.
    let premain_internal = agent.premain_class.replace('.', "/");

    // Eagerly load + verify so a `ClassNotFoundException` shows up as
    // `PremainClassNotFound` rather than a generic `NoPremainMethod`.
    if let Err(e) = shared.load_class_concurrent(&premain_internal) {
        return Err(AgentLoadError::PremainClassNotFound {
            class: agent.premain_class.clone(),
            cause: format!("{e}"),
        });
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
    let agent_args_obj =
        Value::Object(Some(crate::vm::create_java_string(shared, &agent_args_str)));

    let result = if has_two_arg {
        let instrumentation_ref = build_instrumentation_mirror(shared, thread, agent)?;
        crate::vm::invoke_or_native(
            shared,
            thread,
            &premain_internal,
            "premain",
            two_arg_desc,
            &[agent_args_obj, Value::Object(Some(instrumentation_ref))],
        )
    } else if has_one_arg {
        crate::vm::invoke_or_native(
            shared,
            thread,
            &premain_internal,
            "premain",
            one_arg_desc,
            &[agent_args_obj],
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
    let inst_obj = shared.mem.heap.alloc_object(class_id, num_fields);

    // The real-JDK constructor enters VM-private instrumentation setup.  The
    // mirror is instead backed by CratonVM's Instrumentation operations, so it
    // must remain a bare allocation: executing the JDK constructor can neither
    // initialize a real JVMTI environment nor establish its native state.
    let _ = thread;
    let _ = agent;

    Ok(inst_obj)
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
mod tests {
    use super::*;

    #[test]
    fn parse_manifest_premain_only() {
        let manifest_text = "Manifest-Version: 1.0\r\nPremain-Class: foo.Bar\r\n";
        let info = ManifestInfo::parse(manifest_text.as_bytes());
        let agent =
            parse_manifest_attributes(Path::new("/tmp/x.jar"), &info, Some("opt1=foo".into()))
                .expect("parse_manifest_attributes");
        assert_eq!(agent.premain_class, "foo.Bar");
        assert_eq!(agent.agent_args.as_deref(), Some("opt1=foo"));
        assert!(!agent.can_redefine);
        assert!(!agent.can_retransform);
        assert!(!agent.can_set_native_method_prefix);
        assert!(agent.boot_class_path.is_empty());
    }

    #[test]
    fn parse_manifest_full_attributes() {
        let manifest_text = "Manifest-Version: 1.0\r\n\
             Premain-Class: com.example.Agent\r\n\
             Agent-Class: com.example.Agent\r\n\
             Boot-Class-Path: lib/x.jar lib/y.jar\r\n\
             Can-Redefine-Classes: true\r\n\
             Can-Retransform-Classes: TRUE\r\n\
             Can-Set-Native-Method-Prefix: false\r\n";
        let info = ManifestInfo::parse(manifest_text.as_bytes());
        let agent = parse_manifest_attributes(Path::new("/opt/agents/agent.jar"), &info, None)
            .expect("parse_manifest_attributes");

        assert_eq!(agent.premain_class, "com.example.Agent");
        assert_eq!(agent.agent_class.as_deref(), Some("com.example.Agent"));
        assert!(agent.can_redefine);
        assert!(agent.can_retransform);
        assert!(!agent.can_set_native_method_prefix);
        // Boot-Class-Path entries are resolved relative to the JAR's parent.
        assert_eq!(agent.boot_class_path.len(), 2);
        assert!(
            agent.boot_class_path[0].ends_with("lib/x.jar")
                || agent.boot_class_path[0].ends_with("lib\\x.jar")
        );
        assert!(
            agent.boot_class_path[1].ends_with("lib/y.jar")
                || agent.boot_class_path[1].ends_with("lib\\y.jar")
        );
    }

    #[test]
    fn parse_manifest_missing_premain_class_errors() {
        let manifest_text = "Manifest-Version: 1.0\r\nAgent-Class: x.Y\r\n";
        let info = ManifestInfo::parse(manifest_text.as_bytes());
        let err = parse_manifest_attributes(Path::new("/tmp/x.jar"), &info, None)
            .expect_err("missing Premain-Class should error");
        assert!(matches!(err, AgentLoadError::MissingPremainClass { .. }));
    }

    #[test]
    fn parse_manifest_empty_premain_class_errors() {
        let manifest_text = "Manifest-Version: 1.0\r\nPremain-Class: \r\n";
        let info = ManifestInfo::parse(manifest_text.as_bytes());
        let err = parse_manifest_attributes(Path::new("/tmp/x.jar"), &info, None)
            .expect_err("empty Premain-Class should error");
        assert!(matches!(err, AgentLoadError::MissingPremainClass { .. }));
    }

    #[test]
    fn parse_javaagent_spec_bad_prefix() {
        let err =
            parse_javaagent_spec("-Xfoo:bar.jar").expect_err("non -javaagent: arg should fail");
        assert!(matches!(err, AgentLoadError::BadPrefix(_)));
    }

    #[test]
    fn parse_javaagent_spec_missing_jar_file() {
        // Using a non-existent jar should produce JarNotFound.
        let err = parse_javaagent_spec("-javaagent:/nonexistent/path/definitely-not-here.jar")
            .expect_err("missing jar should fail");
        assert!(matches!(err, AgentLoadError::JarNotFound { .. }));
    }

    #[test]
    fn boolean_attribute_parses_case_insensitively() {
        let mut attrs = std::collections::HashMap::new();
        attrs.insert("A".to_string(), "true".to_string());
        attrs.insert("B".to_string(), "TRUE".to_string());
        attrs.insert("C".to_string(), "True".to_string());
        attrs.insert("D".to_string(), "false".to_string());
        attrs.insert("E".to_string(), "garbage".to_string());

        assert!(boolean_attribute(&attrs, "A"));
        assert!(boolean_attribute(&attrs, "B"));
        assert!(boolean_attribute(&attrs, "C"));
        assert!(!boolean_attribute(&attrs, "D"));
        assert!(!boolean_attribute(&attrs, "E"));
        assert!(!boolean_attribute(&attrs, "Missing"));
    }

    /// Parse a real `-javaagent:foo.jar=opts` argument by writing a
    /// synthesised JAR to a tempdir; verifies the round-trip from a
    /// CLI-style string all the way through `LoadedAgent` extraction.
    #[test]
    fn parse_javaagent_spec_full_round_trip() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        use zip::ZipWriter;

        let tmp = tempfile::tempdir().expect("tempdir");
        let jar_path = tmp.path().join("test-agent.jar");

        // Synthesise a JAR with just MANIFEST.MF.
        let manifest_text = "Manifest-Version: 1.0\r\n\
             Premain-Class: com.example.MyAgent\r\n\
             Boot-Class-Path: lib/extra.jar\r\n\
             Can-Redefine-Classes: true\r\n";
        {
            let file = std::fs::File::create(&jar_path).expect("create jar");
            let mut zip = ZipWriter::new(file);
            let opts: SimpleFileOptions =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            zip.start_file("META-INF/MANIFEST.MF", opts)
                .expect("start file");
            zip.write_all(manifest_text.as_bytes())
                .expect("write manifest");
            zip.finish().expect("finish zip");
        }

        let spec = format!("-javaagent:{}=key=value,opt2", jar_path.to_string_lossy());
        let agent = parse_javaagent_spec(&spec).expect("round-trip ok");
        assert_eq!(agent.premain_class, "com.example.MyAgent");
        assert_eq!(agent.agent_args.as_deref(), Some("key=value,opt2"));
        assert!(agent.can_redefine);
        assert!(!agent.can_retransform);
        assert_eq!(agent.boot_class_path.len(), 1);
        assert!(
            agent.boot_class_path[0].ends_with("lib/extra.jar")
                || agent.boot_class_path[0].ends_with("lib\\extra.jar")
        );
        assert_eq!(agent.jar_path, jar_path);
    }

    /// `-javaagent:foo.jar` with no `=opts` should still parse, with
    /// `agent_args` left as `None` so that the dispatcher sends an
    /// empty string to the Java side.
    #[test]
    fn parse_javaagent_spec_no_opts() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        use zip::ZipWriter;

        let tmp = tempfile::tempdir().expect("tempdir");
        let jar_path = tmp.path().join("noopts.jar");

        let manifest_text = "Manifest-Version: 1.0\r\nPremain-Class: x.Y\r\n";
        {
            let file = std::fs::File::create(&jar_path).expect("create jar");
            let mut zip = ZipWriter::new(file);
            let opts: SimpleFileOptions =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            zip.start_file("META-INF/MANIFEST.MF", opts)
                .expect("start file");
            zip.write_all(manifest_text.as_bytes())
                .expect("write manifest");
            zip.finish().expect("finish zip");
        }

        let spec = format!("-javaagent:{}", jar_path.to_string_lossy());
        let agent = parse_javaagent_spec(&spec).expect("ok");
        assert_eq!(agent.premain_class, "x.Y");
        assert!(agent.agent_args.is_none());
    }

    /// `=` inside the agent-arg portion must be preserved verbatim.
    #[test]
    fn parse_javaagent_spec_preserves_equals_in_args() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        use zip::ZipWriter;

        let tmp = tempfile::tempdir().expect("tempdir");
        let jar_path = tmp.path().join("args-with-equals.jar");

        let manifest_text = "Manifest-Version: 1.0\r\nPremain-Class: a.B\r\n";
        {
            let file = std::fs::File::create(&jar_path).expect("create jar");
            let mut zip = ZipWriter::new(file);
            let opts: SimpleFileOptions =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            zip.start_file("META-INF/MANIFEST.MF", opts)
                .expect("start file");
            zip.write_all(manifest_text.as_bytes())
                .expect("write manifest");
            zip.finish().expect("finish zip");
        }

        // First `=` separates path from args; subsequent `=` are agent-side.
        let spec = format!("-javaagent:{}=k1=v1,k2=v2", jar_path.to_string_lossy());
        let agent = parse_javaagent_spec(&spec).expect("ok");
        assert_eq!(agent.agent_args.as_deref(), Some("k1=v1,k2=v2"));
    }

    /// Empty agent list — invoke_premains should be a no-op success.
    /// Note: full premain dispatch can't be tested here without a
    /// full VM bootstrap; that lives in the wave2-4 integration tests
    /// (Owner D). What we DO test here is that the parser produces the
    /// LoadedAgent values that the dispatcher reads.
    #[test]
    fn loaded_agent_roundtrip_fields() {
        let a = LoadedAgent {
            jar_path: PathBuf::from("/tmp/x.jar"),
            agent_args: Some("o".into()),
            premain_class: "com.x.Y".into(),
            agent_class: Some("com.x.Y".into()),
            can_redefine: true,
            can_retransform: false,
            can_set_native_method_prefix: true,
            boot_class_path: vec![PathBuf::from("/tmp/lib/a.jar")],
        };
        // The struct is Clone + Debug — no panic on either path.
        let cloned = a.clone();
        assert_eq!(cloned.premain_class, "com.x.Y");
        assert!(cloned.can_redefine);
        let _ = format!("{a:?}");
    }
}
