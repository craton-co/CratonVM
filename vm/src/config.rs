use std::path::PathBuf;

/// Available garbage collector algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcAlgorithm {
    /// Generational semi-space GC (default, current implementation).
    Generational,
    /// G1 (Garbage-First) region-based collector.
    G1,
}

/// Configuration for the JVM instance.
///
/// Mirrors common JVM `-X` flags and provides defaults suitable for development.
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// Maximum heap size in bytes (equivalent to `-Xmx`).
    pub max_heap_size: usize,

    /// Initial heap size in bytes (equivalent to `-Xms`).
    pub initial_heap_size: usize,

    /// Maximum call stack depth per thread (to detect infinite recursion).
    pub max_stack_depth: usize,

    /// The application classpath entries to search for classes (`-classpath`).
    pub classpath: Vec<String>,

    /// The boot classpath entries (rt.jar, etc.). If empty, auto-discovered
    /// from `java_home` or the `JAVA_HOME` environment variable.
    pub boot_classpath: Vec<String>,

    /// The extension classpath entries ($JAVA_HOME/lib/ext). If empty,
    /// auto-discovered from `java_home` or `JAVA_HOME`.
    pub ext_classpath: Vec<String>,

    /// Path to JAVA_HOME. If `None`, read from the `JAVA_HOME` env var.
    pub java_home: Option<String>,

    /// Whether to print verbose class loading info (`-verbose:class`).
    pub verbose_class_loading: bool,

    /// Whether to print verbose GC info (`-verbose:gc`).
    pub verbose_gc: bool,

    /// System properties (`-D` flags).
    pub system_properties: Vec<(String, String)>,

    /// Skip bytecode verification (`-noverify` / `-Xverify:none`).
    pub skip_verification: bool,

    /// Which garbage collector algorithm to use (`-XX:+UseG1GC`, etc.).
    pub gc_algorithm: GcAlgorithm,

    /// Enable compressed object pointers (`-XX:+UseCompressedOops`).
    /// Reduces memory usage by using 32-bit references for heaps < 32 GB.
    pub use_compressed_oops: bool,

    /// Enable compact object headers (`-XX:+UseCompactObjectHeaders`).
    /// Uses 8-byte headers instead of 16/32-byte, reducing per-object overhead.
    pub use_compact_headers: bool,

    /// CDS shared archive file path (`-XX:SharedArchiveFile=`).
    pub shared_archive_file: Option<String>,

    /// CDS sharing mode (`-Xshare:off/on/auto/dump`).
    pub cds_mode: CdsMode,

    /// AOT mode (`-XX:AOTMode=off/training/production`).
    pub aot_mode: AotMode,

    /// AOT cache input path (`-XX:AOTCache=` for production, `-XX:AOTCacheInput=`).
    pub aot_cache_input: Option<String>,

    /// AOT cache output path (`-XX:AOTCacheOutput=`).
    pub aot_cache_output: Option<String>,

    /// Use synthetic JDK stubs instead of real JDK bytecode (`--synthetic-jdk`).
    /// When `true` (default): all ~5,200 native Rust stubs are registered, no real
    /// JDK class files needed. When `false`: only ~300 truly native methods are
    /// registered, and real JDK classes are loaded from JAVA_HOME/jmods.
    pub use_synthetic_jdk: bool,

    /// When enabled, collect a structured audit log of every ACC_NATIVE method
    /// that was invoked but had no Rust implementation registered.
    /// The log is printed on VM shutdown and can be used to identify which native
    /// methods need to be implemented for real-JDK mode.
    pub audit_missing_natives: bool,

    /// JDWP debug server port. If `Some`, a JDWP debug server is started on
    /// the given port at VM startup, allowing IDE debuggers to attach.
    pub jdwp_port: Option<u16>,

    /// Whether to suspend the VM at startup waiting for a debugger to attach.
    pub jdwp_suspend: bool,

    // -----------------------------------------------------------------------
    // JPMS module system flags (Phase B)
    // -----------------------------------------------------------------------

    /// Extra module-path directories/JARs (`--module-path`).
    pub module_path: Vec<String>,

    /// `--add-reads` directives: `(reader_module, target_module)`.
    /// Parsed from `module=target` strings.
    pub add_reads: Vec<(String, String)>,

    /// `--add-exports` directives: `(module, package, target)`.
    /// Parsed from `module/package=target` strings.
    pub add_exports: Vec<(String, String, String)>,

    /// `--add-opens` directives: `(module, package, target)`.
    /// Parsed from `module/package=target` strings.
    pub add_opens: Vec<(String, String, String)>,

    /// `--add-modules` list of additional root modules to resolve.
    pub add_modules: Vec<String>,

    // -----------------------------------------------------------------------
    // JIT flags
    // -----------------------------------------------------------------------

    /// Aggressive JIT compilation policy.
    ///
    /// When `false` (default), the JIT static skip list applies broad
    /// blanket bans on `java/util/`, `java/lang/`, `rustjvm/Tck*`, and
    /// `rustjvm/*` classes — these correspond to known JIT correctness gaps
    /// (instanceof codegen, GC stack maps, hash-table-loop miscompiles).
    ///
    /// When `true`, the blanket package bans are lifted and the JIT will
    /// attempt to compile every method that is not explicitly known to crash
    /// (`<clinit>`, `<init>`, interface defaults, finalizer-bearing classes).
    /// This is intended for development to surface latent JIT bugs and for
    /// benchmarking the maximal reachable code path.
    ///
    /// See `vm/src/jit/skip_list.rs` for the full policy mapping.
    pub jit_aggressive_compilation: bool,

    /// T1.7.7 — `-XX:+HeapDumpOnOutOfMemoryError`. When `true` and an
    /// `OutOfMemoryError` propagates out of the heap allocator, the
    /// VM writes an HPROF dump to `heap_dump_path` (or
    /// `./java_pid<pid>.hprof` if unset) before raising the Java
    /// exception. Off by default — matches HotSpot.
    pub heap_dump_on_oom: bool,

    /// T1.7.7 — `-XX:HeapDumpPath=...` companion. Path to write the
    /// HPROF dump on OOM. `None` defaults to
    /// `./java_pid<pid>.hprof` in the current working directory.
    pub heap_dump_path: Option<String>,

    /// Enable container/cgroup support (`-XX:+UseContainerSupport`).
    /// When `true` (default), the JVM reads cgroup v1/v2 limits to
    /// auto-size heap and thread pools inside Docker/Kubernetes.
    pub use_container_support: bool,

    /// Unified logging spec (`-Xlog:...`). When `Some`, the unified
    /// logging framework is initialized at VM startup with the given
    /// HotSpot-style spec string (e.g. `gc*=info:stdout:time,level,tags`).
    pub xlog_spec: Option<String>,

    /// T6.3.3 — JVMTI agents requested on the command line.
    ///
    /// Each entry is a verbatim command-line token of the form
    /// `-agentlib:<lib>[=<opts>]`, `-agentpath:<path>[=<opts>]`, or
    /// `-javaagent:<jarpath>[=<opts>]`. The VM startup path hands each
    /// token to `AgentRegistry::parse_agent_option` which walks the
    /// list in the canonical `Agent_OnLoad` order.
    pub jvmti_agent_options: Vec<String>,
}

