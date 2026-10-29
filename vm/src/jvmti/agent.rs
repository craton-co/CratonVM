// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JVMTI agent loading framework.

use std::path::Path;

use super::JvmtiError;

/// The phase at which an agent was loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPhase {
    /// Loaded during VM startup (`-agentlib` / `-agentpath` / `-javaagent`).
    OnLoad,
    /// Loaded into a running VM via the Attach API.
    Live,
}

/// Handle to a dynamically loaded agent library.
pub struct AgentLibrary {
    /// The underlying `libloading::Library`, if the library was loaded.
    library: Option<libloading::Library>,
}

// Safety: Agent libraries are loaded once and their handles are process-global.
unsafe impl Send for AgentLibrary {}
unsafe impl Sync for AgentLibrary {}

impl AgentLibrary {
    fn empty() -> Self {
        Self { library: None }
    }

    /// Returns `true` if the dynamic library was successfully loaded.
    pub fn is_loaded(&self) -> bool {
        self.library.is_some()
    }
}

/// An agent library stays mapped for the life of the process, as in HotSpot
/// (interpreter round i1 wave 13): the agent's callbacks, which a C
/// `jvmtiEnv` keeps (`jvmti::native_env`, never freed), and any thread it
/// started point into it. Dropping the handle used to `dlclose` it with the
/// VM's `AgentRegistry`.
impl Drop for AgentLibrary {
    fn drop(&mut self) {
        if let Some(library) = self.library.take() {
            std::mem::forget(library);
        }
    }
}

impl std::fmt::Debug for AgentLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLibrary")
            .field("loaded", &self.library.is_some())
            .finish()
    }
}

/// Information about a loaded agent.
#[derive(Debug)]
pub struct AgentInfo {
    pub name: String,
    pub options: String,
    pub phase: AgentPhase,
    /// The loaded library, if dynamic loading succeeded.
    pub library: AgentLibrary,
    /// Whether `Agent_OnLoad` / `Agent_OnAttach` was successfully called.
    pub on_load_called: bool,
}

impl Clone for AgentInfo {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            options: self.options.clone(),
            phase: self.phase,
            // Library handles are not cloneable; clones get an empty handle.
            library: AgentLibrary::empty(),
            on_load_called: self.on_load_called,
        }
    }
}

/// Type signature of the JVMTI `Agent_OnLoad` entry point.
/// `Agent_OnLoad(JavaVM *vm, char *options, void *reserved) -> jint`
type AgentOnLoadFn = unsafe extern "C" fn(
    *mut std::os::raw::c_void,
    *const std::os::raw::c_char,
    *mut std::os::raw::c_void,
) -> i32;

/// Type signature of the JVMTI `Agent_OnAttach` entry point.
type AgentOnAttachFn = unsafe extern "C" fn(
    *mut std::os::raw::c_void,
    *const std::os::raw::c_char,
    *mut std::os::raw::c_void,
) -> i32;

/// Type signature of the JVMTI `Agent_OnUnload` entry point.
type AgentOnUnloadFn = unsafe extern "C" fn(*mut std::os::raw::c_void);

/// Registry that tracks loaded agents.
#[derive(Debug)]
pub struct AgentRegistry {
    agents: Vec<AgentInfo>,
    /// The resolved paths of the libraries a live-phase load opened, for
    /// HotSpot's once-per-library dynamic-agent warning
    /// (`JvmtiAgentList::is_dynamic_lib_loaded`).
    dynamic_libraries: Vec<String>,
}

impl AgentRegistry {
    /// Create a new, empty agent registry.
    pub fn new() -> Self {
        Self {
            agents: Vec::new(),
            dynamic_libraries: Vec::new(),
        }
    }

