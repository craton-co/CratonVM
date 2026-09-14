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
pub struct AgentRegistry {
    agents: Vec<AgentInfo>,
}

impl AgentRegistry {
    /// Create a new, empty agent registry.
    pub fn new() -> Self {
        Self { agents: Vec::new() }
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
    pub fn load_agent(&mut self, path: &str, options: &str) -> Result<(), JvmtiError> {
        self.load_agent_for_phase(path, options, AgentPhase::OnLoad, std::ptr::null_mut())
    }

    /// Load an agent via the Attach API (live phase).
    pub fn load_agent_live(
        &mut self,
        path: &str,
        options: &str,
        vm_ptr: *mut std::os::raw::c_void,
    ) -> Result<(), JvmtiError> {
        self.load_agent_for_phase(path, options, AgentPhase::Live, vm_ptr)
    }

    fn load_agent_for_phase(
        &mut self,
        path: &str,
        options: &str,
        phase: AgentPhase,
        vm_ptr: *mut std::os::raw::c_void,
    ) -> Result<(), JvmtiError> {
        if path.is_empty() {
            return Err(JvmtiError::Internal);
        }

        let resolved_path = resolve_agent_path(path);
        let mut info = AgentInfo {
            name: path.to_string(),
            options: options.to_string(),
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

        let entry_name: &[u8] = match phase {
            AgentPhase::OnLoad => b"Agent_OnLoad",
            AgentPhase::Live => b"Agent_OnAttach",
        };
        let entry_symbol = std::str::from_utf8(entry_name).unwrap_or("?").to_string();

        let options_cstring =
            std::ffi::CString::new(options).map_err(|err| JvmtiError::InvalidAgentOptions {
                path: resolved_path.clone(),
                cause: err.to_string(),
            })?;

        let code = match phase {
            AgentPhase::OnLoad => unsafe {
                let entry = lib.get::<AgentOnLoadFn>(entry_name).map_err(|_| {
                    JvmtiError::AgentEntryPointMissing {
                        path: resolved_path.clone(),
                        symbol: entry_symbol.clone(),
                    }
                })?;
                entry(vm_ptr, options_cstring.as_ptr(), std::ptr::null_mut())
            },
            AgentPhase::Live => unsafe {
                let entry = lib.get::<AgentOnAttachFn>(entry_name).map_err(|_| {
                    JvmtiError::AgentEntryPointMissing {
                        path: resolved_path.clone(),
                        symbol: entry_symbol.clone(),
                    }
                })?;
                entry(vm_ptr, options_cstring.as_ptr(), std::ptr::null_mut())
            },
        };

        if code != 0 {
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

    /// Call `Agent_OnUnload` for all agents that have loaded libraries.
    ///
    /// The libraries themselves are **not** unloaded (matching HotSpot behaviour:
    /// agent shared libraries remain mapped for the lifetime of the process).
    pub fn unload_all(&mut self) {
        for agent in &mut self.agents {
            if let Some(ref lib) = agent.library.library {
                let sym: Result<libloading::Symbol<'_, AgentOnUnloadFn>, _> =
                    unsafe { lib.get(b"Agent_OnUnload") };
                if let Ok(f) = sym {
                    unsafe { f(std::ptr::null_mut()) };
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

    #[cfg(target_os = "linux")]
    {
        format!("lib{}.so", path)
    }
    #[cfg(target_os = "macos")]
    {
        format!("lib{}.dylib", path)
    }
    #[cfg(target_os = "windows")]
    {
        format!("{}.dll", path)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        path.to_string()
    }
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
pub fn parse_agent_arg(arg: &str) -> (String, String) {
    let prefixes = ["-agentlib:", "-agentpath:", "-javaagent:"];

    for prefix in &prefixes {
        if let Some(rest) = arg.strip_prefix(prefix) {
            return if let Some((name, opts)) = rest.split_once('=') {
                (name.to_string(), opts.to_string())
            } else {
                (rest.to_string(), String::new())
            };
        }
    }

    // If no known prefix, treat the whole argument as the name.
    (arg.to_string(), String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_agentlib_with_options() {
        let (name, opts) = parse_agent_arg("-agentlib:jdwp=transport=dt_socket,server=y");
        assert_eq!(name, "jdwp");
        assert_eq!(opts, "transport=dt_socket,server=y");
    }

    #[test]
    fn test_parse_agentlib_no_options() {
        let (name, opts) = parse_agent_arg("-agentlib:hprof");
        assert_eq!(name, "hprof");
        assert!(opts.is_empty());
    }

    #[test]
    fn test_parse_agentpath() {
        let (name, opts) = parse_agent_arg("-agentpath:/usr/lib/agent.so=opt1");
        assert_eq!(name, "/usr/lib/agent.so");
        assert_eq!(opts, "opt1");
    }

    #[test]
    fn test_parse_javaagent() {
        let (name, opts) = parse_agent_arg("-javaagent:myagent.jar=config");
        assert_eq!(name, "myagent.jar");
        assert_eq!(opts, "config");
    }

    #[test]
    fn test_parse_unknown_format() {
        let (name, opts) = parse_agent_arg("something_else");
        assert_eq!(name, "something_else");
        assert!(opts.is_empty());
    }

    #[test]
    fn test_missing_agents_fail_closed_without_registration() {
        let mut registry = AgentRegistry::new();
        let err = registry
            .load_agent("definitely_not_a_real_library_1", "opt1")
            .expect_err("missing startup agent must fail closed");
        assert!(matches!(err, JvmtiError::AgentLibraryLoadFailed { .. }));
        assert!(registry.agents().is_empty());

        let err = registry
            .load_agent("definitely_not_a_real_library_2", "opt2")
            .expect_err("subsequent missing agent must also fail closed");
        assert!(matches!(err, JvmtiError::AgentLibraryLoadFailed { .. }));
        assert!(registry.agents().is_empty());
    }

    #[test]
    fn test_load_agent_empty_path_fails() {
        let mut registry = AgentRegistry::new();
        assert!(registry.load_agent("", "opts").is_err());
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

    #[test]
    fn test_agent_registry_default() {
        let registry = AgentRegistry::default();
        assert!(registry.agents().is_empty());
    }

    #[test]
    fn test_nonexistent_library_fails_closed() {
        let mut registry = AgentRegistry::new();
        let result = registry.load_agent("nonexistent_agent_lib_xyz", "opts");
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
        assert_eq!(opts1, "trace=5");

        let (name2, opts2) = parse_agent_arg("-agentpath:/opt/agents/custom.so=debug");
        assert_eq!(name2, "/opt/agents/custom.so");
        assert_eq!(opts2, "debug");

        let (name3, opts3) = parse_agent_arg("-javaagent:myagent.jar=arg1,arg2");
        assert_eq!(name3, "myagent.jar");
        assert_eq!(opts3, "arg1,arg2");

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
            .load_agent("agent_alpha", "verbose")
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