/// AOT compilation mode (Project Leyden, JEPs 483/514/515).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AotMode {
    /// AOT disabled (default).
    Off,
    /// Training mode: collect profile data and write AOT cache on shutdown.
    Training,
    /// Production mode: load AOT cache on startup and use pre-compiled code.
    Production,
}

/// CDS (Class Data Sharing) mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CdsMode {
    /// Sharing disabled (default).
    Off,
    /// Sharing enabled; fail if archive not found.
    On,
    /// Sharing enabled if archive available; fall back to classpath if not.
    Auto,
    /// Dump loaded classes to archive file at shutdown.
    Dump,
}

impl Default for VmConfig {
    fn default() -> Self {
        Self {
            max_heap_size: 256 * 1024 * 1024,    // 256 MB
            initial_heap_size: 16 * 1024 * 1024, // 16 MB
            max_stack_depth: std::env::var("RJ_MAX_STACK_DEPTH")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|n| *n >= 64 && *n <= 65536)
                .unwrap_or(1024),
            classpath: Vec::new(),
            boot_classpath: Vec::new(),
            ext_classpath: Vec::new(),
            java_home: None,
            verbose_class_loading: false,
            verbose_gc: false,
            system_properties: Vec::new(),
            skip_verification: false,
            gc_algorithm: GcAlgorithm::Generational,
            use_compressed_oops: false,
            use_compact_headers: false,
            shared_archive_file: None,
            cds_mode: CdsMode::Off,
            aot_mode: AotMode::Off,
            aot_cache_input: None,
            aot_cache_output: None,
            use_synthetic_jdk: true,
            audit_missing_natives: false,
            jdwp_port: None,
            jdwp_suspend: false,
            module_path: Vec::new(),
            add_reads: Vec::new(),
            add_exports: Vec::new(),
            add_opens: Vec::new(),
            add_modules: Vec::new(),
            jit_aggressive_compilation: false,
            heap_dump_on_oom: false,
            heap_dump_path: None,
            use_container_support: true,
            xlog_spec: None,
            jvmti_agent_options: Vec::new(),
        }
    }
}