    /// Load an agent from the given path with the specified options.
    ///
    /// This attempts to:
    /// 1. Load the dynamic library from `path`
    /// 2. Look up the `Agent_OnLoad` symbol
    /// 3. Call the entry point
    /// 4. Register the agent in the registry
    ///
    /// Startup agents are fail-closed: a missing library, missing entry point,
    /// invalid options string, or non-zero `Agent_OnLoad` return aborts loading
    /// and leaves the registry unchanged.
    ///
    /// `Agent_OnLoad` receives the process's `JavaVM*` (interpreter round i1
    /// wave 11; it received NULL, so an agent's first `(*vm)->GetEnv` was a
    /// null dereference). `GetEnv` for a JVMTI version answers a C
    /// `jvmtiEnv` since wave 12 (`jvmti::native_env`: capabilities, event
    /// callbacks and breakpoints; every other function answers
    /// `JVMTI_ERROR_NOT_AVAILABLE`).
    ///
    /// `options` is `None` for a command-line agent without an option string
    /// (`-agentpath:<lib>` with no `=`): `Agent_OnLoad` then gets a NULL
    /// `options`, as HotSpot hands it (`Arguments::parse_each_vm_init_arg`
    /// keeps `options` NULL without one; interpreter round i2 wave 53, lane
    /// L1, `L1W53JvmtiClassHooks`' `onload` row). Until wave 53 every agent
    /// got a pointer to an empty string, and an agent that tests
    /// `options == NULL` took its with-options branch.
    pub fn load_agent(&mut self, path: &str, options: Option<&str>) -> Result<(), JvmtiError> {
        let vm = crate::native::jni::get_java_vm()
            .cast_mut()
            .cast::<std::os::raw::c_void>();
        self.load_agent_for_phase(
            path,
            resolve_agent_path(path),
            options,
            AgentPhase::OnLoad,
            vm,
            false,
        )
    }

    /// Load a command-line startup agent as HotSpot's `JvmtiAgent::load`
    /// finds it (interpreter round i2 wave 55, lane L1): `-agentpath:`
    /// (`agentlib` false) loads `name` exactly as given, a relative path
    /// included; `-agentlib:` loads the platform library of `name` from the
    /// JDK's own library directory when the VM has a JDK home and the file is
    /// there (`os::dll_locate_lib` over `Arguments::get_dll_dir()`:
    /// `<java_home>/lib/lib<name>.so`, `<java_home>\bin\<name>.dll` on
    /// Windows), else by its platform name from the library path. Until wave
    /// 55 both went through [`resolve_agent_path`], so an agent installed in
    /// the JDK was not found and `-agentpath:x.so` looked for `libx.so.so`.
    pub fn load_startup_agent(
        &mut self,
        name: &str,
        options: Option<&str>,
        agentlib: bool,
        java_home: Option<&str>,
    ) -> Result<(), JvmtiError> {
        let resolved = if agentlib {
            agentlib_path(name, java_home)
        } else {
            name.to_string()
        };
        let vm = crate::native::jni::get_java_vm()
            .cast_mut()
            .cast::<std::os::raw::c_void>();
        self.load_agent_for_phase(name, resolved, options, AgentPhase::OnLoad, vm, false)
    }

    /// Load a native agent through a self-attach (interpreter round i2 wave
    /// 55, lane L1: `VirtualMachine.loadAgentPath` when `absolute`, else
    /// `loadAgentLibrary`, `runtime::instrument::native_vm_load_native_agent`),
    /// found as [`Self::load_startup_agent`] finds a command-line one, and
    /// call its `Agent_OnAttach` with the process's `JavaVM*` and `options`
    /// (never NULL: HotSpot hands an absent option string over as an empty
    /// one, measured).
    pub fn load_attach_agent(
        &mut self,
        name: &str,
        options: &str,
        absolute: bool,
        java_home: Option<&str>,
        dynamic_warning: bool,
    ) -> Result<(), JvmtiError> {
        let resolved = if absolute {
            name.to_string()
        } else {
            agentlib_path(name, java_home)
        };
        let vm = crate::native::jni::get_java_vm()
            .cast_mut()
            .cast::<std::os::raw::c_void>();
        self.load_agent_for_phase(
            name,
            resolved,
            Some(options),
            AgentPhase::Live,
            vm,
            dynamic_warning,
        )
    }

    /// Take over `other`'s agents (wave 55: a self-attach loads its agent in
    /// a registry taken out of the VM's JVMTI lock, then returns it here).
    pub fn absorb(&mut self, mut other: AgentRegistry) {
        self.agents.append(&mut other.agents);
        self.dynamic_libraries.append(&mut other.dynamic_libraries);
    }

    /// Load an agent via the Attach API (live phase).
    pub fn load_agent_live(
        &mut self,
        path: &str,
        options: &str,
        vm_ptr: *mut std::os::raw::c_void,
    ) -> Result<(), JvmtiError> {
        self.load_agent_for_phase(
            path,
            resolve_agent_path(path),
            Some(options),
            AgentPhase::Live,
            vm_ptr,
            false,
        )
    }

