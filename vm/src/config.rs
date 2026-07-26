// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use std::path::{Path, PathBuf};

/// Available garbage collector algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcAlgorithm {
    /// Generational semi-space GC (default, current implementation).
    Generational,
    /// G1 (Garbage-First) region-based collector.
    G1,
    /// ZGC-real backend: a memory-backed stop-the-world mark-sweep collector.
    #[cfg(feature = "zgc")]
    Zgc,
}

/// Map a garbage-collector selector name to a supported [`GcAlgorithm`].
///
/// The input is the collector identifier from a HotSpot `-XX:+Use<name>GC`
/// flag with the `Use`/`GC` wrapper already stripped (e.g. `"G1"`,
/// `"Generational"`), matched case-insensitively with surrounding whitespace
/// trimmed. Returns:
///
/// - `Some(GcAlgorithm::G1)` for `g1`,
/// - `Some(GcAlgorithm::Generational)` for `generational`,
/// - `None` for collectors CratonVM does not implement (`Serial`, `Parallel`,
///   `Shenandoah`, `Epsilon`) or any unrecognized name.
///
/// When the `zgc` cargo feature is enabled, `z` and `zgc` also map to the
/// memory-backed [`GcAlgorithm::Zgc`] backend.
///
/// On `None` the launcher warns and falls back to the default `Generational`
/// collector. HotSpot instead errors on an unknown `-XX:+Use*GC`; CratonVM is
/// deliberately lenient so a `java` drop-in keeps booting — see
/// `docs/feature-designs/concurrent-gc-maturation.md` §3.1.
pub fn parse_gc_algorithm(name: &str) -> Option<GcAlgorithm> {
    match name.trim().to_ascii_lowercase().as_str() {
        "g1" => Some(GcAlgorithm::G1),
        "generational" => Some(GcAlgorithm::Generational),
        #[cfg(feature = "zgc")]
        "z" | "zgc" => Some(GcAlgorithm::Zgc),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// JDK mode — which standard-library implementation the VM runs against
// ---------------------------------------------------------------------------

/// Which Java standard-library implementation a VM instance runs against.
///
/// CratonVM ships **two complete, materially different** class-library
/// surfaces:
///
/// * [`JdkMode::Real`] — real JDK class files are loaded from
///   `$JAVA_HOME/jmods/*.jmod` (JDK 9+) or the `lib/modules` jimage
///   (JRE / `jlink` image), and only the ~300 truly-native methods are
///   implemented in Rust.
/// * [`JdkMode::Synthetic`] — no JDK is needed at all; ~5,200 native Rust
///   stubs stand in for the class library.
///
/// The two have different semantics, different performance envelopes, and
/// **different bug sets**. A bug report is uninterpretable without knowing
/// which one ran, so the mode is reported in the `-version` /
/// `-Xinternalversion` banner and in the CLI's fatal-error output.
///
/// # Determinism contract (2026-07-26)
///
/// The mode is **never** inferred from what happens to be installed on the
/// host machine. Before this change the launcher ran
/// `use_synthetic_jdk = detect_real_jdk().is_none()`, so the *same binary
/// and the same command line* ran a different standard library depending on
/// whether a JDK happened to be on `PATH`. That is gone. Both entry points
/// now have a fixed, documented default:
///
/// | entry point | default | constant |
/// |---|---|---|
/// | `cratonvm` launcher (`VmConfig::for_launcher`) | real-JDK | [`LAUNCHER_DEFAULT_JDK_MODE`] |
/// | embedding / test path (`VmConfig::default`) | synthetic | [`EMBEDDED_DEFAULT_JDK_MODE`] |
///
/// and the only way to change it is an explicit request
/// (`--real-jdk` / `--synthetic-jdk`, or [`VmConfig::with_jdk_mode`]).
/// [`detect_real_jdk`] survives, but strictly as *validation*
/// ([`require_real_jdk`]) — never as selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JdkMode {
    /// Boot against real JDK bytecode from `jmods/` or the `lib/modules`
    /// jimage. Only the ~300 truly-native methods are Rust. Requires a
    /// usable JDK on the host; see [`require_real_jdk`].
    Real,
    /// Boot against the ~5,200 synthetic Rust stubs in `native-builtins`.
    /// Needs no JDK on the host, but the class library is CratonVM's own
    /// re-implementation, not the JDK's.
    ///
    /// Only usable when the crate was built with the `synthetic-jdk`
    /// Cargo feature — see [`SYNTHETIC_JDK_COMPILED_IN`].
    Synthetic,
}

impl JdkMode {
    /// Stable, machine-greppable spelling used in `-version` output, log
    /// lines, and bug reports. Matches the CLI flag spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            JdkMode::Real => "real-jdk",
            JdkMode::Synthetic => "synthetic-jdk",
        }
    }

    /// One-line human description of what the mode actually does, for the
    /// `-version` banner.
    pub fn describe(self) -> &'static str {
        match self {
            JdkMode::Real => {
                "real JDK class files from jmods/ or lib/modules; ~300 native methods in Rust"
            }
            JdkMode::Synthetic => {
                "~5,200 synthetic Rust stubs from native-builtins; no JDK required"
            }
        }
    }

    /// The CLI flag that selects this mode.
    pub fn selecting_flag(self) -> &'static str {
        match self {
            JdkMode::Real => "--real-jdk",
            JdkMode::Synthetic => "--synthetic-jdk",
        }
    }

    /// Bridge to the legacy [`VmConfig::use_synthetic_jdk`] boolean, which
    /// remains the field the VM bootstrap actually reads.
    pub fn use_synthetic_jdk(self) -> bool {
        matches!(self, JdkMode::Synthetic)
    }

    /// Inverse of [`Self::use_synthetic_jdk`].
    pub fn from_use_synthetic_jdk(synthetic: bool) -> Self {
        if synthetic {
            JdkMode::Synthetic
        } else {
            JdkMode::Real
        }
    }
}

impl std::fmt::Display for JdkMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `cratonvm` launcher's fixed default mode.
///
/// Real-JDK is the intended production mode — see the `NEW-11` comment in
/// `vm/Cargo.toml`, which keeps `synthetic-jdk` out of the default feature
/// set precisely because "the default build boots against real JDK
/// bytecode". The launcher default now matches that statement instead of
/// contradicting it on JDK-less machines.
pub const LAUNCHER_DEFAULT_JDK_MODE: JdkMode = JdkMode::Real;

/// The embedding / in-tree-test default mode, used by
/// [`VmConfig::default`].
///
/// This deliberately differs from [`LAUNCHER_DEFAULT_JDK_MODE`]: the
/// library path must stay hermetic so the ~5,000-test in-tree suite and
/// embedders that ship no JDK do not start resolving JMODs from whatever
/// JDK the build machine has. The difference is *declared here* rather
/// than being an emergent property of two unrelated code paths — see
/// `docs/internal/arch-2026-07-26/jdk-mode-determinism.md`.
pub const EMBEDDED_DEFAULT_JDK_MODE: JdkMode = JdkMode::Synthetic;