impl VmConfig {
    pub fn with_audit_missing_natives(mut self, enabled: bool) -> Self {
        self.audit_missing_natives = enabled;
        self
    }

    pub fn with_jdwp(mut self, port: u16, suspend: bool) -> Self {
        self.jdwp_port = Some(port);
        self.jdwp_suspend = suspend;
        self
    }

    pub fn with_container_support(mut self, enabled: bool) -> Self {
        self.use_container_support = enabled;
        self
    }

    pub fn with_xlog_spec(mut self, spec: String) -> Self {
        self.xlog_spec = Some(spec);
        self
    }
}

impl VmConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_heap_size(mut self, size: usize) -> Self {
        self.max_heap_size = size;
        self
    }

    pub fn with_classpath(mut self, classpath: Vec<String>) -> Self {
        self.classpath = classpath;
        self
    }

    pub fn with_boot_classpath(mut self, paths: Vec<String>) -> Self {
        self.boot_classpath = paths;
        self
    }

    pub fn with_ext_classpath(mut self, paths: Vec<String>) -> Self {
        self.ext_classpath = paths;
        self
    }

    pub fn with_java_home(mut self, java_home: String) -> Self {
        self.java_home = Some(java_home);
        self
    }

    pub fn with_verbose_class_loading(mut self, verbose: bool) -> Self {
        self.verbose_class_loading = verbose;
        self
    }

    pub fn with_verbose_gc(mut self, verbose: bool) -> Self {
        self.verbose_gc = verbose;
        self
    }

    pub fn with_skip_verification(mut self, skip: bool) -> Self {
        self.skip_verification = skip;
        self
    }

    pub fn with_aot_mode(mut self, mode: AotMode) -> Self {
        self.aot_mode = mode;
        self
    }

    pub fn with_aot_cache_input(mut self, path: String) -> Self {
        self.aot_cache_input = Some(path);
        self
    }

    pub fn with_aot_cache_output(mut self, path: String) -> Self {
        self.aot_cache_output = Some(path);
        self
    }

    /// Parse a classpath string using the platform-specific separator.
    pub fn parse_classpath(classpath_str: &str) -> Vec<String> {
        let separator = if cfg!(windows) { ';' } else { ':' };
        classpath_str
            .split(separator)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    }

    // -----------------------------------------------------------------------
    // JPMS flag parsers
    // -----------------------------------------------------------------------

    /// Parse an `--add-reads` value: `"reader_module=target_module"`.
    /// Multiple targets can be comma-separated: `"mod=target1,target2"`.
    pub fn parse_add_reads(s: &str) -> Vec<(String, String)> {
        let mut result = Vec::new();
        if let Some((reader, targets)) = s.split_once('=') {
            for target in targets.split(',') {
                let target = target.trim();
                if !target.is_empty() {
                    result.push((reader.trim().to_string(), target.to_string()));
                }
            }
        }
        result
    }

    /// Parse an `--add-exports` or `--add-opens` value:
    /// `"module/package=target_module"`.
    ///
    /// `target_module` may be `ALL-UNNAMED` (the JDK convention), which we
    /// store as an empty string (our unnamed-module sentinel).
    /// Multiple targets can be comma-separated.
    pub fn parse_add_exports(s: &str) -> Option<(String, String, String)> {
        let (left, target) = s.split_once('=')?;
        let (module, pkg) = left.split_once('/')?;
        let target = if target.trim() == "ALL-UNNAMED" {
            String::new()
        } else {
            target.trim().to_string()
        };
        Some((
            module.trim().to_string(),
            pkg.trim().replace('.', "/"),
            target,
        ))
    }
}