    fn load_agent_for_phase(
        &mut self,
        path: &str,
        resolved_path: String,
        options: Option<&str>,
        phase: AgentPhase,
        vm_ptr: *mut std::os::raw::c_void,
        dynamic_warning: bool,
    ) -> Result<(), JvmtiError> {
        if path.is_empty() {
            return Err(JvmtiError::Internal);
        }

        let mut info = AgentInfo {
            name: path.to_string(),
            options: options.unwrap_or_default().to_string(),
            phase,
            library: AgentLibrary::empty(),
            on_load_called: false,
        };

        let lib = unsafe { libloading::Library::new(&resolved_path) }.map_err(|err| {
            JvmtiError::AgentLibraryLoadFailed {
                path: resolved_path.clone(),
                cause: err.to_string(),
            }
        })?;
        tracing::info!("Loaded agent library: {}", resolved_path);
        // Interpreter round i7 (final JVMTI battery, `L1W55AttachNativeAgent`,
        // `L1K54JvmtiLateAgentLambdaFrames`): HotSpot 25's `JvmtiAgent::load`
        // prints this on stderr once the library is open and before its
        // `Agent_OnAttach` runs, for the first live load of each library,
        // naming the agent as given (MEASURED: the five later loads of the
        // same library print nothing more).
        if phase == AgentPhase::Live && !self.dynamic_libraries.contains(&resolved_path) {
            self.dynamic_libraries.push(resolved_path.clone());
            if dynamic_warning {
                print_jvmti_dynamic_agent_warning(path);
            }
        }

        let entry_name: &[u8] = match phase {
            AgentPhase::OnLoad => b"Agent_OnLoad",
            AgentPhase::Live => b"Agent_OnAttach",
        };
        let entry_symbol = std::str::from_utf8(entry_name).unwrap_or("?").to_string();

        let options_cstring = options
            .map(std::ffi::CString::new)
            .transpose()
            .map_err(|err| JvmtiError::InvalidAgentOptions {
                path: resolved_path.clone(),
                cause: err.to_string(),
            })?;
        // NULL when the command line gave no option string.
        let options_ptr = options_cstring
            .as_ref()
            .map_or(std::ptr::null(), |options| options.as_ptr());

        let code = match phase {
            AgentPhase::OnLoad => unsafe {
                let entry = lib.get::<AgentOnLoadFn>(entry_name).map_err(|_| {
                    JvmtiError::AgentEntryPointMissing {
                        path: resolved_path.clone(),
                        symbol: entry_symbol.clone(),
                    }
                })?;
                entry(vm_ptr, options_ptr, std::ptr::null_mut())
            },
            AgentPhase::Live => unsafe {
                let entry = lib.get::<AgentOnAttachFn>(entry_name).map_err(|_| {
                    JvmtiError::AgentEntryPointMissing {
                        path: resolved_path.clone(),
                        symbol: entry_symbol.clone(),
                    }
                })?;
                entry(vm_ptr, options_ptr, std::ptr::null_mut())
            },
        };

        if code != 0 {
            // The entry point ran: it may have obtained a C `jvmtiEnv`, set
            // callbacks into this library and enabled events before failing,
            // and envs are never freed (`jvmti::native_env`). Keep it mapped,
            // as `Drop for AgentLibrary` keeps a loaded one (wave 14; it was
            // `dlclose`d here).
            std::mem::forget(lib);
            return Err(JvmtiError::AgentEntryPointFailed {
                path: resolved_path,
                symbol: entry_symbol,
                code,
            });
        }

        info.on_load_called = true;
        tracing::info!("Agent {} entry point returned success", entry_symbol);
        info.library = AgentLibrary { library: Some(lib) };
        self.agents.push(info);
        Ok(())
    }

    /// Call `Agent_OnUnload` for all agents that have loaded libraries, with
    /// the process's `JavaVM*` (interpreter round i1 wave 13: it got NULL, as
    /// `Agent_OnLoad` did before wave 11).
    ///
    /// The libraries themselves are **not** unloaded (matching HotSpot behaviour:
    /// agent shared libraries remain mapped for the lifetime of the process;
    /// `Drop for AgentLibrary`).
    pub fn unload_all(&mut self) {
        let vm = crate::native::jni::get_java_vm()
            .cast_mut()
            .cast::<std::os::raw::c_void>();
        for agent in &mut self.agents {
            if let Some(ref lib) = agent.library.library {
                let sym: Result<libloading::Symbol<'_, AgentOnUnloadFn>, _> =
                    unsafe { lib.get(b"Agent_OnUnload") };
                if let Ok(f) = sym {
                    unsafe { f(vm) };
                    tracing::info!("Called Agent_OnUnload for {}", agent.name);
                }
            }
            // Intentionally do NOT drop the library to keep it mapped.
        }
    }