/// Whether this build actually contains the synthetic class library.
///
/// The ~5,200 stubs are registered inside `#[cfg(feature = "synthetic-jdk")]`
/// blocks in `vm/src/vm/vm_init.rs`. The default Cargo feature set does
/// **not** enable `synthetic-jdk`, so in a default build asking for
/// [`JdkMode::Synthetic`] at runtime yields neither the stubs *nor* a boot
/// classpath — a silently broken VM. Callers must check this constant (or
/// call [`require_synthetic_jdk`]) before honouring a synthetic request.
pub const SYNTHETIC_JDK_COMPILED_IN: bool = cfg!(feature = "synthetic-jdk");

/// Configuration for the JVM instance.
///
/// Mirrors common JVM `-X` flags and provides defaults suitable for development.
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// Maximum heap size in bytes (equivalent to `-Xmx`).
    pub max_heap_size: usize,

    /// Maximum direct (off-heap NIO) buffer memory in bytes, equivalent to
    /// `-XX:MaxDirectMemorySize`. `None` means the flag was not passed
    /// explicitly; real JDK then defaults the cap to `-Xmx` (`max_heap_size`),
    /// and `vm_init` resolves it the same way when wiring up
    /// `native_io::direct_buffer`'s accounting. See
    /// docs/known-issues/h2-suite-bugs/bug-h2-largeblob-direct-memory-oom.md —
    /// previously this cap was a hardcoded 256 MiB regardless of `-Xmx`,
    /// which OOM'd direct-buffer-heavy workloads (H2 MVStore chunk writes)
    /// that HotSpot handles fine at the same `-Xmx`.
    pub max_direct_memory_size: Option<usize>,

    /// Initial heap size in bytes (equivalent to `-Xms`).
    pub initial_heap_size: usize,

    /// Maximum call stack depth per thread (to detect infinite recursion).
    pub max_stack_depth: usize,

    /// The application classpath entries to search for classes (`-classpath`).
    pub classpath: Vec<String>,

    /// When the VM was launched with `-jar <FILE>`, the user-supplied jar
    /// path as a single string. The full search classpath (this jar plus
    /// any manifest `Class-Path` entries) is still recorded in
    /// [`Self::classpath`] so the class loader can resolve sibling jars,
    /// but the `java.class.path` system property is set to *only* this
    /// value — matching the HotSpot contract that a `-jar` launch reports
    /// the bare jar path, not the resolved transitive classpath.
    ///
    /// Liberty/Quarkus boot launchers (e.g.
    /// `com.ibm.ws.kernel.boot.cmdline.UtilityMain`) call
    /// `new JarFile(new File(System.getProperty("java.class.path")))`
    /// and then `getManifest().getMainAttributes()`. If `java.class.path`
    /// were a `;`-joined list, `new File(list)` would not point at a
    /// real jar, `getManifest()` would return null, and the
    /// `NullPointerException: Cannot invoke getMainAttributes on null`
    /// seen in 21 wlp tool jars would fire.
    pub launcher_jar: Option<String>,

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
    ///
    /// This boolean stays as the verifier-dispatcher's read path for
    /// backward compatibility; `xverify_mode` below carries the richer
    /// `-Xverify:none|remote|all` selection. The two are kept in sync at
    /// CLI parse time.
    pub skip_verification: bool,

    /// `-Xverify:none|remote|all` selection. See [`XverifyMode`].
    pub xverify_mode: XverifyMode,

    /// Which garbage collector algorithm to use (`-XX:+UseG1GC`, etc.).
    pub gc_algorithm: GcAlgorithm,

    /// G1 tuning overrides (only honoured when `gc_algorithm == G1`). `None`
    /// keeps the collector default. Wired from the corresponding `-XX:` knobs.
    /// `-XX:InitiatingHeapOccupancyPercent=<n>` — start concurrent marking when
    /// old-gen occupancy crosses this percent.
    pub g1_ihop_percent: Option<u8>,
    /// `-XX:G1HeapRegionSize=<bytes>` — G1 region size.
    pub g1_region_size: Option<usize>,
    /// `-XX:MaxGCPauseMillis=<n>` — target max pause (mixed-CSet sizing).
    pub g1_max_gc_pause_ms: Option<u64>,
    /// `-XX:±UseStringDeduplication` — G1 String backing-array dedup.
    pub g1_string_dedup: Option<bool>,

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

    /// Use synthetic JDK stubs instead of real JDK bytecode.
    ///
    /// This is the raw boolean the VM bootstrap reads; [`JdkMode`] is the
    /// named form and [`VmConfig::jdk_mode`] / [`VmConfig::with_jdk_mode`]
    /// the preferred accessors.
    ///
    /// * `true` ([`JdkMode::Synthetic`]) — all ~5,200 native Rust stubs are
    ///   registered and no real JDK class files are needed. Requires the
    ///   `synthetic-jdk` Cargo feature ([`SYNTHETIC_JDK_COMPILED_IN`]);
    ///   without it the registration blocks in `vm_init.rs` are compiled
    ///   out AND boot-classpath discovery is skipped, leaving a VM with
    ///   neither class library.
    /// * `false` ([`JdkMode::Real`]) — only the ~300 truly-native methods
    ///   are registered and real JDK classes load from
    ///   `$JAVA_HOME/jmods` (or the `lib/modules` jimage).
    ///
    /// # This value is never host-derived
    ///
    /// Until 2026-07-26 the launcher set this from
    /// `detect_real_jdk().is_none()`, so the same binary ran a different
    /// standard library — with a different bug set — depending on whether
    /// a JDK happened to be installed, and nothing in the VM's output said
    /// which. That autodetection is gone. There are exactly two fixed
    /// defaults, [`LAUNCHER_DEFAULT_JDK_MODE`] (real) for
    /// [`VmConfig::for_launcher`] and [`EMBEDDED_DEFAULT_JDK_MODE`]
    /// (synthetic) for [`VmConfig::default`]; anything else is an explicit
    /// `--real-jdk` / `--synthetic-jdk` / [`VmConfig::with_jdk_mode`]
    /// request. When real-JDK mode is selected and no usable JDK is found,
    /// [`require_real_jdk`] produces a hard error instead of a silent
    /// downgrade to synthetic.
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
    /// blanket bans on `java/util/`, `java/lang/`, `cratonvm/Tck*`, and
    /// `cratonvm/*` classes — these correspond to known JIT correctness gaps
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

    /// Effective processor count to report from `Runtime.availableProcessors()`
    /// and the JMX `OperatingSystemMXBean`, derived from the cgroup CPU
    /// quota/period when running under `-XX:+UseContainerSupport`.
    ///
    /// `None` (the default) means "report the host hardware thread count".
    /// The launcher sets this from `container::detect_container()` only when
    /// container support is enabled, so the `-XX:-UseContainerSupport` toggle is
    /// honored transitively (disabled → left `None` → host count). Embedding-API
    /// callers may set it directly to pin a count.
    pub container_effective_processors: Option<u32>,

    /// HotSpot `-XX:±ShowCodeDetailsInExceptionMessages` (JEP 358). When
    /// `true`, the interpreter routes the *non-invoke* null-deref opcodes
    /// (getfield/putfield/arraylength/array-access/monitor/athrow) through the
    /// JEP-358 `Cannot ... because "<expr>" is null` helper. Defaults to `true`
    /// here, matching HotSpot's own default, now that the differential
    /// compliance run confirmed the messages are byte-identical to HotSpot's
    /// `getExtendedNPEMessage` (see `docs/feature-designs/jep358-helpful-npe.md`,
    /// Increment 5). Pass `-XX:-ShowCodeDetailsInExceptionMessages` to opt out;
    /// the increment-1 invoke-site message is unconditionally on regardless. The
    /// `CRATONVM_HELPFUL_NPE_OPCODES` env var, when set, overrides this flag.
    /// See [`crate::runtime::env_cache::helpful_npe_opcodes`].
    pub show_code_details_in_exception_messages: bool,

    /// Unified logging spec (`-Xlog:...`). When `Some`, the unified
    /// logging framework is initialized at VM startup with the given
    /// HotSpot-style spec string (e.g. `gc*=info:stdout:time,level,tags`).
    pub xlog_spec: Option<String>,

    /// T6.3.3 — JVMTI agents requested on the command line.
    ///
    /// Each entry is a verbatim command-line token of the form
    /// `-agentlib:<lib>[=<opts>]` or `-agentpath:<path>[=<opts>]`. The VM
    /// startup path loads each native library and calls `Agent_OnLoad` in
    /// declaration order. Java instrumentation agents use
    /// `runtime::agent_loader::parse_javaagent_spec` plus `invoke_premains`
    /// after VM bootstrap.
    pub jvmti_agent_options: Vec<String>,

    // -----------------------------------------------------------------------
    // GPU offload (Part E of the GPU offload plan)
    // -----------------------------------------------------------------------
    // Every field below is gated behind the `gpu-offload` Cargo feature.
    // With the feature off the struct has the same shape (and the same
    // default-constructed bit pattern) as before the GPU work landed.
    /// Master switch — when `true` and a CUDA driver is available, eligible
    /// static methods are offloaded to the GPU. Default `false`.
    #[cfg(feature = "gpu-offload")]
    pub gpu_offload_enabled: bool,

    /// Ordinal of the CUDA device to use. Default `0`.
    #[cfg(feature = "gpu-offload")]
    pub gpu_device_ordinal: u32,

    /// Minimum `estimated_work` (from the analyzer) before we pay the
    /// upload/launch/download overhead. Default `4096`.
    #[cfg(feature = "gpu-offload")]
    pub gpu_min_work: u32,

    /// Log one `tracing::info!` line per analyzer verdict so a developer
    /// can see why a candidate method was or wasn't offloaded. Default
    /// `false`.
    #[cfg(feature = "gpu-offload")]
    pub print_gpu_decisions: bool,
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