// ---------------------------------------------------------------------------
// Boot / extension classpath auto-discovery
// ---------------------------------------------------------------------------

/// Discover boot classpath entries from JAVA_HOME.
///
/// Supports JDK 8 and earlier: `$JAVA_HOME/lib/rt.jar` (and `jre/lib/rt.jar`
/// for full JDK installs). For JDK 9+, scans `$JAVA_HOME/jmods/` and adds
/// ALL `.jmod` files to the boot classpath, with `java.base.jmod` first.
/// This enables lazy loading of any JDK class on demand.
///
/// Returns an empty `Vec` if JAVA_HOME is not set or doesn't contain the
/// expected files.
pub fn discover_boot_classpath(java_home: Option<&str>) -> Vec<String> {
    let java_home = resolve_java_home(java_home);
    let Some(java_home) = java_home else {
        return Vec::new();
    };

    // JDK 8 and earlier: look for rt.jar
    let rt_candidates = [
        java_home.join("lib").join("rt.jar"),
        java_home.join("jre").join("lib").join("rt.jar"),
    ];

    for candidate in &rt_candidates {
        if candidate.exists() {
            return vec![candidate.to_string_lossy().into_owned()];
        }
    }

    // JDK 9+: load ALL JMOD files from $JAVA_HOME/jmods/ so that any JDK
    // class can be resolved on demand (lazy class loading from JMOD).
    let jmods_dir = java_home.join("jmods");
    if jmods_dir.is_dir() {
        let mut entries = Vec::new();
        // java.base.jmod first — contains java.lang.*, java.util.*, etc.
        // We add it before the directory scan to guarantee it appears first
        // in the classpath (bootstrap ordering).
        let base_jmod = jmods_dir.join("java.base.jmod");
        if base_jmod.exists() {
            entries.push(base_jmod.to_string_lossy().into_owned());
        }
        // Scan all remaining .jmod files so every JDK module is on the
        // boot classpath. This enables lazy loading of ANY JDK class
        // without maintaining a hardcoded module list.
        if let Ok(dir_entries) = std::fs::read_dir(&jmods_dir) {
            let mut additional: Vec<String> = dir_entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let path = e.path();
                    path.extension().is_some_and(|ext| ext == "jmod")
                        && path.file_name() != Some(std::ffi::OsStr::new("java.base.jmod"))
                })
                .map(|e| e.path().to_string_lossy().into_owned())
                .collect();
            // Sort for deterministic classpath ordering across runs
            additional.sort();
            entries.extend(additional);
        }
        if !entries.is_empty() {
            return entries;
        }
    }

    Vec::new()
}

/// Discover extension classpath entries from JAVA_HOME.
///
/// Scans `$JAVA_HOME/lib/ext/` (or `jre/lib/ext/`) for `.jar` files.
pub fn discover_ext_classpath(java_home: Option<&str>) -> Vec<String> {
    let java_home = resolve_java_home(java_home);
    let Some(java_home) = java_home else {
        return Vec::new();
    };

    let candidates = [
        java_home.join("lib").join("ext"),
        java_home.join("jre").join("lib").join("ext"),
    ];

    let dir = candidates.iter().find(|d| d.is_dir());
    let Some(dir) = dir else {
        return Vec::new();
    };

    let mut entries = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(dir) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "jar") {
                entries.push(path.to_string_lossy().into_owned());
            }
        }
    }
    entries.sort(); // deterministic order
    entries
}

/// Public wrapper for `resolve_java_home` — used by integration tests
/// and by the VM initialization code.
pub fn resolve_java_home_public(explicit: Option<&str>) -> Option<PathBuf> {
    resolve_java_home(explicit)
}