    /// Return a slice of all registered agents.
    pub fn agents(&self) -> &[AgentInfo] {
        &self.agents
    }
}

/// HotSpot's warning for a JVM TI agent loaded dynamically (the Java agent's
/// is `instrument::print_dynamic_agent_warning`).
#[cold]
#[inline(never)]
fn print_jvmti_dynamic_agent_warning(name: &str) {
    use std::io::Write;
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "WARNING: A JVM TI agent has been loaded dynamically ({name})");
    let _ = writeln!(
        err,
        "WARNING: If a serviceability tool is in use, please run with -XX:+EnableDynamicAgentLoading to hide this warning"
    );
    let _ = writeln!(
        err,
        "WARNING: Dynamic loading of agents will be disallowed by default in a future release"
    );
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve an agent name to a platform-specific library path.
///
/// If `path` is already an absolute path or contains a path separator, use it
/// directly.  Otherwise, apply platform-specific naming conventions:
/// - Linux: `lib{name}.so`
/// - macOS: `lib{name}.dylib`
/// - Windows: `{name}.dll`
fn resolve_agent_path(path: &str) -> String {
    if Path::new(path).is_absolute() || path.contains('/') || path.contains('\\') {
        return path.to_string();
    }
    platform_library_name(path)
}

/// The platform library name of agent `name` (`lib<name>.so`,
/// `lib<name>.dylib`, `<name>.dll`), whatever `name` holds, as HotSpot's
/// `os::dll_build_name` builds it for `-agentlib:` (wave 55).
fn platform_library_name(name: &str) -> String {
    #[cfg(target_os = "linux")]
    {
        format!("lib{name}.so")
    }
    #[cfg(target_os = "macos")]
    {
        format!("lib{name}.dylib")
    }
    #[cfg(target_os = "windows")]
    {
        format!("{name}.dll")
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        name.to_string()
    }
}