/// `-Xverify:none|remote|all` policy.
///
/// The HotSpot semantics:
///   - **None**   — skip Pass 3 (linking/typestate) verification entirely.
///                  Equivalent to the legacy `-noverify`.
///   - **Remote** — verify everything *except* boot classes (`java/`, `jdk/`,
///                  `sun/`, `com/sun/`). This is HotSpot's default.
///   - **All**    — verify boot classes too. Useful for compliance testing.
///
/// cratonvm currently runs Pass 2 (structural) on every class and Pass 3
/// (typestate) on non-boot classes by default; selecting `All` is honoured
/// by the verifier dispatcher in `vm/src/vm/vm_util.rs`. `None` is wired
/// through `skip_verification` for backward compatibility with the existing
/// `--noverify` CLI flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum XverifyMode {
    /// Equivalent to `-Xverify:none` / `-noverify`.
    None,
    /// HotSpot default — verify non-boot classes only.
    #[default]
    Remote,
    /// `-Xverify:all` — verify boot classes too.
    All,
}

impl XverifyMode {
    /// Parse the `-Xverify:<mode>` argument value. Returns `None` for an
    /// unrecognised mode so the caller can issue a warning and fall back
    /// to the default.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "remote" => Some(Self::Remote),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// True when this mode skips Pass 3 verification entirely.
    pub fn skips_verification(self) -> bool {
        matches!(self, Self::None)
    }
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
            max_direct_memory_size: None,
            initial_heap_size: 16 * 1024 * 1024, // 16 MB
            // Default raised from 1024 to 8192 (root-cause fix for
            // DefaultListableBeanFactoryTests.extensiveCircularReference):
            // real JDK's default `-Xss` comfortably supports several thousand
            // to tens of thousands of frames for typical (non-huge-frame)
            // methods, but this JVM-level frame-count guard was hardcoded far
            // below that regardless of the actual native stack available.
            // Spring's `preInstantiateSingletons()` over a long chain of
            // circularly-referencing beans (bean0->bean1->...->bean99->bean0)
            // recurses roughly 10 Java frames deep per bean through
            // `getBean`/`doGetBean`/`createBean`/`populateBean`/
            // `resolveReference`, so ~99 beans in the cycle landed right at
            // the 1024-frame ceiling and threw a spurious `StackOverflowError`
            // (masked by `BeanCreationException` wrapping) where HotSpot
            // succeeds outright. 8192 stays well under the native-stack-size
            // -derived safety ceilings that actually guard against a hard,
            // uncatchable process abort (see `EXEC_DEPTH_CEILING` /
            // `derive_exec_depth_ceiling` in `runtime/interpreter.rs`, ~8192
            // levels on the 128 MiB main-vm thread, ~512 on an 8 MiB worker
            // carrier) — those ceilings remain the actual backstop on smaller
            // stacks and will trip first there, so raising this default adds
            // no new crash risk.
            max_stack_depth: std::env::var("RJ_MAX_STACK_DEPTH")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|n| *n >= 64 && *n <= 65536)
                .unwrap_or(8192),
            classpath: Vec::new(),
            launcher_jar: None,
            boot_classpath: Vec::new(),
            ext_classpath: Vec::new(),
            java_home: None,
            verbose_class_loading: false,
            verbose_gc: false,
            system_properties: Vec::new(),
            skip_verification: false,
            xverify_mode: XverifyMode::Remote,
            gc_algorithm: GcAlgorithm::Generational,
            g1_ihop_percent: None,
            g1_region_size: None,
            g1_max_gc_pause_ms: None,
            g1_string_dedup: None,
            use_compressed_oops: false,
            use_compact_headers: false,
            shared_archive_file: None,
            cds_mode: CdsMode::Off,
            aot_mode: AotMode::Off,
            aot_cache_input: None,
            aot_cache_output: None,
            // Hermetic embedding/test default. Declared once, as
            // `EMBEDDED_DEFAULT_JDK_MODE`, so the fact that it differs from
            // the launcher default is a stated contract rather than an
            // accident of two code paths. The launcher must NOT use
            // `default()` for this field — it calls `VmConfig::for_launcher`.
            use_synthetic_jdk: EMBEDDED_DEFAULT_JDK_MODE.use_synthetic_jdk(),
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
            container_effective_processors: None,
            // JEP 358: default ON to match HotSpot (messages verified
            // byte-identical). Opt out via -XX:-ShowCodeDetailsInExceptionMessages.
            show_code_details_in_exception_messages: true,
            xlog_spec: None,
            jvmti_agent_options: Vec::new(),
            #[cfg(feature = "gpu-offload")]
            gpu_offload_enabled: false,
            #[cfg(feature = "gpu-offload")]
            gpu_device_ordinal: 0,
            #[cfg(feature = "gpu-offload")]
            gpu_min_work: 4096,
            #[cfg(feature = "gpu-offload")]
            print_gpu_decisions: false,
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

    /// The launcher's starting configuration: identical to
    /// [`VmConfig::default`] except that the JDK mode is
    /// [`LAUNCHER_DEFAULT_JDK_MODE`] (real-JDK).
    ///
    /// Deterministic by construction — it inspects nothing about the host.
    /// If the caller ends up in real-JDK mode it must validate that a
    /// usable JDK exists via [`require_real_jdk`] and fail loudly if not;
    /// this constructor never downgrades to synthetic on its own.
    pub fn for_launcher() -> Self {
        Self::default().with_jdk_mode(LAUNCHER_DEFAULT_JDK_MODE)
    }

    /// Compatibility alias for [`VmConfig::for_launcher`].
    ///
    /// **The name is now a misnomer and the behaviour has changed.** It
    /// used to probe the host (`use_synthetic_jdk = detect_real_jdk()
    /// .is_none()`), which is exactly the silent, host-dependent
    /// divergence this module no longer permits. It is retained only so
    /// out-of-crate callers (`libcratonvm`, `cratonvm-embed` docs) keep
    /// compiling; new code should call [`VmConfig::for_launcher`] and pair
    /// it with [`require_real_jdk`]. See
    /// `docs/internal/arch-2026-07-26/jdk-mode-determinism.md` for the
    /// migration note.
    pub fn with_host_jdk_default() -> Self {
        Self::for_launcher()
    }

    /// The JDK mode this configuration will boot in.
    pub fn jdk_mode(&self) -> JdkMode {
        JdkMode::from_use_synthetic_jdk(self.use_synthetic_jdk)
    }

    /// Explicit, named mode selection. Preferred over
    /// [`Self::with_synthetic_jdk`] because the call site reads as a
    /// choice between two implementations rather than as a boolean.
    pub fn with_jdk_mode(mut self, mode: JdkMode) -> Self {
        self.use_synthetic_jdk = mode.use_synthetic_jdk();
        self
    }

    /// Explicit setter for `use_synthetic_jdk`. Use this in launcher code
    /// when an explicit `--synthetic-jdk` CLI flag has been provided so
    /// the override is visible at the call site.
    pub fn with_synthetic_jdk(mut self, synthetic: bool) -> Self {
        self.use_synthetic_jdk = synthetic;
        self
    }

    pub fn with_max_heap_size(mut self, size: usize) -> Self {
        self.max_heap_size = size;
        self
    }

    pub fn with_classpath(mut self, classpath: Vec<String>) -> Self {
        self.classpath = classpath;
        self
    }

    /// Record the user-supplied `-jar <FILE>` path. See
    /// [`Self::launcher_jar`] for the rationale (HotSpot reports
    /// `-jar <FILE>` as `java.class.path = <FILE>`, not the
    /// resolved transitive manifest classpath).
    pub fn with_launcher_jar(mut self, jar: String) -> Self {
        self.launcher_jar = Some(jar);
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
        if skip {
            self.xverify_mode = XverifyMode::None;
        }
        self
    }

    /// Select the `-Xverify:none|remote|all` policy. Keeps
    /// `skip_verification` in sync with the legacy bool flag.
    pub fn with_xverify_mode(mut self, mode: XverifyMode) -> Self {
        self.xverify_mode = mode;
        self.skip_verification = mode.skips_verification();
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
    ///
    /// On Windows, additionally normalises MSYS/Cygwin/Git-Bash style POSIX
    /// drive paths (`/c/foo/bar.jar`, `/cygdrive/c/foo/bar.jar`) to native
    /// Windows form (`C:/foo/bar.jar`). MSYS bash's automatic path
    /// translation only applies to single-argument paths: when multiple
    /// paths are joined with `;` (the Windows path separator) into one
    /// argument, MSYS leaves them untranslated. Without this normalisation
    /// a user running
    ///   `java.exe -cp "/c/a.jar;/c/b.jar" Main`
    /// from a MinGW/Git-Bash shell would see every classpath entry
    /// silently dropped because `/c/a.jar` does not resolve under the
    /// Win32 file API. A single-jar invocation (`java.exe -cp /c/a.jar`)
    /// would have worked because MSYS translates that single token to
    /// `C:/a.jar` before passing it to the child process.
    pub fn parse_classpath(classpath_str: &str) -> Vec<String> {
        let separator = if cfg!(windows) { ';' } else { ':' };
        classpath_str
            .split(separator)
            .filter(|s| !s.is_empty())
            .map(normalize_classpath_entry)
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

/// Normalise a single classpath entry, converting MSYS/Cygwin/Git-Bash
/// style POSIX drive paths to Windows form on Windows. On non-Windows
/// hosts the entry is returned unchanged.
///
/// Accepted POSIX-style inputs (Windows only):
///   `/c/foo/bar.jar`            -> `C:/foo/bar.jar`
///   `/C/foo/bar.jar`            -> `C:/foo/bar.jar`
///   `/cygdrive/c/foo/bar.jar`   -> `C:/foo/bar.jar`
///
/// Already-native paths (`C:/...`, `C:\...`, relative paths, wildcards
/// like `lib/*`) are returned unchanged.
#[cfg(windows)]
fn normalize_classpath_entry(entry: &str) -> String {
    // `/cygdrive/<letter>/...`
    if let Some(rest) = entry.strip_prefix("/cygdrive/") {
        if let Some(stripped) = posix_drive_tail(rest) {
            return stripped;
        }
    }
    // `/<letter>/...` (MSYS/Git-Bash style). Require exactly:
    // leading slash, single ASCII letter, slash. This avoids touching
    // legitimate Unix-rooted paths like `/etc/...` that some users might
    // place on a Windows classpath as a literal string.
    if let Some(rest) = entry.strip_prefix('/') {
        if let Some(stripped) = posix_drive_tail(rest) {
            return stripped;
        }
    }
    entry.to_string()
}

/// Non-Windows: classpath entries are taken verbatim. Forward-slash
/// POSIX paths are already native here.
#[cfg(not(windows))]
fn normalize_classpath_entry(entry: &str) -> String {
    entry.to_string()
}

/// Given `<letter>/<rest>` or `<letter>` (where `<letter>` is a single
/// ASCII alphabetic character), return `Some("<LETTER>:/<rest>")` or
/// `Some("<LETTER>:/")`. Returns `None` for any other shape so the caller
/// can leave the original entry untouched.
#[cfg(windows)]
fn posix_drive_tail(rest: &str) -> Option<String> {
    let bytes = rest.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
        return None;
    }
    match bytes.get(1) {
        None => Some(format!("{}:/", (bytes[0] as char).to_ascii_uppercase())),
        Some(b'/') => Some(format!(
            "{}:/{}",
            (bytes[0] as char).to_ascii_uppercase(),
            &rest[2..]
        )),
        _ => None,
    }
}

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

    // JRE-style and jlink-trimmed runtime distributions: no `jmods/` directory,
    // but `$JAVA_HOME/lib/modules` is the jimage blob containing every module
    // class. This includes Adoptium/Temurin "JRE" archives, custom jlink
    // images, and any JDK where `jmods/` was deleted to save space. Our reader
    // has jimage support (see classloading/src/class_path.rs::JImageFile);
    // ClassPath::is_likely_jimage detects the file and routes to JImageReader.
    //
    // RKC16N.9: missing this fallback was the root cause of `Void.TYPE` (and
    // every other wrapper TYPE field) being null at boot — pre_init_wrapper
    // skipped them because bootstrap_core_classes saw an empty boot
    // classpath, so wrapper classes never loaded eagerly and Module.<clinit>
    // hit a null at `getstatic Void.TYPE`.
    let lib_modules = java_home.join("lib").join("modules");
    if lib_modules.is_file() {
        return vec![lib_modules.to_string_lossy().into_owned()];
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

/// Probe the host for a real JDK installation suitable for booting
/// `java.base` from JMOD (or `lib/modules`).
///
/// Resolution order matches [`resolve_java_home_public`]:
///   1. `CRATONVM_JAVA_HOME`
///   2. `JAVA_HOME`
///   3. `java` on `PATH` (via `java -XshowSettings:properties`)
///
/// Returns `Some(java_home)` only when the resolved directory actually
/// contains `jmods/java.base.jmod` (JDK 9+) **or** `lib/modules`
/// (JRE / `jlink` image) — i.e. only when the JMOD/jimage boot path is
/// actually loadable. Returns `None` otherwise (notably for a JDK 8-style
/// `lib/rt.jar`-only install: CratonVM has no `rt.jar` boot loader).
///
/// # This is validation, not selection
///
/// This function used to *decide* the boot mode for the `cratonvm`
/// launcher. It no longer does: the mode is fixed by
/// [`LAUNCHER_DEFAULT_JDK_MODE`] / an explicit flag, and this probe only
/// answers "is the mode you asked for actually available?" — see
/// [`require_real_jdk`], which wraps it with an actionable error message.
/// `VmConfig::default()` deliberately does **not** call this, so the
/// embedded library path stays hermetic and tests stay fast.
pub fn detect_real_jdk() -> Option<PathBuf> {
    detect_real_jdk_from(None)
}

/// [`detect_real_jdk`] with an explicit `--java-home` / `VmConfig::java_home`
/// candidate taking priority over the environment, mirroring
/// [`resolve_java_home_public`].
pub fn detect_real_jdk_from(explicit: Option<&str>) -> Option<PathBuf> {
    let java_home = resolve_java_home(explicit)?;
    let jmod = java_home.join("jmods").join("java.base.jmod");
    if jmod.is_file() {
        return Some(java_home);
    }
    let lib_modules = java_home.join("lib").join("modules");
    if lib_modules.is_file() {
        return Some(java_home);
    }
    // The `java` executable was found but neither `jmods/java.base.jmod`
    // nor `lib/modules` is present — e.g. a JDK 8 install with `rt.jar`
    // only, or a broken installation.
    None
}

/// Human-readable account of every location the JDK probe consults, with
/// each one's current value. Used to build the real-JDK-unavailable error
/// so the operator can see *why* the probe failed rather than guessing.
pub fn describe_jdk_search(explicit: Option<&str>) -> String {
    fn env_line(key: &str) -> String {
        match std::env::var(key) {
            Ok(v) if !v.trim().is_empty() => {
                let p = Path::new(v.trim());
                if p.is_dir() {
                    format!("  {key} = {v}  (directory exists)")
                } else {
                    format!("  {key} = {v}  (NOT a directory)")
                }
            }
            _ => format!("  {key} = <unset>"),
        }
    }

    let mut lines = Vec::new();
    match explicit {
        Some(p) if Path::new(p).is_dir() => {
            lines.push(format!("  --java-home = {p}  (directory exists)"));
            lines.push(
                "  (an explicit --java-home is authoritative; the probes below were not consulted)"
                    .to_string(),
            );
            return lines.join("\n");
        }
        Some(p) => {
            lines.push(format!("  --java-home = {p}  (NOT a directory)"));
            lines.push(
                "  (an explicit --java-home is authoritative; the probes below were not consulted)"
                    .to_string(),
            );
            return lines.join("\n");
        }
        None => lines.push("  --java-home = <not passed>".to_string()),
    }
    lines.push(env_line("CRATONVM_JAVA_HOME"));
    lines.push(env_line("JAVA_HOME"));
    match first_java_executable_on_path() {
        Some(p) => lines.push(format!("  java on PATH = {}", p.display())),
        None => lines.push("  java on PATH = <not found>".to_string()),
    }
    lines.join("\n")
}

/// Validate that [`JdkMode::Real`] is actually usable, returning the
/// resolved `JAVA_HOME` on success and a complete, actionable error
/// message on failure.
///
/// **Never falls back to synthetic mode.** The silent fallback is precisely
/// the defect this module exists to remove: it produced runs whose class
/// library — and therefore whose bug set — depended on the host, with
/// nothing in the VM's output to say so. Callers must surface the error.
pub fn require_real_jdk(explicit: Option<&str>) -> Result<PathBuf, String> {
    if let Some(home) = detect_real_jdk_from(explicit) {
        return Ok(home);
    }
    Err(format!(
        "real-JDK mode was selected but no usable JDK was found.\n\
         \n\
         Searched, in order:\n\
         {}\n\
         \n\
         An acceptable JDK root must contain either:\n  \
         * jmods/java.base.jmod   (a full JDK 9+ installation), or\n  \
         * lib/modules            (a JRE or jlink-trimmed runtime image, read via the jimage reader)\n\
         \n\
         A JDK 8-style installation with only lib/rt.jar is NOT accepted: CratonVM has no rt.jar boot loader.\n\
         \n\
         Fix by one of:\n  \
         * set JAVA_HOME (or CRATONVM_JAVA_HOME, which wins over JAVA_HOME) to a JDK 9+ root;\n  \
         * pass --java-home <PATH>;\n  \
         * put a `java` launcher from a JDK 9+ installation on PATH;\n  \
         * or run the other class library explicitly with --synthetic-jdk \
         (requires a build with the `synthetic-jdk` Cargo feature).\n\
         \n\
         CratonVM does not silently substitute the synthetic class library here: \
         the two implementations have different semantics and different bugs, so a \
         run whose library was chosen by the host is not reproducible or reportable.",
        describe_jdk_search(explicit)
    ))
}

/// Validate that [`JdkMode::Synthetic`] is actually usable in this build.
///
/// The ~5,200 stubs live behind `#[cfg(feature = "synthetic-jdk")]`, which
/// is **not** in the default feature set. In a default build, asking for
/// synthetic mode registers no stubs *and* suppresses boot-classpath
/// discovery (`vm_init.rs`: `if config.boot_classpath.is_empty() &&
/// !config.use_synthetic_jdk`), leaving a VM with no class library at all.
/// That must be an error at launch, not a mystery at first class load.
pub fn require_synthetic_jdk() -> Result<(), String> {
    if SYNTHETIC_JDK_COMPILED_IN {
        return Ok(());
    }
    Err("synthetic-JDK mode was selected but this binary was built without the \
         `synthetic-jdk` Cargo feature, so none of the ~5,200 synthetic stubs are \
         compiled in.\n\
         \n\
         Running in this state would give a VM with neither the synthetic class \
         library nor a real-JDK boot classpath (synthetic mode also suppresses \
         boot-classpath discovery), so it is rejected here rather than failing \
         later as an unexplained NoClassDefFoundError.\n\
         \n\
         Fix by one of:\n  \
         * rebuild with `cargo build -p cratonvm-cli --features synthetic-jdk`; or\n  \
         * drop --synthetic-jdk and run the default real-JDK mode."
        .to_string())
}

/// Resolve the JAVA_HOME path from an explicit value, the environment, or
/// by probing `java` on the system PATH.
///
/// Resolution order:
///   1. Explicit value passed via `--java-home` CLI flag
///   2. `CRATONVM_JAVA_HOME` environment variable (real JDK when `JAVA_HOME`
///      is a cratonvm shim directory)
///   3. `JAVA_HOME` environment variable
///   4. Locate `java` on PATH, then walk up to find the JDK root
///      (handles both `$JDK/bin/java` and symlink wrappers like
///       `C:\Program Files\Common Files\Oracle\Java\javapath\java.exe`).
///      Skips this step when the first `java` on PATH is the current
///      executable (cratonvm masquerading as `java.exe`).
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

    // 2. CRATONVM_JAVA_HOME — used when JAVA_HOME points at a cratonvm shim
    // tree (Maven, Gradle) but boot modules must come from a real JDK.
    if let Ok(val) = std::env::var("CRATONVM_JAVA_HOME") {
        let p = PathBuf::from(val.trim());
        if p.is_dir() {
            return Some(p);
        }
    }

    // 3. JAVA_HOME env var
    if let Ok(val) = std::env::var("JAVA_HOME") {
        let p = PathBuf::from(&val);
        if p.is_dir() {
            return Some(p);
        }
    }

    // 4. Detect from `java` on PATH
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
fn first_java_executable_on_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    for dir in std::env::split_paths(&path_var) {
        let candidate = Path::new(&dir).join(exe);
        if candidate.is_file() {
            return std::fs::canonicalize(&candidate)
                .ok()
                .or_else(|| Some(candidate));
        }
    }
    None
}

fn detect_java_home_from_path() -> Option<PathBuf> {
    // If the first `java` on PATH is this process (e.g. cratonvm installed as
    // `java.exe` for Maven Surefire), probing it would recurse or yield a
    // bogus java.home — skip and force explicit JAVA_HOME / CRATONVM_JAVA_HOME.
    if let (Ok(this), Some(first)) = (
        std::env::current_exe().and_then(|p| std::fs::canonicalize(p)),
        first_java_executable_on_path(),
    ) {
        if this == first {
            return None;
        }
    }

    // Try the flag variants in order of specificity
    let flag_variants = [
        "-XshowSettings:properties", // JDK 25+
        "-XshowSettings:property",   // JDK 9-24
        "-XshowSettings:all",        // universal fallback
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
        assert_eq!(config.max_stack_depth, 8192);
        assert!(config.classpath.is_empty());
        assert!(config.boot_classpath.is_empty());
        assert!(config.ext_classpath.is_empty());
        assert!(config.java_home.is_none());
        // Generational is the default and the safety net during G1 maturation.
        assert_eq!(config.gc_algorithm, GcAlgorithm::Generational);
    }

    #[test]
    fn parse_gc_algorithm_supported() {
        assert_eq!(parse_gc_algorithm("g1"), Some(GcAlgorithm::G1));
        assert_eq!(parse_gc_algorithm("G1"), Some(GcAlgorithm::G1));
        #[cfg(feature = "zgc")]
        {
            assert_eq!(parse_gc_algorithm("Z"), Some(GcAlgorithm::Zgc));
            assert_eq!(parse_gc_algorithm("ZGC"), Some(GcAlgorithm::Zgc));
            assert_eq!(parse_gc_algorithm("  zGc  "), Some(GcAlgorithm::Zgc));
        }
        assert_eq!(
            parse_gc_algorithm("Generational"),
            Some(GcAlgorithm::Generational)
        );
        // Case-insensitive + surrounding whitespace tolerated.
        assert_eq!(
            parse_gc_algorithm("  gEnErAtIoNaL  "),
            Some(GcAlgorithm::Generational)
        );
    }

    #[test]
    fn parse_gc_algorithm_unsupported_is_none() {
        // Known HotSpot collectors CratonVM does not implement → None so the
        // launcher warns and falls back to Generational.
        for name in ["Serial", "Parallel", "Shenandoah", "Epsilon"] {
            assert_eq!(
                parse_gc_algorithm(name),
                None,
                "{name} should be unsupported"
            );
        }
        // Garbage / empty input is also None.
        assert_eq!(parse_gc_algorithm("nonsense"), None);
        assert_eq!(parse_gc_algorithm(""), None);
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
        assert_eq!(config.max_stack_depth, 8192);
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

    /// MSYS/Git-Bash on Windows leaves `;`-joined classpaths untranslated,
    /// so `parse_classpath` has to recognise POSIX-drive entries and
    /// convert them. Verified on Windows only — on POSIX hosts the
    /// translation is a no-op and `/c/...` is taken verbatim.
    #[test]
    #[cfg(windows)]
    fn parse_classpath_normalizes_msys_paths() {
        let entries =
            VmConfig::parse_classpath("/c/craton/bootstrap.jar;/c/craton/tomcat-juli.jar");
        assert_eq!(
            entries,
            vec![
                "C:/craton/bootstrap.jar".to_string(),
                "C:/craton/tomcat-juli.jar".to_string(),
            ]
        );
    }

    #[test]
    #[cfg(windows)]
    fn parse_classpath_normalizes_cygdrive_paths() {
        let entries = VmConfig::parse_classpath("/cygdrive/c/a.jar;/cygdrive/d/b.jar");
        assert_eq!(
            entries,
            vec!["C:/a.jar".to_string(), "D:/b.jar".to_string()]
        );
    }

    /// Native Windows entries and relative entries pass through untouched.
    #[test]
    #[cfg(windows)]
    fn parse_classpath_preserves_native_windows_paths() {
        let entries = VmConfig::parse_classpath("C:/foo/a.jar;lib/b.jar;.;C:\\bar\\c.jar");
        assert_eq!(
            entries,
            vec![
                "C:/foo/a.jar".to_string(),
                "lib/b.jar".to_string(),
                ".".to_string(),
                "C:\\bar\\c.jar".to_string(),
            ]
        );
    }

    /// An entry that is just a leading slash followed by a name that
    /// isn't a single drive letter (e.g. `/etc/...`) must not be
    /// rewritten — we don't want to mangle paths that genuinely start
    /// at the filesystem root on some other platform that happens to be
    /// passed through verbatim.
    #[test]
    #[cfg(windows)]
    fn parse_classpath_leaves_non_drive_root_paths_untouched() {
        let entries = VmConfig::parse_classpath("/etc/foo.jar;/usr/lib/bar.jar");
        assert_eq!(
            entries,
            vec!["/etc/foo.jar".to_string(), "/usr/lib/bar.jar".to_string(),]
        );
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
        assert_eq!(
            config.aot_cache_output.as_deref(),
            Some("/tmp/cache_out.aot")
        );
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
    //   cargo test -p cratonvm-vm -- --ignored

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
        assert!(
            jdk.is_dir(),
            "java.home should be a directory: {}",
            jdk.display()
        );
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
            entries.len(),
            jmod_count,
            jdk.display()
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
        assert_eq!(
            result,
            vec![
                ("modA".to_string(), "modB".to_string()),
                ("modA".to_string(), "modC".to_string()),
            ]
        );
    }

    #[test]
    fn parse_add_reads_no_equals() {
        let result = VmConfig::parse_add_reads("modA");
        assert!(result.is_empty());
    }

    #[test]
    fn parse_add_exports_basic() {
        let result = VmConfig::parse_add_exports("modA/com.foo=modB");
        assert_eq!(
            result,
            Some((
                "modA".to_string(),
                "com/foo".to_string(),
                "modB".to_string()
            ))
        );
    }

    #[test]
    fn parse_add_exports_all_unnamed() {
        let result = VmConfig::parse_add_exports("java.base/java.lang=ALL-UNNAMED");
        assert_eq!(
            result,
            Some((
                "java.base".to_string(),
                "java/lang".to_string(),
                String::new()
            ))
        );
    }

    #[test]
    fn parse_add_exports_invalid() {
        assert!(VmConfig::parse_add_exports("garbage").is_none());
        assert!(VmConfig::parse_add_exports("mod=target").is_none()); // missing /package
    }

    // -----------------------------------------------------------------------
    // Real-JDK detection and boot-default decision (task #53)
    // -----------------------------------------------------------------------

    /// Helper: create a fake JDK layout containing `jmods/java.base.jmod`.
    fn fake_jdk_with_jmod(root: &std::path::Path) {
        let jmods = root.join("jmods");
        std::fs::create_dir_all(&jmods).unwrap();
        std::fs::write(jmods.join("java.base.jmod"), b"JM\x01\x00").unwrap();
    }

    /// Helper: create a fake jlink-image layout with `lib/modules`.
    fn fake_jdk_with_lib_modules(root: &std::path::Path) {
        let lib = root.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("modules"), b"jimg\x00\x00\x00\x01").unwrap();
    }

    /// Several tests below drive [`detect_real_jdk`] / [`require_real_jdk`]
    /// by pointing `CRATONVM_JAVA_HOME` at a synthesised JDK tree.
    ///
    /// The `JAVA_HOME` / `CRATONVM_JAVA_HOME` env vars are process-wide,
    /// so this test races with any concurrent test that also touches
    /// them. We isolate by stashing/restoring and serialising via a
    /// static mutex below.
    fn with_env<R>(key: &str, value: Option<&str>, f: impl FnOnce() -> R) -> R {
        let prev = std::env::var_os(key);
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        let result = f();
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        result
    }

    /// Avoid env-var races between the detection tests.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn detect_real_jdk_returns_some_when_java_base_jmod_present() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        fake_jdk_with_jmod(tmp.path());
        with_env(
            "CRATONVM_JAVA_HOME",
            Some(tmp.path().to_str().unwrap()),
            || {
                with_env("JAVA_HOME", None, || {
                    let detected = detect_real_jdk();
                    assert!(
                        detected.is_some(),
                        "detect_real_jdk should find synthetic JDK at {}",
                        tmp.path().display()
                    );
                    assert_eq!(detected.unwrap(), tmp.path());
                });
            },
        );
    }

    #[test]
    fn detect_real_jdk_returns_some_for_lib_modules_image() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        fake_jdk_with_lib_modules(tmp.path());
        with_env(
            "CRATONVM_JAVA_HOME",
            Some(tmp.path().to_str().unwrap()),
            || {
                with_env("JAVA_HOME", None, || {
                    let detected = detect_real_jdk();
                    assert!(
                        detected.is_some(),
                        "detect_real_jdk should accept jlink-image layout"
                    );
                });
            },
        );
    }

    #[test]
    fn detect_real_jdk_returns_none_when_only_rtjar_present() {
        // JDK 8-style: `lib/rt.jar` only. We have no JMOD/jimage loader,
        // so this should NOT count as "real JDK boot available".
        // `CRATONVM_JAVA_HOME` is the highest-priority probe in
        // `resolve_java_home`, so we don't need to scrub `JAVA_HOME` or
        // `PATH` — the synthesised directory wins.
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("rt.jar"), b"PK\x03\x04").unwrap();
        with_env(
            "CRATONVM_JAVA_HOME",
            Some(tmp.path().to_str().unwrap()),
            || {
                assert!(
                    detect_real_jdk().is_none(),
                    "rt.jar-only install must not satisfy detect_real_jdk"
                );
            },
        );
    }

    /// **New contract (2026-07-26).** The launcher default is real-JDK
    /// *unconditionally*. This test previously asserted that
    /// `with_host_jdk_default` flipped the mode when a JDK was detected;
    /// it now asserts the mode does not depend on detection at all. The
    /// JDK-present arm is kept so the two host shapes (JDK present /
    /// JDK absent) are still both exercised — they must agree.
    #[test]
    fn launcher_default_is_real_jdk_when_host_has_a_jdk() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        fake_jdk_with_jmod(tmp.path());
        with_env(
            "CRATONVM_JAVA_HOME",
            Some(tmp.path().to_str().unwrap()),
            || {
                with_env("JAVA_HOME", None, || {
                    let cfg = VmConfig::for_launcher();
                    assert_eq!(cfg.jdk_mode(), JdkMode::Real);
                    assert!(!cfg.use_synthetic_jdk);
                });
            },
        );
    }

    /// The behaviour this file used to encode — "no JDK found ⇒ silently
    /// run the synthetic class library instead" — is now a defect, and
    /// this test pins its absence.
    ///
    /// Using an empty real directory (instead of unsetting `JAVA_HOME`
    /// and emptying `PATH`) avoids racing with the system `java`
    /// binary, which `Command::new("java")` may still resolve on
    /// Windows via `CreateProcess` fallback search paths even with an
    /// empty `PATH`.
    #[test]
    fn launcher_default_never_falls_back_to_synthetic_when_no_jdk() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        // Bare directory: no `jmods/`, no `lib/modules`. `resolve_java_home`
        // will accept it (it's a real directory) but `detect_real_jdk`
        // must reject it because neither boot blob is present.
        with_env(
            "CRATONVM_JAVA_HOME",
            Some(tmp.path().to_str().unwrap()),
            || {
                with_env("JAVA_HOME", None, || {
                    let cfg = VmConfig::for_launcher();
                    assert_eq!(
                        cfg.jdk_mode(),
                        JdkMode::Real,
                        "the launcher default must not depend on whether a JDK exists"
                    );
                    // ...and the unavailability must surface as a hard error.
                    let err = require_real_jdk(None)
                        .expect_err("require_real_jdk must fail when no boot modules exist");
                    assert!(err.contains("no usable JDK was found"), "{err}");
                });
            },
        );
    }

    /// `with_host_jdk_default` is retained only as a compatibility alias
    /// for out-of-crate callers (`libcratonvm`). It must be exactly
    /// `for_launcher()` — in particular it must NOT probe the host.
    #[test]
    fn with_host_jdk_default_is_an_alias_for_for_launcher() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        // Host with no usable JDK: the legacy implementation would have
        // returned synthetic here.
        with_env(
            "CRATONVM_JAVA_HOME",
            Some(tmp.path().to_str().unwrap()),
            || {
                with_env("JAVA_HOME", None, || {
                    assert_eq!(
                        VmConfig::with_host_jdk_default().jdk_mode(),
                        VmConfig::for_launcher().jdk_mode()
                    );
                    assert_eq!(VmConfig::with_host_jdk_default().jdk_mode(), JdkMode::Real);
                });
            },
        );
    }

    /// Explicit `--synthetic-jdk` opt-in must beat the launcher default:
    /// even with a real JDK on the host, an explicit synthetic request is
    /// honoured (subject to `require_synthetic_jdk`, which the CLI checks).
    #[test]
    fn explicit_synthetic_jdk_override_wins_over_launcher_default() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        fake_jdk_with_jmod(tmp.path());
        with_env(
            "CRATONVM_JAVA_HOME",
            Some(tmp.path().to_str().unwrap()),
            || {
                with_env("JAVA_HOME", None, || {
                    let cfg = VmConfig::for_launcher().with_jdk_mode(JdkMode::Synthetic);
                    assert_eq!(cfg.jdk_mode(), JdkMode::Synthetic);
                    assert!(cfg.use_synthetic_jdk);
                    // The legacy boolean setter must stay equivalent.
                    assert_eq!(
                        VmConfig::for_launcher().with_synthetic_jdk(true).jdk_mode(),
                        JdkMode::Synthetic
                    );
                });
            },
        );
    }

    /// `VmConfig::default()` must remain hermetic (synthetic) so library
    /// callers and the ~5000-test suite don't accidentally start
    /// resolving JMODs from the host JDK — and the fact that it differs
    /// from the launcher default must be a *declared* difference.
    #[test]
    fn default_config_stays_synthetic_regardless_of_host() {
        let cfg = VmConfig::default();
        assert_eq!(cfg.jdk_mode(), EMBEDDED_DEFAULT_JDK_MODE);
        assert_eq!(EMBEDDED_DEFAULT_JDK_MODE, JdkMode::Synthetic);
        assert!(
            cfg.use_synthetic_jdk,
            "VmConfig::default() must remain synthetic (hermetic embedding/test path)"
        );
        // The split is intentional and must stay visible: if these two
        // ever converge, delete one of the constants rather than letting
        // callers rediscover the difference by debugging.
        assert_ne!(
            EMBEDDED_DEFAULT_JDK_MODE, LAUNCHER_DEFAULT_JDK_MODE,
            "the launcher and embedding defaults differ by design; see \
             docs/internal/arch-2026-07-26/jdk-mode-determinism.md"
        );
        assert_eq!(LAUNCHER_DEFAULT_JDK_MODE, JdkMode::Real);
    }

    /// The real-JDK-unavailable error must name every location searched
    /// and every accepted layout, so a bug report carries enough to fix
    /// the machine without a round trip.
    #[test]
    fn require_real_jdk_error_names_search_path_and_layouts() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        with_env(
            "CRATONVM_JAVA_HOME",
            Some(tmp.path().to_str().unwrap()),
            || {
                with_env("JAVA_HOME", None, || {
                    let err = require_real_jdk(None).expect_err("bare dir is not a JDK");
                    for needle in [
                        "CRATONVM_JAVA_HOME",
                        "JAVA_HOME",
                        "java on PATH",
                        "jmods/java.base.jmod",
                        "lib/modules",
                        "lib/rt.jar",
                        "--java-home",
                        "--synthetic-jdk",
                    ] {
                        assert!(
                            err.contains(needle),
                            "error message must mention {needle:?}; got:\n{err}"
                        );
                    }
                });
            },
        );
    }

    /// An `rt.jar`-only (JDK 8) install is rejected by the *validation*
    /// path too, not merely by detection — and the rejection is an error,
    /// never a downgrade. Companion to
    /// `detect_real_jdk_returns_none_when_only_rtjar_present`.
    #[test]
    fn require_real_jdk_rejects_rtjar_only_install() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("rt.jar"), b"PK\x03\x04").unwrap();
        with_env(
            "CRATONVM_JAVA_HOME",
            Some(tmp.path().to_str().unwrap()),
            || {
                assert!(require_real_jdk(None).is_err());
            },
        );
    }

    /// A valid JDK makes validation succeed and hand back the resolved
    /// root, and an explicit `--java-home` short-circuits the env probes.
    #[test]
    fn require_real_jdk_accepts_jmods_and_honours_explicit_java_home() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        fake_jdk_with_jmod(tmp.path());
        let explicit = tmp.path().to_str().unwrap().to_string();
        // Explicit path wins even when the env points somewhere useless.
        let bogus = tempfile::tempdir().unwrap();
        with_env(
            "CRATONVM_JAVA_HOME",
            Some(bogus.path().to_str().unwrap()),
            || {
                with_env("JAVA_HOME", None, || {
                    let home =
                        require_real_jdk(Some(explicit.as_str())).expect("explicit JDK is valid");
                    assert_eq!(home, tmp.path());
                    let described = describe_jdk_search(Some(explicit.as_str()));
                    assert!(described.contains("--java-home"), "{described}");
                });
            },
        );
    }

    /// `require_synthetic_jdk` must agree with the compile-time feature —
    /// this is what stops a default (feature-off) build from accepting
    /// `--synthetic-jdk` and booting with no class library at all.
    #[test]
    fn require_synthetic_jdk_tracks_the_cargo_feature() {
        assert_eq!(
            require_synthetic_jdk().is_ok(),
            SYNTHETIC_JDK_COMPILED_IN,
            "synthetic mode must be accepted exactly when the stubs are compiled in"
        );
        assert_eq!(SYNTHETIC_JDK_COMPILED_IN, cfg!(feature = "synthetic-jdk"));
    }

    /// Mode naming is part of the bug-report contract: the strings in
    /// `-version` output must stay stable and must match the CLI flags.
    #[test]
    fn jdk_mode_strings_are_stable_and_match_cli_flags() {
        assert_eq!(JdkMode::Real.as_str(), "real-jdk");
        assert_eq!(JdkMode::Synthetic.as_str(), "synthetic-jdk");
        assert_eq!(JdkMode::Real.selecting_flag(), "--real-jdk");
        assert_eq!(JdkMode::Synthetic.selecting_flag(), "--synthetic-jdk");
        assert_eq!(format!("{}", JdkMode::Real), "real-jdk");
        assert!(!JdkMode::Real.use_synthetic_jdk());
        assert!(JdkMode::Synthetic.use_synthetic_jdk());
        assert_eq!(JdkMode::from_use_synthetic_jdk(true), JdkMode::Synthetic);
        assert_eq!(JdkMode::from_use_synthetic_jdk(false), JdkMode::Real);
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