/// Resolve the JAVA_HOME path from an explicit value, the environment, or
/// by probing `java` on the system PATH.
///
/// Resolution order:
///   1. Explicit value passed via `--java-home` CLI flag
///   2. `JAVA_HOME` environment variable
///   3. Locate `java` on PATH, then walk up to find the JDK root
///      (handles both `$JDK/bin/java` and symlink wrappers like
///       `C:\Program Files\Common Files\Oracle\Java\javapath\java.exe`)
///
/// Returns `None` if no valid JDK installation can be found.
fn resolve_java_home(explicit: Option<&str>) -> Option<PathBuf> {
    // 1. Explicit value — if provided, it's authoritative.
    //    Don't fall through to env/PATH if the explicit path is invalid.
    if let Some(path) = explicit {
        let p = PathBuf::from(path);
        if p.is_dir() {
            return Some(p);
        }
        // Explicit path was provided but doesn't exist — don't silently
        // use a different JDK installation. Return None.
        return None;
    }

    // 2. JAVA_HOME env var
    if let Ok(val) = std::env::var("JAVA_HOME") {
        let p = PathBuf::from(&val);
        if p.is_dir() {
            return Some(p);
        }
    }

    // 3. Detect from `java` on PATH
    detect_java_home_from_path()
}

/// Attempt to find JAVA_HOME by running `java -XshowSettings` and parsing
/// the `java.home` property from the output.
///
/// This handles cases where JAVA_HOME is not set but `java` is on the
/// system PATH (common on Windows with Oracle JDK installers).
///
/// Tries multiple flag variants for compatibility:
///   - JDK ≤24: `-XshowSettings:property`  (singular)
///   - JDK 25+: `-XshowSettings:properties` (plural)
///   - Fallback: `-XshowSettings:all`
fn detect_java_home_from_path() -> Option<PathBuf> {
    // Try the flag variants in order of specificity
    let flag_variants = [
        "-XshowSettings:properties",  // JDK 25+
        "-XshowSettings:property",    // JDK 9-24
        "-XshowSettings:all",         // universal fallback
    ];

    for flag in &flag_variants {
        let output = std::process::Command::new("java")
            .args([flag, "-version"])
            .output()
            .ok();

        let Some(output) = output else { continue };

        // JDK prints property settings to stderr
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Skip if this variant wasn't recognized (JDK prints "Unrecognized")
        if stderr.contains("Unrecognized") {
            continue;
        }

        for line in stderr.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("java.home") {
                // Format: "java.home = /path/to/jdk"
                if let Some(value) = rest.trim().strip_prefix('=') {
                    let path = PathBuf::from(value.trim());
                    if path.is_dir() {
                        tracing::info!("Auto-detected JAVA_HOME from PATH: {}", path.display());
                        return Some(path);
                    }
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = VmConfig::default();
        assert_eq!(config.max_heap_size, 256 * 1024 * 1024);
        assert_eq!(config.max_stack_depth, 1024);
        assert!(config.classpath.is_empty());
        assert!(config.boot_classpath.is_empty());
        assert!(config.ext_classpath.is_empty());
        assert!(config.java_home.is_none());
    }

    #[test]
    fn parse_classpath_platform() {
        if cfg!(windows) {
            let entries = VmConfig::parse_classpath("lib;classes;.");
            assert_eq!(entries, vec!["lib", "classes", "."]);
        } else {
            let entries = VmConfig::parse_classpath("lib:classes:.");
            assert_eq!(entries, vec!["lib", "classes", "."]);
        }
    }

    #[test]
    fn builder_pattern() {
        let config = VmConfig::new()
            .with_max_heap_size(512 * 1024 * 1024)
            .with_classpath(vec!["lib".to_string()])
            .with_verbose_gc(true);
        assert_eq!(config.max_heap_size, 512 * 1024 * 1024);
        assert_eq!(config.classpath, vec!["lib"]);
        assert!(config.verbose_gc);
    }

    #[test]
    fn builder_boot_classpath() {
        let config = VmConfig::new()
            .with_boot_classpath(vec!["rt.jar".to_string()])
            .with_java_home("/opt/jdk8".to_string());
        assert_eq!(config.boot_classpath, vec!["rt.jar"]);
        assert_eq!(config.java_home.as_deref(), Some("/opt/jdk8"));
    }

    #[test]
    fn discover_boot_classpath_no_java_home() {
        // With a non-existent path, should return empty
        let result = discover_boot_classpath(Some("/nonexistent/path"));
        assert!(result.is_empty());
    }

    #[test]
    fn discover_ext_classpath_no_java_home() {
        let result = discover_ext_classpath(Some("/nonexistent/path"));
        assert!(result.is_empty());
    }

    #[test]
    fn resolve_java_home_nonexistent() {
        assert!(resolve_java_home(Some("/nonexistent/jdk")).is_none());
    }

    #[test]
    fn skip_verification_default_is_false() {
        let config = VmConfig::default();
        assert!(!config.skip_verification);
    }

    #[test]
    fn with_skip_verification() {
        let config = VmConfig::new().with_skip_verification(true);
        assert!(config.skip_verification);
    }

    #[test]
    fn default_stack_depth_is_reasonable() {
        let config = VmConfig::default();
        assert!(config.max_stack_depth >= 64);
        assert!(config.max_stack_depth <= 10000);
    }

    #[test]
    fn parse_classpath_empty_string() {
        let entries = VmConfig::parse_classpath("");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_classpath_single_entry() {
        let entries = VmConfig::parse_classpath("lib");
        assert_eq!(entries, vec!["lib"]);
    }

    #[test]
    fn with_ext_classpath_builder() {
        let config = VmConfig::new().with_ext_classpath(vec!["ext.jar".to_string()]);
        assert_eq!(config.ext_classpath, vec!["ext.jar"]);
    }

    // ── Additional edge case tests ────────────────────────────────────

    #[test]
    fn default_initial_heap_size() {
        let config = VmConfig::default();
        assert_eq!(config.initial_heap_size, 16 * 1024 * 1024);
    }

    #[test]
    fn default_verbose_flags_are_false() {
        let config = VmConfig::default();
        assert!(!config.verbose_class_loading);
        assert!(!config.verbose_gc);
    }

    #[test]
    fn default_system_properties_empty() {
        let config = VmConfig::default();
        assert!(config.system_properties.is_empty());
    }

    #[test]
    fn custom_heap_size_via_builder() {
        let config = VmConfig::new().with_max_heap_size(1024);
        assert_eq!(config.max_heap_size, 1024);
        // Other fields remain default
        assert_eq!(config.initial_heap_size, 16 * 1024 * 1024);
        assert_eq!(config.max_stack_depth, 1024);
    }

    #[test]
    fn heap_size_zero() {
        let config = VmConfig::new().with_max_heap_size(0);
        assert_eq!(config.max_heap_size, 0);
    }

    #[test]
    fn heap_size_very_large() {
        let config = VmConfig::new().with_max_heap_size(usize::MAX);
        assert_eq!(config.max_heap_size, usize::MAX);
    }

    #[test]
    fn parse_classpath_consecutive_separators() {
        // Consecutive separators produce empty strings which should be filtered
        if cfg!(windows) {
            let entries = VmConfig::parse_classpath("lib;;classes");
            assert_eq!(entries, vec!["lib", "classes"]);
        } else {
            let entries = VmConfig::parse_classpath("lib::classes");
            assert_eq!(entries, vec!["lib", "classes"]);
        }
    }

    #[test]
    fn parse_classpath_trailing_separator() {
        if cfg!(windows) {
            let entries = VmConfig::parse_classpath("lib;classes;");
            assert_eq!(entries, vec!["lib", "classes"]);
        } else {
            let entries = VmConfig::parse_classpath("lib:classes:");
            assert_eq!(entries, vec!["lib", "classes"]);
        }
    }

    #[test]
    fn parse_classpath_leading_separator() {
        if cfg!(windows) {
            let entries = VmConfig::parse_classpath(";lib;classes");
            assert_eq!(entries, vec!["lib", "classes"]);
        } else {
            let entries = VmConfig::parse_classpath(":lib:classes");
            assert_eq!(entries, vec!["lib", "classes"]);
        }
    }

    #[test]
    fn builder_chaining_all_options() {
        let config = VmConfig::new()
            .with_max_heap_size(128 * 1024 * 1024)
            .with_classpath(vec!["a.jar".to_string(), "b.jar".to_string()])
            .with_boot_classpath(vec!["rt.jar".to_string()])
            .with_ext_classpath(vec!["ext.jar".to_string()])
            .with_java_home("/usr/lib/jvm/java-8".to_string())
            .with_verbose_class_loading(true)
            .with_verbose_gc(true)
            .with_skip_verification(true);

        assert_eq!(config.max_heap_size, 128 * 1024 * 1024);
        assert_eq!(config.classpath.len(), 2);
        assert_eq!(config.boot_classpath, vec!["rt.jar"]);
        assert_eq!(config.ext_classpath, vec!["ext.jar"]);
        assert_eq!(config.java_home.as_deref(), Some("/usr/lib/jvm/java-8"));
        assert!(config.verbose_class_loading);
        assert!(config.verbose_gc);
        assert!(config.skip_verification);
    }

    #[test]
    fn config_clone_is_independent() {
        let config1 = VmConfig::new().with_max_heap_size(100);
        let mut config2 = config1.clone();
        config2.max_heap_size = 200;
        assert_eq!(config1.max_heap_size, 100);
        assert_eq!(config2.max_heap_size, 200);
    }

    #[test]
    fn config_debug_format() {
        let config = VmConfig::default();
        let debug = format!("{:?}", config);
        assert!(debug.contains("VmConfig"));
        assert!(debug.contains("max_heap_size"));
    }

    // ── AOT configuration tests ────────────────────────��─────────────

    #[test]
    fn default_aot_mode_is_off() {
        let config = VmConfig::default();
        assert_eq!(config.aot_mode, AotMode::Off);
        assert!(config.aot_cache_input.is_none());
        assert!(config.aot_cache_output.is_none());
    }

    #[test]
    fn with_aot_mode_training() {
        let config = VmConfig::new().with_aot_mode(AotMode::Training);
        assert_eq!(config.aot_mode, AotMode::Training);
    }

    #[test]
    fn with_aot_mode_production() {
        let config = VmConfig::new().with_aot_mode(AotMode::Production);
        assert_eq!(config.aot_mode, AotMode::Production);
    }

    #[test]
    fn with_aot_cache_paths() {
        let config = VmConfig::new()
            .with_aot_cache_input("/tmp/cache.aot".to_string())
            .with_aot_cache_output("/tmp/cache_out.aot".to_string());
        assert_eq!(config.aot_cache_input.as_deref(), Some("/tmp/cache.aot"));
        assert_eq!(config.aot_cache_output.as_deref(), Some("/tmp/cache_out.aot"));
    }

    #[test]
    fn aot_builder_chaining() {
        let config = VmConfig::new()
            .with_aot_mode(AotMode::Training)
            .with_aot_cache_output("/tmp/out.aot".to_string())
            .with_max_heap_size(512 * 1024 * 1024);
        assert_eq!(config.aot_mode, AotMode::Training);
        assert_eq!(config.aot_cache_output.as_deref(), Some("/tmp/out.aot"));
        assert_eq!(config.max_heap_size, 512 * 1024 * 1024);
    }

    #[test]
    fn aot_mode_clone_is_independent() {
        let c1 = VmConfig::new().with_aot_mode(AotMode::Training);
        let mut c2 = c1.clone();
        c2.aot_mode = AotMode::Production;
        assert_eq!(c1.aot_mode, AotMode::Training);
        assert_eq!(c2.aot_mode, AotMode::Production);
    }

    // ── Boot classpath auto-discovery integration tests ───────────────
    // These tests require a JDK installation on the host. They are ignored
    // by default so CI without a JDK doesn't fail. Run with:
    //   cargo test -p rustjvm-vm -- --ignored

    /// Helper: find a real JDK installation on this machine, or return None.
    fn find_local_jdk() -> Option<PathBuf> {
        // Check JAVA_HOME first
        if let Ok(val) = std::env::var("JAVA_HOME") {
            let p = PathBuf::from(&val);
            if p.join("jmods").is_dir() {
                return Some(p);
            }
        }
        // Fall back to PATH detection
        detect_java_home_from_path()
    }

    #[test]
    #[ignore] // requires JDK on host
    fn detect_java_home_from_path_finds_jdk() {
        let jdk = detect_java_home_from_path();
        assert!(
            jdk.is_some(),
            "Expected to find JDK via `java` on PATH. \
             Is Java installed and on the system PATH?"
        );
        let jdk = jdk.unwrap();
        assert!(jdk.is_dir(), "java.home should be a directory: {}", jdk.display());
        // JDK 9+ should have a jmods directory
        let jmods = jdk.join("jmods");
        assert!(
            jmods.is_dir() || jdk.join("lib").join("rt.jar").exists(),
            "JDK at {} should have jmods/ (JDK 9+) or lib/rt.jar (JDK 8)",
            jdk.display()
        );
    }

    #[test]
    #[ignore] // requires JDK on host
    fn discover_boot_classpath_finds_jmods() {
        let jdk = find_local_jdk();
        if jdk.is_none() {
            eprintln!("Skipping: no JDK found");
            return;
        }
        let jdk = jdk.unwrap();

        let entries = discover_boot_classpath(Some(jdk.to_str().unwrap()));
        assert!(
            !entries.is_empty(),
            "discover_boot_classpath should find entries for JDK at {}",
            jdk.display()
        );

        // java.base.jmod should be the first entry
        let first = &entries[0];
        assert!(
            first.contains("java.base"),
            "First boot classpath entry should be java.base.jmod, got: {first}"
        );

        // Should have multiple modules
        let jmod_count = entries.iter().filter(|e| e.ends_with(".jmod")).count();
        assert!(
            jmod_count >= 10,
            "Expected at least 10 jmod files, found {jmod_count}"
        );

        eprintln!(
            "Discovered {} boot classpath entries ({} jmods) from {}",
            entries.len(), jmod_count, jdk.display()
        );
    }

    // -----------------------------------------------------------------------
    // Phase B: JPMS CLI parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_add_reads_single() {
        let result = VmConfig::parse_add_reads("modA=modB");
        assert_eq!(result, vec![("modA".to_string(), "modB".to_string())]);
    }

    #[test]
    fn parse_add_reads_multiple_targets() {
        let result = VmConfig::parse_add_reads("modA=modB,modC");
        assert_eq!(result, vec![
            ("modA".to_string(), "modB".to_string()),
            ("modA".to_string(), "modC".to_string()),
        ]);
    }

    #[test]
    fn parse_add_reads_no_equals() {
        let result = VmConfig::parse_add_reads("modA");
        assert!(result.is_empty());
    }

    #[test]
    fn parse_add_exports_basic() {
        let result = VmConfig::parse_add_exports("modA/com.foo=modB");
        assert_eq!(result, Some(("modA".to_string(), "com/foo".to_string(), "modB".to_string())));
    }

    #[test]
    fn parse_add_exports_all_unnamed() {
        let result = VmConfig::parse_add_exports("java.base/java.lang=ALL-UNNAMED");
        assert_eq!(result, Some(("java.base".to_string(), "java/lang".to_string(), String::new())));
    }

    #[test]
    fn parse_add_exports_invalid() {
        assert!(VmConfig::parse_add_exports("garbage").is_none());
        assert!(VmConfig::parse_add_exports("mod=target").is_none()); // missing /package
    }

    #[test]
    #[ignore] // requires JDK on host
    fn resolve_java_home_finds_jdk_without_env_var() {
        // This test verifies the full resolution chain works
        let jdk = resolve_java_home(None);
        // On a machine with Java on PATH but no JAVA_HOME, this should
        // still find the JDK via PATH detection.
        // We can't guarantee this on all CI machines, so just log.
        if let Some(jdk) = jdk {
            eprintln!("resolve_java_home(None) found: {}", jdk.display());
            assert!(jdk.is_dir());
        } else {
            eprintln!(
                "resolve_java_home(None) returned None — \
                 no JAVA_HOME env and no java on PATH"
            );
        }
    }
}
