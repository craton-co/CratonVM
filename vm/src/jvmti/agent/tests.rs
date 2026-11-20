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