/// What `-agentlib:<name>` loads (wave 55, [`AgentRegistry::load_startup_agent`]):
/// the platform library in the JDK's library directory (`bin` on Windows,
/// `lib` elsewhere) when it exists there, else the bare platform name, which
/// the platform loader looks for on the library path.
fn agentlib_path(name: &str, java_home: Option<&str>) -> String {
    let file = platform_library_name(name);
    // Not the JDK's own `libjdwp`: it would run against this VM's partial C
    // JVMTI table (`jvmti::native_env`), where it fails or misbehaves. Since
    // interpreter round i4 wave 85 the launcher maps `-agentlib:jdwp` onto
    // the built-in JDWP server (docs/feature-designs/jvmti-delivery-threading.md),
    // so only an embedder's option reaches here; it is looked for on the
    // library path only, as before wave 55 (docs/feature-designs/jdk-only-mode.md).
    if name == "jdwp" {
        return file;
    }
    let dir = if cfg!(target_os = "windows") {
        "bin"
    } else {
        "lib"
    };
    if let Some(home) = java_home.filter(|home| !home.is_empty()) {
        let candidate = Path::new(home).join(dir).join(&file);
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    file
}

/// Parse an agent command-line argument into `(name_or_path, options)`.
///
/// Supported formats:
/// - `-agentlib:name=options`
/// - `-agentlib:name`
/// - `-agentpath:/path/to/lib=options`
/// - `-agentpath:/path/to/lib`
/// - `-javaagent:path=options`
/// - `-javaagent:path`
///
/// The option string is `None` when the argument has no `=` (interpreter
/// round i2 wave 53: `Agent_OnLoad` then gets NULL, [`AgentRegistry::load_agent`]).
pub fn parse_agent_arg(arg: &str) -> (String, Option<String>) {
    let prefixes = ["-agentlib:", "-agentpath:", "-javaagent:"];

    for prefix in &prefixes {
        if let Some(rest) = arg.strip_prefix(prefix) {
            return if let Some((name, opts)) = rest.split_once('=') {
                (name.to_string(), Some(opts.to_string()))
            } else {
                (rest.to_string(), None)
            };
        }
    }

    // If no known prefix, treat the whole argument as the name.
    (arg.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_agentlib_with_options() {
        let (name, opts) = parse_agent_arg("-agentlib:jdwp=transport=dt_socket,server=y");
        assert_eq!(name, "jdwp");
        assert_eq!(opts.as_deref(), Some("transport=dt_socket,server=y"));
    }

    #[test]
    fn test_parse_agentlib_no_options() {
        let (name, opts) = parse_agent_arg("-agentlib:hprof");
        assert_eq!(name, "hprof");
        assert_eq!(opts, None, "no option string is NULL, not empty");
    }

    #[test]
    fn test_parse_agentpath() {
        let (name, opts) = parse_agent_arg("-agentpath:/usr/lib/agent.so=opt1");
        assert_eq!(name, "/usr/lib/agent.so");
        assert_eq!(opts.as_deref(), Some("opt1"));
    }

    #[test]
    fn test_parse_javaagent() {
        let (name, opts) = parse_agent_arg("-javaagent:myagent.jar=config");
        assert_eq!(name, "myagent.jar");
        assert_eq!(opts.as_deref(), Some("config"));
    }

    #[test]
    fn test_parse_unknown_format() {
        let (name, opts) = parse_agent_arg("something_else");
        assert_eq!(name, "something_else");
        assert_eq!(opts, None);
    }

    #[test]
    fn test_missing_agents_fail_closed_without_registration() {
        let mut registry = AgentRegistry::new();
        let err = registry
            .load_agent("definitely_not_a_real_library_1", Some("opt1"))
            .expect_err("missing startup agent must fail closed");
        assert!(matches!(err, JvmtiError::AgentLibraryLoadFailed { .. }));
        assert!(registry.agents().is_empty());

        let err = registry
            .load_agent("definitely_not_a_real_library_2", Some("opt2"))
            .expect_err("subsequent missing agent must also fail closed");
        assert!(matches!(err, JvmtiError::AgentLibraryLoadFailed { .. }));
        assert!(registry.agents().is_empty());
    }

    #[test]
    fn test_load_agent_empty_path_fails() {
        let mut registry = AgentRegistry::new();
        assert!(registry.load_agent("", Some("opts")).is_err());
    }

    #[test]
    fn test_resolve_agent_path_absolute() {
        #[cfg(target_os = "windows")]
        assert_eq!(
            resolve_agent_path("C:\\agents\\test.dll"),
            "C:\\agents\\test.dll"
        );
        #[cfg(not(target_os = "windows"))]
        assert_eq!(resolve_agent_path("/usr/lib/test.so"), "/usr/lib/test.so");
    }

    #[test]
    fn test_resolve_agent_path_name_only() {
        let resolved = resolve_agent_path("jdwp");
        #[cfg(target_os = "linux")]
        assert_eq!(resolved, "libjdwp.so");
        #[cfg(target_os = "macos")]
        assert_eq!(resolved, "libjdwp.dylib");
        #[cfg(target_os = "windows")]
        assert_eq!(resolved, "jdwp.dll");
    }

    /// Interpreter round i2 wave 55, lane L1: `-agentlib:<name>` finds the
    /// platform library in the JDK's library directory first, as HotSpot's
    /// `os::dll_locate_lib` does, else names it for the library path; a name
    /// with a separator is still a library name, not a path.
    #[test]
    fn agentlib_looks_in_the_jdks_library_directory_first() {
        let home = std::env::temp_dir().join(format!("cratonvm-agentlib-{}", std::process::id()));
        let dir = home.join(if cfg!(target_os = "windows") {
            "bin"
        } else {
            "lib"
        });
        std::fs::create_dir_all(&dir).expect("a temporary JDK home");
        let file = dir.join(platform_library_name("w55home"));
        std::fs::write(&file, b"not a library").expect("a library file");
        let home_text = home.to_string_lossy().into_owned();
        assert_eq!(
            agentlib_path("w55home", Some(&home_text)),
            file.to_string_lossy()
        );
        assert_eq!(
            agentlib_path("w55absent", Some(&home_text)),
            platform_library_name("w55absent")
        );
        assert_eq!(
            agentlib_path("w55home", None),
            platform_library_name("w55home")
        );
        // Found and loaded from there: not a library, so the loader refuses
        // it at that path.
        let mut registry = AgentRegistry::new();
        let expected = file.to_string_lossy().into_owned();
        let err = registry
            .load_startup_agent("w55home", None, true, Some(&home_text))
            .expect_err("not a library");
        assert!(
            matches!(&err, JvmtiError::AgentLibraryLoadFailed { path, .. } if *path == expected),
            "{err:?}"
        );
        // `-agentpath:` loads the name as given.
        let err = registry
            .load_startup_agent("w55-relative.so", None, false, Some(&home_text))
            .expect_err("no such library");
        assert!(
            matches!(&err, JvmtiError::AgentLibraryLoadFailed { path, .. } if path == "w55-relative.so"),
            "{err:?}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn test_agent_registry_default() {
        let registry = AgentRegistry::default();
        assert!(registry.agents().is_empty());
    }

    #[test]
    fn test_nonexistent_library_fails_closed() {
        let mut registry = AgentRegistry::new();
        let result = registry.load_agent("nonexistent_agent_lib_xyz", Some("opts"));
        assert!(matches!(
            result,
            Err(JvmtiError::AgentLibraryLoadFailed { .. })
        ));
        assert!(registry.agents().is_empty());
    }

    #[test]
    fn test_unload_all_empty() {
        let mut registry = AgentRegistry::new();
        // Should not panic on empty registry
        registry.unload_all();
    }

    #[test]
    fn test_agent_info_clone() {
        let info = AgentInfo {
            name: "test".to_string(),
            options: "opt".to_string(),
            phase: AgentPhase::OnLoad,
            library: AgentLibrary::empty(),
            on_load_called: true,
        };
        let cloned = info.clone();
        assert_eq!(cloned.name, "test");
        assert_eq!(cloned.phase, AgentPhase::OnLoad);
        assert!(cloned.on_load_called);
        // Clone should have an empty library handle
        assert!(!cloned.library.is_loaded());
    }

    /// T2.9.20 — End-to-end JNI agent loading pipeline test.
    ///
    /// Since `-agentlib:hprof` is built into modern JVMs (not a separate
    /// shared library), this test verifies parsing, platform resolution, and
    /// fail-closed handling when a requested agent .so/.dll is not present on
    /// the host.
    #[test]
    fn t2_9_20_agent_loading_pipeline_end_to_end() {
        // 1. Parse all three agent argument styles
        let (name1, opts1) = parse_agent_arg("-agentlib:test_agent=trace=5");
        assert_eq!(name1, "test_agent");
        assert_eq!(opts1.as_deref(), Some("trace=5"));

        let (name2, opts2) = parse_agent_arg("-agentpath:/opt/agents/custom.so=debug");
        assert_eq!(name2, "/opt/agents/custom.so");
        assert_eq!(opts2.as_deref(), Some("debug"));

        let (name3, opts3) = parse_agent_arg("-javaagent:myagent.jar=arg1,arg2");
        assert_eq!(name3, "myagent.jar");
        assert_eq!(opts3.as_deref(), Some("arg1,arg2"));

        // 2. Resolve agent paths (platform-aware)
        let resolved = resolve_agent_path("hprof");
        #[cfg(target_os = "windows")]
        assert_eq!(resolved, "hprof.dll");
        #[cfg(target_os = "linux")]
        assert_eq!(resolved, "libhprof.so");
        #[cfg(target_os = "macos")]
        assert_eq!(resolved, "libhprof.dylib");

        // 3. Missing native libraries fail closed and leave no metadata behind.
        let mut registry = AgentRegistry::new();
        let err = registry
            .load_agent("agent_alpha", Some("verbose"))
            .expect_err("missing OnLoad agent must fail");
        assert!(matches!(err, JvmtiError::AgentLibraryLoadFailed { .. }));
        assert!(registry.agents().is_empty());

        // Live phase agents use the same fail-closed contract.
        let err = registry
            .load_agent_live("dynamic_agent", "monitor", std::ptr::null_mut())
            .expect_err("missing live agent must fail");
        assert!(matches!(err, JvmtiError::AgentLibraryLoadFailed { .. }));
        assert!(registry.agents().is_empty());

        // 4. Unload remains safe on an empty registry.
        registry.unload_all();
        assert!(registry.agents().is_empty());
    }
}
