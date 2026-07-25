// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use anyhow::{bail, Context, Result};
use clap::Parser;

// Round-7 cross-cutting Fix 2: install mimalloc as the process-wide global
// allocator. Gated on the `mimalloc` feature (enabled by default) so musl /
// exotic targets can `--no-default-features` back to the system allocator.
// Without this `#[global_allocator]` declaration the `mimalloc` dependency
// would be linked but never actually used, so the documented speedup on the
// VM's tiny-object workload (Value, ObjectRef, frame locals, Strings) would
// never take effect.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
use cratonvm_vm::error::MethodCallFailed;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::{
    create_java_string, invoke_on_class_shared, invoke_on_class_shared_no_retarget, Vm,
};
use cratonvm_vm::{ClassPath, VmConfig};
use tracing::info;

/// CratonVM - A Java Virtual Machine implemented in Rust.
///
/// Executes Java programs by loading and interpreting `.class` files.
///
/// Usage: cratonvm [OPTIONS] <CLASS_NAME> [ARGS]...
///        cratonvm [OPTIONS] --jar <FILE.jar> [ARGS]...
#[derive(Parser, Debug)]
#[command(name = "cratonvm", version, about)]
struct Args {
    /// The fully qualified class name to execute (e.g., com.example.Main).
    class_name: Option<String>,

    /// Execute a JAR file. The main class is read from META-INF/MANIFEST.MF.
    /// When -jar is used, the -cp/-classpath flag is ignored; the classpath
    /// comes from the JAR itself and its manifest Class-Path attribute.
    #[arg(long = "jar", value_name = "FILE")]
    jar: Option<String>,

    /// Classpath: directories and JAR files to search for classes.
    // `overrides_with` (self) makes a repeated flag last-wins instead of a hard
    // error, matching the real `java` launcher. Maven Surefire/the WildFly
    // testsuite fork with `-Xmx512m` twice (surefire memory args + jvm.args);
    // without this clap aborts with "cannot be used multiple times" (exit 2),
    // which Surefire reports as "forked VM terminated without saying goodbye".
    #[arg(
        short = 'c',
        long = "classpath",
        alias = "cp",
        overrides_with = "classpath"
    )]
    classpath: Option<String>,

    /// Maximum heap size (e.g., 256m, 1g).
    #[arg(long = "Xmx", value_name = "SIZE", overrides_with = "max_heap")]
    max_heap: Option<String>,

    /// Print verbose class loading information.
    #[arg(long = "verbose:class")]
    verbose_class: bool,

    /// Print verbose GC information.
    #[arg(long = "verbose:gc")]
    verbose_gc: bool,

    /// Boot classpath (overrides JAVA_HOME auto-discovery).
    #[arg(long = "Xbootclasspath", value_name = "PATH")]
    boot_classpath: Option<String>,

    /// JAVA_HOME path for automatic boot/ext classpath discovery.
    #[arg(long = "java-home", value_name = "PATH")]
    java_home: Option<String>,

    /// Skip bytecode verification (like -noverify / -Xverify:none).
    #[arg(long = "noverify")]
    noverify: bool,

    /// Disable JIT compilation (interpreter-only execution).
    ///
    /// Equivalent to setting `CRATONVM_DISABLE_JIT=1` in the environment.
    /// Useful for diagnosing whether a misbehaviour originates in the JIT
    /// versus the interpreter, and as a safety fallback when the JIT is
    /// known to mis-compile a particular library.
    #[arg(long = "nojit")]
    nojit: bool,

    /// Bytecode verification policy (-Xverify:none|remote|all).
    /// `none` skips verification entirely (equivalent to --noverify).
    /// `remote` (HotSpot default) verifies non-boot classes only.
    /// `all` verifies boot classes too.
    #[arg(long = "Xverify", value_name = "MODE")]
    xverify: Option<String>,

    /// CDS shared archive file path (-XX:SharedArchiveFile=path).
    #[arg(long = "XX:SharedArchiveFile", value_name = "PATH")]
    shared_archive_file: Option<String>,

    /// CDS sharing mode (-Xshare:off/on/auto/dump).
    #[arg(long = "Xshare", value_name = "MODE", default_value = "off")]
    xshare: String,

    /// Force synthetic JDK mode (Rust stubs instead of real JDK bytecode).
    ///
    /// As of task #53 the launcher defaults to real-JDK boot via JMOD
    /// (`java.base.jmod` from `JAVA_HOME` / `CRATONVM_JAVA_HOME` / `java`
    /// on `PATH`) whenever a JDK is detected on the host. Passing this
    /// flag forces synthetic mode even when a JDK is present — useful
    /// for hermetic test runs or when comparing synthetic vs. real-JDK
    /// behaviour. When no JDK is detectable the launcher falls back to
    /// synthetic automatically, so this flag is only an explicit
    /// override.
    #[arg(long = "synthetic-jdk")]
    synthetic_jdk: bool,

    /// Enable Panama FFI native access (mirrors JDK `--enable-native-access`).
    ///
    /// With native access disabled (the default after the security fix),
    /// Panama downcalls, `MemorySegment.ofAddress`, and `reinterpret` throw
    /// `IllegalCallerException`. Passing this flag opens the process-wide
    /// gate so those restricted FFI operations are permitted.
    ///
    /// The optional value (`ALL-UNNAMED` or a module name) mirrors the JDK
    /// spelling but is accepted-and-ignored: CratonVM's gate is a single
    /// coarse process-wide toggle, not a per-module grant. The flag is also
    /// accepted with no value at all (bare `--enable-native-access`). Optional
    /// values must use `=` so a following main class is not consumed as the
    /// module name. Because clap accepts the long name directly, the JDK
    /// invocation (`--enable-native-access=ALL-UNNAMED`) passes through
    /// unchanged.
    #[arg(
        long = "enable-native-access",
        value_name = "MODULE",
        num_args = 0..=1,
        default_missing_value = "ALL-UNNAMED",
        require_equals = true,
    )]
    enable_native_access: Option<String>,

    /// AOT compilation mode (-XX:AOTMode=off/training/production).
    #[arg(long = "XX:AOTMode", value_name = "MODE", default_value = "off")]
    aot_mode: String,

    /// AOT cache file path (-XX:AOTCache=path). Used as input in production
    /// mode and as output in training mode (unless AOTCacheOutput is set).
    #[arg(long = "XX:AOTCache", value_name = "PATH")]
    aot_cache: Option<String>,

    /// AOT cache output path (-XX:AOTCacheOutput=path). Overrides AOTCache
    /// for writing in training mode.
    #[arg(long = "XX:AOTCacheOutput", value_name = "PATH")]
    aot_cache_output: Option<String>,

    /// Audit missing native methods: log all ACC_NATIVE methods that were invoked
    /// but had no Rust implementation. Printed on VM shutdown.
    #[arg(long = "XX:AuditMissingNatives")]
    audit_missing_natives: bool,

    /// HotSpot `-XX:±ShowCodeDetailsInExceptionMessages` (JEP 358): route the
    /// non-invoke null-deref opcodes (getfield/putfield/arraylength/array
    /// access/monitor/athrow) through the helpful-NPE message helper. Default
    /// **on**, matching HotSpot (messages verified byte-identical). Tri-state:
    /// absent → use the `VmConfig` default (on); the `-XX:+`/`-XX:-` JDK
    /// spellings are rewritten to `=true`/`=false` (see `rewrite_jvm_args`).
    #[arg(
        long = "XX:ShowCodeDetailsInExceptionMessages",
        num_args = 0..=1,
        default_missing_value = "true"
    )]
    show_code_details_in_exception_messages: Option<bool>,

    /// NEW-10: dump the missing-natives audit log to the given JSON file
    /// on VM shutdown. Implies `--XX:AuditMissingNatives`. The output
    /// schema is `{ "missing_natives": [{class, name, descriptor,
    /// sample_call_site}...] }` with entries sorted so the file is
    /// diff-stable against a committed baseline.
    #[arg(long = "dump-missing-natives", value_name = "FILE")]
    dump_missing_natives: Option<String>,

    /// T2.1.3: dump the missing-natives audit log to the given JSON file,
    /// grouped by JDK module. Implies `--XX:AuditMissingNatives`. Schema:
    /// `{ "version": 1, "modules": { "java.base": [...], "other": [...] } }`.
    /// Entries and module keys are sorted for byte-stable output, so the
    /// file is suitable for committing as a census baseline.
    #[arg(long = "dump-missing-natives-grouped", value_name = "FILE")]
    dump_missing_natives_grouped: Option<String>,

    /// Synthetic-stub census: dump every registered native with its
    /// classification (intrinsic / bridge / synthetic-stub) to the given
    /// JSON file on VM shutdown. Schema:
    /// `{ "counts": {...}, "natives": [{class, name, descriptor, kind}...] }`,
    /// sorted for byte-stable output. Use this to verify the default build is
    /// synthetic-stub-free. See docs/synthetic-vs-real-explained.md.
    #[arg(long = "dump-native-registry", value_name = "FILE")]
    dump_native_registry: Option<String>,

    /// Enable JDWP debug server on the given port (e.g., 5005).
    /// Equivalent to -agentlib:jdwp=transport=dt_socket,server=y,address=PORT
    #[arg(long = "jdwp-port", value_name = "PORT")]
    jdwp_port: Option<u16>,

    /// Suspend VM at startup waiting for debugger to attach (requires --jdwp-port).
    #[arg(long = "jdwp-suspend")]
    jdwp_suspend: bool,

    // -----------------------------------------------------------------------
    // JPMS module system flags
    // -----------------------------------------------------------------------
    /// Module path: directories and modular JARs to search for modules.
    /// Format: path1;path2 (Windows) or path1:path2 (Unix).
    #[arg(long = "module-path", alias = "p", value_name = "PATH")]
    module_path: Option<String>,

    /// Add a read edge between modules.
    /// Format: `reader_module=target_module[,target_module2,...]`.
    /// Can be specified multiple times.
    #[arg(long = "add-reads", value_name = "MODULE=TARGET")]
    add_reads: Vec<String>,

    /// Export a package from a module to another module (or ALL-UNNAMED).
    /// Format: `module/package=target_module`.
    /// Can be specified multiple times.
    #[arg(long = "add-exports", value_name = "MODULE/PKG=TARGET")]
    add_exports: Vec<String>,

    /// Open a package for deep reflection from a module to another module.
    /// Format: `module/package=target_module`.
    /// Can be specified multiple times.
    #[arg(long = "add-opens", value_name = "MODULE/PKG=TARGET")]
    add_opens: Vec<String>,

    /// Additional root modules to resolve beyond the initial module.
    /// Can be specified multiple times.  `ALL-MODULE-PATH` resolves all
    /// modules found on the module path.
    #[arg(long = "add-modules", value_name = "MODULE")]
    add_modules: Vec<String>,

    /// Disable container/cgroup support (-XX:-UseContainerSupport).
    /// When set, the JVM ignores cgroup memory/CPU limits.
    #[arg(long = "XX:-UseContainerSupport")]
    disable_container_support: bool,

    /// Garbage-collector selector. Carries the collector name from a HotSpot
    /// `-XX:+Use<name>GC` flag (with the `Use`/`GC` wrapper stripped by
    /// `normalize_java_launcher_argv`), e.g. `G1` for `-XX:+UseG1GC`. CratonVM
    /// honours `G1` and the default `Generational`; any other collector warns
    /// and falls back to Generational. Repeated flags follow HotSpot last-wins
    /// (`overrides_with` self → no `ArgumentConflict` on a second occurrence).
    /// See docs/feature-designs/concurrent-gc-maturation.md §3.1.
    #[arg(long = "XX:UseGc", value_name = "NAME", overrides_with = "gc_selector")]
    gc_selector: Option<String>,

    /// `-XX:InitiatingHeapOccupancyPercent=<n>` → G1 IHOP (honoured under G1).
    #[arg(long = "XX:IHOP", value_name = "PCT", overrides_with = "g1_ihop")]
    g1_ihop: Option<String>,

    /// `-XX:MaxDirectMemorySize=<size>` -> direct (off-heap NIO) buffer
    /// accounting cap. Mirrors real JDK: when absent, the cap defaults to
    /// `-Xmx` instead of a fixed value. See
    /// docs/known-issues/h2/bug-h2-largeblob-direct-memory-oom.md.
    #[arg(
        long = "XX:MaxDirectMemorySize",
        value_name = "SIZE",
        overrides_with = "max_direct_memory"
    )]
    max_direct_memory: Option<String>,

    /// `-XX:G1HeapRegionSize=<bytes>` → G1 region size (honoured under G1).
    #[arg(
        long = "XX:G1RegionSize",
        value_name = "SIZE",
        overrides_with = "g1_region_size"
    )]
    g1_region_size: Option<String>,

    /// `-XX:MaxGCPauseMillis=<n>` → G1 pause target (honoured under G1).
    #[arg(
        long = "XX:MaxGCPause",
        value_name = "MS",
        overrides_with = "g1_max_pause"
    )]
    g1_max_pause: Option<String>,

    /// `-XX:±UseStringDeduplication` → G1 String dedup (honoured under G1).
    #[arg(
        long = "XX:StringDedup",
        value_name = "BOOL",
        overrides_with = "g1_string_dedup"
    )]
    g1_string_dedup: Option<String>,

    /// Unified logging spec (-Xlog:tag[+tag]*[=level][:output[:decorators]]).
    /// Example: --Xlog gc*=info:stdout:time,level,tags
    #[arg(long = "Xlog", value_name = "SPEC")]
    xlog: Option<String>,

    /// T19.H1 — if set, spawn a watchdog thread that, after `SECONDS`,
    /// signals every interpreter thread to dump its frame chain to
    /// stderr and then calls `std::process::abort()`. Used to
    /// diagnose silent-hang bootstraps (Keycloak, WildFly, Quarkus).
    /// The flag is honoured on a best-effort basis — threads stuck
    /// inside Rust native code will not dump (the watchdog still
    /// aborts with a reduced-information banner in that case).
    #[arg(long = "stack-dump-on-timeout", value_name = "SECONDS")]
    stack_dump_on_timeout: Option<u64>,

    // -----------------------------------------------------------------------
    // GPU offload (see docs/gpu/cuda-oxide-evaluation.md)
    //
    // Every field below is gated behind the `gpu` Cargo feature. Without
    // the feature the flags are not parsed, not documented in --help,
    // and the CPU execution path is byte-identical to before the GPU
    // work landed.
    // -----------------------------------------------------------------------
    /// Enable GPU offload of eligible static methods. Requires the
    /// CLI to be built with `--features gpu` and a CUDA driver. With
    /// no driver, the flag is honoured but no methods are offloaded.
    #[cfg(feature = "gpu")]
    #[arg(long = "gpu")]
    gpu: bool,

    /// Select CUDA device ordinal when --gpu is on. Defaults to 0.
    #[cfg(feature = "gpu")]
    #[arg(long = "gpu-device", value_name = "N", default_value_t = 0)]
    gpu_device: u32,

    /// Minimum estimated work (array length / loop trip count) before
    /// a method is offloaded. Smaller inputs run on the CPU because
    /// the host↔device round-trip dominates.
    #[cfg(feature = "gpu")]
    #[arg(long = "gpu-min-work", value_name = "N", default_value_t = 4096)]
    gpu_min_work: u32,

    /// Print one line per analyzer verdict (Eligible / Rejected) at
    /// INFO level. Useful for understanding why a method did or did
    /// not offload.
    #[cfg(feature = "gpu")]
    #[arg(long = "print-gpu-decisions")]
    print_gpu_decisions: bool,

    /// Probe the GPU, print device name + compute capability + memory,
    /// then exit. Useful for sanity-checking before a real run.
    #[cfg(feature = "gpu")]
    #[arg(long = "gpu-info")]
    gpu_info: bool,

    /// Arguments passed to the Java program's main method.
    #[arg(trailing_var_arg = true)]
    args: Vec<String>,
}

/// Validate that `name` is a valid Java class name in internal (slash) notation.
///
/// Each segment (split by `/`) must:
/// - Not be empty
/// - Start with a letter, underscore, or dollar sign
/// - Contain only alphanumeric characters, underscores, or dollar signs
fn validate_class_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Class name must not be empty");
    }
    for (i, segment) in name.split('/').enumerate() {
        if segment.is_empty() {
            bail!(
                "Invalid class name '{}': segment {} is empty (double slash or leading/trailing slash)",
                name,
                i + 1
            );
        }
        let first = match segment.chars().next() {
            Some(ch) => ch,
            None => continue, // skip empty segments (already caught above, but be safe)
        };
        if !first.is_ascii_alphabetic() && first != '_' && first != '$' {
            bail!(
                "Invalid class name '{}': segment '{}' must start with a letter, underscore, or dollar sign",
                name,
                segment
            );
        }
        for ch in segment.chars() {
            if !ch.is_ascii_alphanumeric() && ch != '_' && ch != '$' {
                bail!(
                    "Invalid class name '{}': segment '{}' contains invalid character '{}'",
                    name,
                    segment,
                    ch
                );
            }
        }
    }
    Ok(())
}

/// Expand missing aggregate JAR references into sibling split JARs.
///
/// Some libraries ship as a single "all-in-one" fat JAR (e.g.
/// `netty-all.jar`, `groovy-all.jar`) or, alternatively, as a set of split
/// modules that live next to it (e.g. `netty-common.jar`, `netty-buffer.jar`,
/// …).  When a user passes a classpath entry that points at the aggregate JAR
/// but the repository only contains the split distribution, the classloader
/// silently drops the entry and the program fails with a puzzling
/// `NoClassDefFoundError`.
///
/// This helper detects that situation for a fixed set of known prefixes
/// (currently `netty`) and substitutes the aggregate name with every sibling
/// split JAR that shares the prefix.  The substitution only happens when the
/// aggregate file does *not* exist on disk; if it exists the entry is left
/// untouched.
///
/// Returns a new classpath list with the substitutions applied.  A warning is
/// emitted to stderr when a substitution occurs so the user knows what
/// happened.
/// Cheap sniff for whether `jar_path` looks like a Quarkus fast-jar /
/// runner packaging, used to decide whether the expensive multi-dir
/// classpath walk (the `app/quarkus/lib/...` probe in `run()`) is worth
/// running. A real Quarkus app always ships one of these signature
/// artifacts next to the runner jar:
///
///   * `quarkus-run.jar` — the canonical fast-jar launcher;
///   * `quarkus-app/` — the fast-jar output directory;
///   * a `quarkus/` subdir — holds `quarkus-application.dat` +
///     `generated-bytecode.jar`;
///   * `quarkus-application.dat` — the serialized bootstrap metadata.
///
/// We check the jar's own directory AND its parent (Keycloak puts the
/// runner one level deep in `lib/`), mirroring the two `roots` the walk
/// itself probes. Each check is a single `Path::exists()` stat — far
/// cheaper than the `canonicalize` + `read_dir` of ~10 candidate dirs the
/// walk performs. For a trivial non-Quarkus HelloWorld jar this returns
/// `false` after at most a handful of stats, skipping the walk entirely.
///
/// Conservative by design: any false positive merely re-enables the same
/// walk that previously always ran, so real Quarkus behaviour is never
/// degraded.
fn quarkus_signature_present(jar_path: &std::path::Path) -> bool {
    // Signature file/dir names looked for in each candidate directory.
    const SIGNATURES: &[&str] = &[
        "quarkus-run.jar",
        "quarkus-app",
        "quarkus",
        "quarkus-application.dat",
    ];

    // The runner jar's own dir, plus its parent (one-level-deep packagings
    // such as Keycloak's `lib/quarkus-run.jar`). No canonicalisation: a
    // relative `jar_path` still has a usable parent chain for `.join()`,
    // and `exists()` resolves relative paths against the cwd just fine.
    let mut dirs: Vec<&std::path::Path> = Vec::with_capacity(2);
    if let Some(d) = jar_path.parent() {
        // `Path::parent()` of a bare basename is `Some("")`; treat the
        // empty path as "current directory" so the stats still hit.
        dirs.push(if d.as_os_str().is_empty() {
            std::path::Path::new(".")
        } else {
            d
        });
        if let Some(pp) = d.parent() {
            if !pp.as_os_str().is_empty() {
                dirs.push(pp);
            }
        }
    } else {
        dirs.push(std::path::Path::new("."));
    }

    for dir in dirs {
        for sig in SIGNATURES {
            if dir.join(sig).exists() {
                return true;
            }
        }
    }
    false
}

fn expand_aggregate_jars(entries: Vec<String>) -> Vec<String> {
    // (aggregate_file_name, split_prefix) pairs.  The split prefix is
    // matched case-insensitively against sibling file names.
    const AGGREGATES: &[(&str, &str)] = &[("netty-all.jar", "netty-")];

    let mut out: Vec<String> = Vec::with_capacity(entries.len());
    for entry in entries {
        let path = std::path::Path::new(&entry);
        // PERF: the aggregate-jar case is rare, so do a cheap filename match
        // before touching the filesystem. Entries whose file name is not a
        // known aggregate name can never be substituted, so they skip the
        // `exists()` stat syscall (and the `read_dir` below) entirely.
        let file_name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_ascii_lowercase(),
            None => {
                out.push(entry);
                continue;
            }
        };
        let matched = AGGREGATES.iter().find(|(name, _)| file_name == *name);
        let Some((_, split_prefix)) = matched else {
            out.push(entry);
            continue;
        };
        // Only now (for a candidate aggregate name) pay for the stat syscall.
        // If the entry already exists on disk (or is a directory), keep it.
        if path.exists() {
            out.push(entry);
            continue;
        }
        // Look for split jars next to where the aggregate was expected to be.
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => std::path::PathBuf::from("."),
        };
        let Ok(reader) = std::fs::read_dir(&parent) else {
            out.push(entry);
            continue;
        };
        let mut substitutes: Vec<String> = Vec::new();
        for dirent in reader.flatten() {
            let p = dirent.path();
            if p.extension().map(|e| e == "jar").unwrap_or(false) {
                if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                    if name.to_ascii_lowercase().starts_with(split_prefix)
                        && name.to_ascii_lowercase() != file_name
                    {
                        substitutes.push(p.to_string_lossy().into_owned());
                    }
                }
            }
        }
        if substitutes.is_empty() {
            // Nothing to substitute — preserve original entry so downstream
            // logging surfaces the missing file.
            out.push(entry);
            continue;
        }
        // Deterministic order helps reproducibility.
        substitutes.sort();
        eprintln!(
            "Note: classpath entry {entry} not found; substituting {n} sibling '{split_prefix}*.jar' files from {parent}",
            n = substitutes.len(),
            parent = parent.display()
        );
        out.extend(substitutes);
    }
    out
}

struct StagedArchiveCleanup {
    path: std::path::PathBuf,
}

static STAGED_ARCHIVE_COPIES: std::sync::OnceLock<std::sync::Mutex<Vec<std::path::PathBuf>>> =
    std::sync::OnceLock::new();

fn staged_archive_copies() -> &'static std::sync::Mutex<Vec<std::path::PathBuf>> {
    STAGED_ARCHIVE_COPIES.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn lock_staged_archive_copies(
    registry: &'static std::sync::Mutex<Vec<std::path::PathBuf>>,
) -> std::sync::MutexGuard<'static, Vec<std::path::PathBuf>> {
    match registry.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn remove_staged_archive_copy(path: &std::path::Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                "failed to remove staged archive copy {}: {e}",
                path.display()
            );
        }
    }
}

fn register_staged_archive_copy(path: std::path::PathBuf) {
    lock_staged_archive_copies(staged_archive_copies()).push(path);
}

fn unregister_staged_archive_copy(path: &std::path::Path) {
    if let Some(registry) = STAGED_ARCHIVE_COPIES.get() {
        let mut guard = lock_staged_archive_copies(registry);
        guard.retain(|registered| registered.as_path() != path);
    }
}

fn cleanup_staged_archive_copies() {
    if let Some(registry) = STAGED_ARCHIVE_COPIES.get() {
        let paths: Vec<_> = {
            let mut guard = lock_staged_archive_copies(registry);
            guard.drain(..).collect()
        };
        for path in paths {
            remove_staged_archive_copy(&path);
        }
    }
}

impl Drop for StagedArchiveCleanup {
    fn drop(&mut self) {
        unregister_staged_archive_copy(&self.path);
        remove_staged_archive_copy(&self.path);
    }
}

fn classpath_entry_for_archive(
    archive_path: &std::path::Path,
) -> Result<(std::path::PathBuf, Option<StagedArchiveCleanup>)> {
    if archive_path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
    {
        return Ok((archive_path.to_path_buf(), None));
    }

    // JN3: ClassPath::new only accepts entries whose extension is `.jar`
    // (or `.jmod`/`modules`). WARs, EARs, and other Java archive types are
    // silently dropped, so stage a temporary `.jar` copy that remains alive
    // until the launcher returns.
    let stem = archive_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("app");
    let pid = std::process::id();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = std::env::temp_dir();
    let mut tmp = dir.join(format!("cratonvm-{pid}-{now_ms}-{stem}.jar"));
    let mut dst_file = None;
    for attempt in 0..16 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(f) => {
                dst_file = Some(f);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                tmp = dir.join(format!("cratonvm-{pid}-{now_ms}-{attempt}-{stem}.jar"));
            }
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!(
                    "failed to stage {} as {} for classpath registration",
                    archive_path.display(),
                    tmp.display()
                )));
            }
        }
    }
    let mut dst_file = dst_file.ok_or_else(|| {
        anyhow::anyhow!(
            "failed to stage {} for classpath registration: \
             could not create a unique temp file in {}",
            archive_path.display(),
            dir.display()
        )
    })?;
    let copy_result = (|| -> Result<()> {
        let mut src_file = std::fs::File::open(archive_path)
            .with_context(|| format!("failed to open {} for staging", archive_path.display()))?;
        std::io::copy(&mut src_file, &mut dst_file).with_context(|| {
            format!(
                "failed to stage {} as {} for classpath registration",
                archive_path.display(),
                tmp.display()
            )
        })?;
        Ok(())
    })();
    drop(dst_file);
    if let Err(e) = copy_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    tracing::debug!(
        "JN3: staged non-.jar archive {} в†’ {} so ClassPath accepts it",
        archive_path.display(),
        tmp.display()
    );
    register_staged_archive_copy(tmp.clone());
    Ok((tmp.clone(), Some(StagedArchiveCleanup { path: tmp })))
}

/// Launcher options that consume the *following* argv token as their value
/// (the `--opt value` form). Needed by [`insert_program_args_separator`] so a
/// value token (e.g. the classpath string after `-cp`) is not mistaken for the
/// bare main-class name that selects the program. Both the HotSpot spellings
/// (`-jar`, `-classpath`, `-cp`, `-p`, `-mp`) and the clap long spellings
/// (`--jar`, `--classpath`, ...) are listed because this runs before
/// `normalize_java_launcher_argv` rewrites them.
///
/// Options using the inline `--opt=value` form need no entry here -- the value
/// travels in the same token. Boolean flags also need no entry.
const VALUE_TAKING_OPTS: &[&str] = &[
    "-jar",
    "--jar",
    "-classpath",
    "-cp",
    "--classpath",
    "-c",
    "-p",
    "--module-path",
    "-mp",
    // HotSpot single-dash forms used as separate tokens (`-Xmx 256m`,
    // `-Xshare on`, etc.). Most users write them inline (`-Xmx256m`),
    // but Maven Surefire and some test harnesses split them. Listing
    // them here keeps the separator-inserter from mistaking the value
    // token for a bare main-class name.
    "-Xmx",
    "-Xms",
    "-Xshare",
    "-Xverify",
    "-Xbootclasspath",
    "-Xlog",
    "--Xmx",
    "--Xms",
    "--Xbootclasspath",
    "--java-home",
    "--Xverify",
    "--XX:SharedArchiveFile",
    "--XX:UseGc",
    "--Xshare",
    "--XX:AOTMode",
    "--XX:AOTCache",
    "--XX:AOTCacheOutput",
    "--dump-missing-natives",
    "--dump-missing-natives-grouped",
    "--dump-native-registry",
    "--jdwp-port",
    "--add-reads",
    "--add-exports",
    "--add-opens",
    "--add-modules",
    "--Xlog",
    "--stack-dump-on-timeout",
    "--gpu-device",
    "--gpu-min-work",
];

/// Expand Java argument files (`@<path>`).
///
/// When the JVM launcher sees an argument starting with `@` (not `@@`),
/// it reads the file at that path and expands its contents as additional
/// command-line arguments. This is Java's argument-file feature (JEP 293,
/// available since Java 9). Gradle uses it to pass large classpaths via
/// `@classpath-file.txt` to avoid command-line length limits.
///
/// The file format:
/// - Arguments separated by whitespace (spaces, tabs, newlines)
/// - `"..."` or `'...'` quoted strings (quotes stripped, content preserved)
/// - `#` starts a comment to end of line
/// - Backslash escapes the next character inside quotes
/// - `@@path` is a literal `@path` (single-expansion escape)
///
/// Expansion is NOT recursive (nested `@file` references inside the
/// expanded content are left as-is) to avoid runaway expansion.
/// The `args[0]` element (program name) is never expanded.
fn expand_argfiles(args: Vec<String>) -> Vec<String> {
    if args.is_empty() {
        return args;
    }
    let mut out = vec![args[0].clone()]; // preserve argv[0]
    for arg in &args[1..] {
        if let Some(path_str) = arg.strip_prefix('@') {
            if path_str.starts_with('@') {
                // `@@path` -> literal `@path`
                out.push(path_str.to_string());
                continue;
            }
            match std::fs::read_to_string(path_str) {
                Ok(content) => {
                    // Tokenize the file contents
                    out.extend(tokenize_argfile(&content));
                    if std::env::var_os("CRATONVM_DBG_ARGS").is_some() {
                        eprintln!("[cratonvm] @-expanded {path_str}: {} tokens", out.len());
                    }
                }
                Err(e) => {
                    // If the file can't be read, leave the @arg as-is so
                    // downstream stages can produce a clear error message.
                    if std::env::var_os("CRATONVM_DBG_ARGS").is_some() {
                        eprintln!("[cratonvm] @-file read error {path_str}: {e}");
                    }
                    out.push(arg.clone());
                }
            }
        } else {
            out.push(arg.clone());
        }
    }
    out
}

/// Tokenize the content of a Java argument file.
///
/// Splits on unquoted whitespace; `"..."` and `'...'` preserve whitespace
/// and strip the outer quotes; `#` starts a comment to end of line;
/// backslash inside quotes escapes the following character.
fn tokenize_argfile(content: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = content.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '#' => {
                // Comment: skip to end of line
                if current.is_empty() {
                    // Flush any pending token
                } else {
                    tokens.push(std::mem::take(&mut current));
                }
                for ch2 in chars.by_ref() {
                    if ch2 == '\n' {
                        break;
                    }
                }
            }
            '"' | '\'' => {
                // Quoted string: collect until matching quote
                let quote = ch;
                loop {
                    match chars.next() {
                        None => break,
                        Some('\\') if quote == '"' => {
                            // Backslash escape inside double-quotes
                            if let Some(escaped) = chars.next() {
                                current.push(escaped);
                            }
                        }
                        Some(c) if c == quote => break,
                        Some(c) => current.push(c),
                    }
                }
            }
            ' ' | '\t' | '\r' | '\n' => {
                // Whitespace: flush token
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Enforce `java`-launcher positional semantics: every token *after* the
/// program selector is a program argument and must be passed to the Java
/// application verbatim -- even if it starts with `-`/`--` or equals
/// `--help` / `--version` / `--list-modules`.
///
/// The program is selected by either `-jar <jarfile>` or the first bare
/// (non-option) token used as a main-class name. This function scans the
/// leading option section and, as soon as it identifies the selector,
/// inserts a literal `--` separator immediately after it. The downstream
/// stages (`normalize_java_launcher_argv`, `extract_system_properties`,
/// `extract_hotspot_flags`) and clap itself all treat everything past `--`
/// as opaque program args, so launcher options are recognised only in the
/// leading section -- matching the stock `java` launcher.
///
/// If the caller already supplied an explicit `--`, or no program selector
/// is present (e.g. `java --version` / `java --help` with no program), the
/// argv is returned unchanged so the launcher still handles those itself.
fn insert_program_args_separator(args: Vec<String>) -> Vec<String> {
    if args.is_empty() {
        return args;
    }
    let mut out = vec![args[0].clone()];
    let mut i = 1usize;
    while i < args.len() {
        let a = args[i].as_str();
        // An explicit separator already delimits the program args -- respect
        // it and copy the remainder verbatim.
        if a == "--" {
            out.extend_from_slice(&args[i..]);
            return out;
        }
        // `-jar <jar>`: the jar is the selector. Copy `-jar` and its operand,
        // then insert `--` so the rest of argv is program args (unless the
        // caller already placed an explicit `--` there).
        if (a == "-jar" || a == "--jar") && i + 1 < args.len() {
            out.push(args[i].clone());
            out.push(args[i + 1].clone());
            if args.get(i + 2).map(String::as_str) != Some("--") {
                out.push("--".into());
            }
            out.extend_from_slice(&args[i + 2..]);
            return out;
        }
        // Inline `--jar=<jar>` / `-jar=<jar>` form.
        if a.starts_with("-jar=") || a.starts_with("--jar=") {
            out.push(args[i].clone());
            if args.get(i + 1).map(String::as_str) != Some("--") {
                out.push("--".into());
            }
            out.extend_from_slice(&args[i + 1..]);
            return out;
        }
        // An option that consumes the next token as its value -- copy both
        // and keep scanning; the value token is not the main-class name.
        if VALUE_TAKING_OPTS.contains(&a) {
            out.push(args[i].clone());
            if i + 1 < args.len() {
                out.push(args[i + 1].clone());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        // Any other `-`/`--`-prefixed token in the leading section is a
        // launcher option (boolean flag or inline `--opt=value`) -- copy and
        // continue scanning.
        if a.starts_with('-') {
            out.push(args[i].clone());
            i += 1;
            continue;
        }
        // First bare token: the main-class name. It selects the program;
        // insert `--` right after it so all following tokens are program
        // args (unless an explicit `--` already follows).
        out.push(args[i].clone());
        if args.get(i + 1).map(String::as_str) != Some("--") {
            out.push("--".into());
        }
        out.extend_from_slice(&args[i + 1..]);
        return out;
    }
    out
}

/// Rewrite common HotSpot launcher spellings so clap can parse them.
///
/// HotSpot uses single-dash flags with idiosyncratic syntax (`-Xmx256m`,
/// `-Xshare:on`, `-XX:AOTMode=off`, `-XX:-UseContainerSupport`, etc.) that
/// clap's "long option" parser cannot handle natively — clap expects
/// `--Xmx 256m`. The `[[bin]] name = "java"` alias (see Cargo.toml) exists
/// specifically so the cratonvm binary is drop-in compatible with stock
/// `java`, so a Maven / Surefire / Gradle invocation like
/// `java -Xmx256m -classpath x Main` MUST parse.
///
/// This function rewrites every HotSpot single-dash spelling we care about
/// to the equivalent double-dash clap form *before* clap sees the argv.
/// `extract_hotspot_flags` handles the few cases that aren't expressible as
/// clap long options (`-XX:+/-Foo` boolean toggles for `HeapDumpOnOutOfMemoryError`,
/// `-agentlib:` / `-agentpath:` / `-javaagent:`).
fn normalize_java_launcher_argv(args: Vec<String>) -> Vec<String> {
    if args.is_empty() {
        return args;
    }
    let mut out = vec![args[0].clone()];
    let mut i = 1usize;
    let mut past_separator = false;
    while i < args.len() {
        let a = args[i].as_str();
        if past_separator {
            out.push(args[i].clone());
            i += 1;
            continue;
        }
        if a == "--" {
            past_separator = true;
            out.push(args[i].clone());
            i += 1;
            continue;
        }
        // JBoss Modules / WildFly use `-mp <modules dir>` after `-jar
        // jboss-modules.jar`. clap parses `-mp` as the short-flag cluster
        // `-m` + `-p`, which errors ("unexpected argument '-m'"). Mirror an
        // explicit `--` so everything from `-mp` onward becomes program args.
        if a == "-mp" {
            out.push("--".into());
            past_separator = true;
            out.push(args[i].clone());
            i += 1;
            continue;
        }
        if a == "-jar" && i + 1 < args.len() {
            out.push("--jar".into());
            out.push(args[i + 1].clone());
            i += 2;
        } else if a == "-version" || a == "-v" {
            out.push("--version".into());
            i += 1;
        } else if (a == "-classpath" || a == "-cp") && i + 1 < args.len() {
            out.push("--classpath".into());
            out.push(args[i + 1].clone());
            i += 2;
        } else if let Some(rest) = a.strip_prefix("-classpath=") {
            out.push("--classpath".into());
            out.push(rest.to_string());
            i += 1;
        } else if let Some(rest) = a.strip_prefix("-cp=") {
            out.push("--classpath".into());
            out.push(rest.to_string());
            i += 1;
        }
        // -------- HotSpot -X compat: inline single-dash spellings --------
        // `-Xmx256m` / `-Xmx 256m` -> `--Xmx 256m`
        else if let Some(rest) = a.strip_prefix("-Xmx") {
            if rest.is_empty() && i + 1 < args.len() {
                out.push("--Xmx".into());
                out.push(args[i + 1].clone());
                i += 2;
            } else {
                out.push("--Xmx".into());
                out.push(rest.to_string());
                i += 1;
            }
        }
        // `-Xshare:on` / `-Xshare:off` -> `--Xshare on`
        else if let Some(rest) = a.strip_prefix("-Xshare:") {
            out.push("--Xshare".into());
            out.push(rest.to_string());
            i += 1;
        }
        // `-Xshare<sp>val` -> `--Xshare val` (rare)
        else if a == "-Xshare" && i + 1 < args.len() {
            out.push("--Xshare".into());
            out.push(args[i + 1].clone());
            i += 2;
        }
        // `-Xverify:none|remote|all` -> `--Xverify none|remote|all`
        else if let Some(rest) = a.strip_prefix("-Xverify:") {
            // `-Xverify:none` is the HotSpot shorthand for `-noverify`.
            // Translate to `--noverify` so the boolean flag fires; the
            // existing CLI also accepts `--Xverify none` as a value flag.
            if rest == "none" {
                out.push("--noverify".into());
            } else {
                out.push("--Xverify".into());
                out.push(rest.to_string());
            }
            i += 1;
        }
        // `-Xverify val` (separate token, rare)
        else if a == "-Xverify" && i + 1 < args.len() {
            out.push("--Xverify".into());
            out.push(args[i + 1].clone());
            i += 2;
        }
        // `-noverify` -> `--noverify` (HotSpot deprecated but still accepted)
        else if a == "-noverify" {
            out.push("--noverify".into());
            i += 1;
        }
        // `-Xbootclasspath:path` / `-Xbootclasspath/a:path` / `-Xbootclasspath/p:path`
        // -> `--Xbootclasspath path`. The /a (append) and /p (prepend) forms
        // are collapsed to a plain replace; cratonvm does not model the three
        // positions separately (boot CP is a single ordered list).
        else if let Some(rest) = a.strip_prefix("-Xbootclasspath/a:") {
            out.push("--Xbootclasspath".into());
            out.push(rest.to_string());
            i += 1;
        } else if let Some(rest) = a.strip_prefix("-Xbootclasspath/p:") {
            out.push("--Xbootclasspath".into());
            out.push(rest.to_string());
            i += 1;
        } else if let Some(rest) = a.strip_prefix("-Xbootclasspath:") {
            out.push("--Xbootclasspath".into());
            out.push(rest.to_string());
            i += 1;
        } else if a == "-Xbootclasspath" && i + 1 < args.len() {
            out.push("--Xbootclasspath".into());
            out.push(args[i + 1].clone());
            i += 2;
        }
        // `-Xlog:spec` -> `--Xlog spec`
        else if let Some(rest) = a.strip_prefix("-Xlog:") {
            out.push("--Xlog".into());
            out.push(rest.to_string());
            i += 1;
        } else if a == "-Xlog" && i + 1 < args.len() {
            out.push("--Xlog".into());
            out.push(args[i + 1].clone());
            i += 2;
        }
        // -------- HotSpot -XX compat: -XX:Foo=val and -XX:+/-Foo --------
        // `-XX:SharedArchiveFile=path` -> `--XX:SharedArchiveFile path`
        else if let Some(rest) = a.strip_prefix("-XX:SharedArchiveFile=") {
            out.push("--XX:SharedArchiveFile".into());
            out.push(rest.to_string());
            i += 1;
        }
        // `-XX:AOTMode=off|training|production` -> `--XX:AOTMode <val>`
        else if let Some(rest) = a.strip_prefix("-XX:AOTMode=") {
            out.push("--XX:AOTMode".into());
            out.push(rest.to_string());
            i += 1;
        }
        // `-XX:AOTCache=path` -> `--XX:AOTCache <path>`
        else if let Some(rest) = a.strip_prefix("-XX:AOTCache=") {
            out.push("--XX:AOTCache".into());
            out.push(rest.to_string());
            i += 1;
        }
        // `-XX:AOTCacheOutput=path` -> `--XX:AOTCacheOutput <path>`
        else if let Some(rest) = a.strip_prefix("-XX:AOTCacheOutput=") {
            out.push("--XX:AOTCacheOutput".into());
            out.push(rest.to_string());
            i += 1;
        }
        // `-XX:+AuditMissingNatives` -> `--XX:AuditMissingNatives`
        // `-XX:-AuditMissingNatives` -> drop (default off).
        else if a == "-XX:+AuditMissingNatives" {
            out.push("--XX:AuditMissingNatives".into());
            i += 1;
        } else if a == "-XX:-AuditMissingNatives" {
            // Default is off; nothing to emit.
            i += 1;
        }
        // `-XX:+ShowCodeDetailsInExceptionMessages` -> the clap toggle (on);
        // `-XX:-...` -> the explicit `=false` form (the default is now on, so
        // opting out must be representable, not merely "absent").
        else if a == "-XX:+ShowCodeDetailsInExceptionMessages" {
            out.push("--XX:ShowCodeDetailsInExceptionMessages=true".into());
            i += 1;
        } else if a == "-XX:-ShowCodeDetailsInExceptionMessages" {
            out.push("--XX:ShowCodeDetailsInExceptionMessages=false".into());
            i += 1;
        }
        // `-XX:-UseContainerSupport` -> `--XX:-UseContainerSupport`
        // (the clap long name literally is `XX:-UseContainerSupport`).
        else if a == "-XX:-UseContainerSupport" {
            out.push("--XX:-UseContainerSupport".into());
            i += 1;
        } else if a == "-XX:+UseContainerSupport" {
            // Default is on; nothing to emit.
            i += 1;
        }
        // `-Xms<size>` (minimum/initial heap): CratonVM sizes the heap from
        // `-Xmx` only, so the minimum-heap hint is accepted and ignored
        // rather than rejected. A drop-in `java` must not abort on it —
        // Maven Surefire forks pass `-Xms512m` unconditionally.
        //
        // Both the inline (`-Xms512m`) and separate-token (`-Xms 512m`) forms
        // must be handled. The separate-token form is listed in
        // `VALUE_TAKING_OPTS`, so `insert_program_args_separator` keeps the
        // value adjacent to the flag here; if we only dropped the `-Xms`
        // token the bare value (`512m`) would survive and clap would mistake
        // it for the main-class positional, shifting/consuming the real
        // class name and the following program args. So when the value is a
        // separate token (`a == "-Xms"`), consume it too — mirroring the
        // `-Xshare`/`-Xverify`/`-Xbootclasspath`/`-Xlog` separate-token
        // branches. The flag is accepted-and-ignored, so nothing is emitted
        // either way.
        else if a.starts_with("-Xms") {
            if std::env::var_os("CRATONVM_DBG_ARGS").is_some() {
                eprintln!("[cratonvm] ignoring unimplemented HotSpot flag: {a}");
            }
            if a == "-Xms" && i + 1 < args.len() {
                // Separate-token form `-Xms 512m`: drop the value token too.
                i += 2;
            } else {
                // Inline form `-Xms512m` (value rides on the same token), or a
                // bare trailing `-Xms` with no value: drop just this token.
                i += 1;
            }
        }
        // GC selector: `-XX:+Use<Name>GC` -> `--XX:UseGc <Name>`. The collector
        // name (`<Name>` between `Use` and `GC`) is forwarded verbatim; the
        // config-apply step (`parse_gc_algorithm`) honours `G1` / `Generational`
        // and warns-and-falls-back for any other collector. Emitting a value
        // option (rather than acting here) means a later `-XX:+Use...GC`
        // overrides an earlier one via clap last-wins, matching HotSpot.
        //
        // `-XX:-UseG1GC` explicitly turns G1 off -> revert to the default
        // Generational. Other `-XX:-Use<Name>GC` ("do not use collector X")
        // select nothing and fall through to the silent-ignore arm below.
        //
        // Guarded so `-XX:+UseStringDeduplication`, `-XX:+UseCompressedOops`,
        // etc. (no `GC` suffix) do NOT match and keep their existing handling.
        else if let Some(name) = a
            .strip_prefix("-XX:+Use")
            .and_then(|core| core.strip_suffix("GC"))
            .filter(|core| !core.is_empty())
        {
            out.push("--XX:UseGc".into());
            out.push(name.to_string());
            i += 1;
        } else if a == "-XX:-UseG1GC" {
            out.push("--XX:UseGc".into());
            out.push("Generational".into());
            i += 1;
        }
        // G1 tuning knobs (§7 item 4). `-XX:Name=Value` → `--XX:<short> Value`;
        // honoured only under G1 (the config-apply step is G1-gated). Each maps
        // to a `G1CollectorConfig` field via `G1ConfigOverrides`.
        else if let Some(v) = a.strip_prefix("-XX:InitiatingHeapOccupancyPercent=") {
            out.push("--XX:IHOP".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:G1HeapRegionSize=") {
            out.push("--XX:G1RegionSize".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:MaxGCPauseMillis=") {
            out.push("--XX:MaxGCPause".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:MaxDirectMemorySize=") {
            out.push("--XX:MaxDirectMemorySize".into());
            out.push(v.to_string());
            i += 1;
        } else if a == "-XX:+UseStringDeduplication" {
            out.push("--XX:StringDedup".into());
            out.push("true".into());
            i += 1;
        } else if a == "-XX:-UseStringDeduplication" {
            out.push("--XX:StringDedup".into());
            out.push("false".into());
            i += 1;
        }
        // These `-XX` flags are not expressible as clap long names, so keep
        // them verbatim for `extract_hotspot_flags`, which runs after this
        // normalization stage.
        else if a == "-XX:+HeapDumpOnOutOfMemoryError"
            || a == "-XX:-HeapDumpOnOutOfMemoryError"
            || a.starts_with("-XX:HeapDumpPath=")
        {
            out.push(args[i].clone());
            i += 1;
        }
        // Any other `-XX:...` flag is a HotSpot tuning knob CratonVM does not
        // implement (`-XX:MetaspaceSize`, `-XX:MaxMetaspaceSize`,
        // `-XX:+ExitOnOutOfMemoryError`, …).
        // Recognized `-XX:` flags are rewritten by the branches above;
        // everything else is silently ignored so a Maven Surefire / Gradle
        // fork — which passes these unconditionally — launches instead of clap
        // aborting with "unexpected argument '-X'".
        else if a.starts_with("-XX:") {
            if std::env::var_os("CRATONVM_DBG_ARGS").is_some() {
                eprintln!("[cratonvm] ignoring unimplemented HotSpot flag: {a}");
            }
            i += 1;
        }
        // Any other single-dash `-X...` flag is a HotSpot knob CratonVM does
        // not implement (`-Xss<size>` thread stack size — Surefire/Gradle pass
        // this routinely — `-Xint`, `-Xbatch`, `-Xrs`, `-XshowSettings`,
        // `-Xnoclassgc`, …). The recognized value-taking `-X` spellings
        // (`-Xmx`/`-Xms`/`-Xshare`/`-Xverify`/`-Xbootclasspath`/`-Xlog`) are
        // rewritten by the branches above; everything else is accepted-and-
        // ignored here — same as `-XX:` — so a drop-in `java` launches instead
        // of clap aborting with "unexpected argument '-X...'". These remaining
        // `-X` flags are all the inline/no-value form, so dropping just this
        // single token is correct (HotSpot has no separate-token spelling for
        // the ones not handled above).
        //
        // [LOW arg-parse fix (3)] Deliberately drop ONLY this one token
        // (`i += 1`) and never consume the following token. An unrecognized
        // `-X` flag must not swallow the next token when that token is the
        // main-class name: e.g. `java -Xunknown Main` (or, if the separator
        // inserter did not run, `-Xint Main`) must still resolve `Main` as the
        // main class, not silently treat it as the unknown flag's value and
        // shift the real class into the program args. We make that invariant
        // explicit here: when the next token is a bare positional (no leading
        // `-`), it is the main-class candidate and is left for the normal
        // positional/`--` handling to claim — we never absorb it.
        else if a.starts_with("-X") {
            if std::env::var_os("CRATONVM_DBG_ARGS").is_some() {
                eprintln!("[cratonvm] ignoring unimplemented HotSpot flag: {a}");
                // Surface the swallow-avoidance: if a bare positional follows an
                // unknown `-X` flag, it is the main-class candidate and is left
                // untouched (we drop only the flag, never `i += 2`).
                if let Some(next) = args.get(i + 1) {
                    if !next.starts_with('-') && next != "--" {
                        eprintln!(
                            "[cratonvm] keeping following token '{next}' as a \
                             positional (unknown -X flag does not consume it)"
                        );
                    }
                }
            }
            // Drop ONLY this flag token; do NOT consume the next token. This is
            // what keeps an unknown `-X` flag from swallowing the main-class
            // name (see the comment above).
            i += 1;
        }
        // HotSpot VM-selection flags. Modern HotSpot accepts `-server` and
        // `-client` for compatibility (the server VM is effectively the only
        // implementation on current JDKs). WildFly's HostController launch
        // command still passes `-server`; accept-and-ignore it so the `java`
        // shim remains drop-in compatible instead of clap interpreting
        // `-server` as a short-option cluster and aborting on `-s`.
        else if a == "-server" || a == "-client" {
            if std::env::var_os("CRATONVM_DBG_ARGS").is_some() {
                eprintln!("[cratonvm] ignoring HotSpot VM selection flag: {a}");
            }
            i += 1;
        }
        // Assertion control flags: `-ea`/`-enableassertions[:<pkgname>...|:<classname>]`,
        // `-da`/`-disableassertions[...]`, `-esa`/`-enablesystemassertions`,
        // `-dsa`/`-disablesystemassertions`. CratonVM does not implement assertion
        // checking; silently ignore so Gradle/Maven forks that pass `-ea`
        // unconditionally don't crash clap (which would treat `-ea` as
        // short-option bundling `-e -a` and abort with "unexpected argument '-e'").
        else if a == "-ea"
            || a == "-da"
            || a == "-esa"
            || a == "-dsa"
            || a.starts_with("-ea:")
            || a.starts_with("-da:")
            || a.starts_with("-enableassertions")
            || a.starts_with("-disableassertions")
            || a.starts_with("-enablesystemassertions")
            || a.starts_with("-disablesystemassertions")
        {
            if std::env::var_os("CRATONVM_DBG_ARGS").is_some() {
                eprintln!("[cratonvm] ignoring assertion flag: {a}");
            }
            i += 1;
        } else {
            out.push(args[i].clone());
            i += 1;
        }
    }
    out
}

/// Pre-process raw command-line arguments to extract `-Dkey=value` system
/// property flags (Java-style) before handing the rest to clap.  Returns
/// `(filtered_args, system_properties)`.
fn extract_system_properties(raw: Vec<String>) -> (Vec<String>, Vec<(String, String)>) {
    let mut filtered = Vec::with_capacity(raw.len());
    let mut props = Vec::new();
    let mut past_separator = false;
    // [LOW arg-parse fix (2)] When the PREVIOUS token was a separate-token
    // value-taking option (`--classpath`, `--Xlog`, `--add-opens`, …), the
    // CURRENT token is that option's VALUE, not an option position. A value
    // may legitimately begin with `-D` (e.g. `--Xlog -Dspecial`, or a
    // classpath/module-path entry on an exotic path), and must NOT be hijacked
    // as a `-Dkey=value` system property — doing so both loses the option's
    // value and fabricates a bogus property. Track the value position and pass
    // it through verbatim. (`-D` itself is never a value-taking option name, so
    // a genuine `-Dkey=value` in an *option* position is still extracted.)
    let mut prev_was_value_opt = false;
    for arg in raw {
        // Anything after `--` is a program argument and must be preserved
        // verbatim, including bare `-Dfoo=bar` tokens that the Java program
        // (e.g. jboss-modules) wants to consume itself.
        if past_separator {
            filtered.push(arg);
            continue;
        }
        if arg == "--" {
            // Mark the separator boundary so subsequent `-D...` tokens are
            // preserved as program arguments. We still emit the `--` to
            // clap so it knows where program args begin (otherwise tokens
            // like `-mp` would be rejected as unknown short flags). The
            // `--` itself is stripped out of the program-args vector
            // after clap parsing, before the String[] is built for
            // Java's main().
            past_separator = true;
            prev_was_value_opt = false;
            filtered.push(arg);
            continue;
        }
        // This token is the value of a preceding value-taking option: emit it
        // unchanged even if it starts with `-D`, and do not treat it as an
        // option position.
        if prev_was_value_opt {
            prev_was_value_opt = false;
            filtered.push(arg);
            continue;
        }
        // Remember whether THIS token is a separate-token value-taking option,
        // so the next iteration knows the following token is its value.
        prev_was_value_opt = VALUE_TAKING_OPTS.contains(&arg.as_str());
        if let Some(kv) = arg.strip_prefix("-D") {
            if let Some((k, v)) = kv.split_once('=') {
                props.push((k.to_string(), v.to_string()));
            } else {
                // `-Dkey` with no value -> set to empty string (matches java behaviour)
                props.push((kv.to_string(), String::new()));
            }
        } else {
            filtered.push(arg);
        }
    }
    (filtered, props)
}

/// T6 CLI compat: HotSpot-style flags extracted from raw argv before clap
/// sees them.  clap's long-option format can't natively parse a `+`/`-`
/// sign inside the option name the way HotSpot's `-XX:+Foo` / `-XX:-Foo`
/// and `-agentlib:` spellings do.
///
/// Fields are populated by [`extract_hotspot_flags`] and consumed inside
/// `run()` to mutate `VmConfig` after clap returns.
#[derive(Debug, Default, Clone)]
struct HotspotFlags {
    /// `-XX:+HeapDumpOnOutOfMemoryError`. `-XX:-...` turns it off.
    heap_dump_on_oom: Option<bool>,
    /// `-XX:HeapDumpPath=<path>` companion.
    heap_dump_path: Option<String>,
    /// `-agentlib:<spec>`, `-agentpath:<spec>`, `-javaagent:<spec>` — the
    /// entire token (including prefix) is preserved so the existing
    /// `AgentRegistry::parse_agent_option` can consume it verbatim.
    agent_options: Vec<String>,
}

/// Pull HotSpot-style flags out of raw argv.
///
/// This runs before clap so the bare `-XX:+Foo` / `-agentlib:` spellings
/// don't confuse its parser. Flags we recognize are removed from the
/// returned vector; anything we don't recognize passes through unchanged
/// so clap can still reject unknown flags with a useful error.
fn extract_hotspot_flags(raw: Vec<String>) -> (Vec<String>, HotspotFlags) {
    let mut filtered = Vec::with_capacity(raw.len());
    let mut out = HotspotFlags::default();
    let mut past_separator = false;
    for arg in raw {
        // Tokens after `--` are program arguments — pass through unchanged.
        if past_separator {
            filtered.push(arg);
            continue;
        }
        if arg == "--" {
            past_separator = true;
            filtered.push(arg);
            continue;
        }
        match arg.as_str() {
            // Boolean toggles: -XX:+Foo / -XX:-Foo
            "-XX:+HeapDumpOnOutOfMemoryError" => out.heap_dump_on_oom = Some(true),
            "-XX:-HeapDumpOnOutOfMemoryError" => out.heap_dump_on_oom = Some(false),
            _ => {
                if let Some(rest) = arg.strip_prefix("-XX:HeapDumpPath=") {
                    out.heap_dump_path = Some(rest.to_string());
                } else if arg.starts_with("-agentlib:")
                    || arg.starts_with("-agentpath:")
                    || arg.starts_with("-javaagent:")
                {
                    out.agent_options.push(arg);
                } else {
                    filtered.push(arg);
                }
            }
        }
    }
    (filtered, out)
}

fn resolve_watchdog_timeout(
    stack_dump_on_timeout: Option<u64>,
    default_watchdog_sec: Option<&str>,
) -> Option<u64> {
    match stack_dump_on_timeout {
        Some(secs) if secs > 0 => Some(secs),
        Some(_) => None,
        None => default_watchdog_sec
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|secs| *secs > 0),
    }
}

fn run() -> Result<()> {
    // Install the pre-`std::process::exit` hook on `native_system_exit` /
    // `native_runtime_exit`. A silent `System.exit(N)` during real app boot
    // (e.g. Cassandra NodeTool's airline NPE catch path) otherwise tears the
    // process down before any downstream observer can print state. The hook
    // fires immediately before the process exits; when `CRATONVM_DBG_EXIT=1`
    // is set it dumps the dispatch-trace ring so the last Java method run
    // before the exit is visible in stderr for diagnosis. First-installer
    // wins (OnceLock), so installing it once here at the top of `run()` is
    // sufficient. (Installing the hook here only registers the closure; it
    // doesn't run it, so this can safely stay ahead of the tracing-subscriber
    // init below.)
    cratonvm_native_builtins::lang_system::set_pre_exit_hook(|code| {
        cleanup_staged_archive_copies();
        if std::env::var("CRATONVM_DBG_EXIT").ok().as_deref() == Some("1") {
            eprintln!("=== CRATONVM_DBG_EXIT: System.exit({code}) — dispatch trace ===");
            cratonvm_vm::dispatch_trace::dump_to_stderr_unconditional("pre-system-exit");
        }
        if std::env::var("CRATONVM_DBG_JIT_METHOD_STATS")
            .ok()
            .as_deref()
            == Some("1")
        {
            cratonvm_jit::tiered::dump_method_stats_to_stderr();
        }
    });

    // `java`-launcher positional semantics: insert a `--` separator right
    // after the program selector (`-jar <jar>` or the first bare main-class
    // token) so every token past it is treated as a program argument and
    // passed to the Java application verbatim — even `--help`, `--version`,
    // `--list-modules`. Without this, clap would intercept those anywhere.
    // Runs first so the explicit `--` it parks is honoured by every
    // downstream stage.
    let raw_argv: Vec<String> = expand_argfiles(std::env::args().collect());
    let argv: Vec<String> = insert_program_args_separator(raw_argv);
    // Extract -Dkey=value system properties before clap parsing
    let raw_args: Vec<String> = normalize_java_launcher_argv(argv);
    let (filtered_args, system_properties) = extract_system_properties(raw_args);
    // T6 CLI compat: strip HotSpot-style flags before clap so their
    // non-standard spellings (`-XX:+Foo`, `-agentlib:`) don't confuse it.
    let (filtered_args, hotspot_flags) = extract_hotspot_flags(filtered_args);
    let mut args = Args::parse_from(filtered_args);

    // Initialize tracing. B6: route WARN+ diagnostics to stderr so silent
    // swallow sites surface without polluting the program's stdout (which
    // Java's System.out also writes to).
    //
    // This used to be the very first thing in `run()`, ahead of CLI
    // parsing. It's now built after `Args::parse_from` above so the
    // `--print-gpu-decisions` handling below can consult `args`. Nothing in
    // between the old and new init point ever logs through `tracing`
    // (`expand_argfiles`, `insert_program_args_separator`,
    // `normalize_java_launcher_argv`, `extract_system_properties`,
    // `extract_hotspot_flags`, and `set_pre_exit_hook` all checked — the
    // pre-exit hook only *installs* a closure here, it doesn't run it), so
    // moving the subscriber install past them drops no log lines.
    let mut env_filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive(tracing::Level::WARN.into());

    // `--gpu --print-gpu-decisions` was a silent no-op: the decision lines
    // it promises (one per analyzer verdict) are emitted via
    // `tracing::info!`/`tracing::debug!` in `vm/src/runtime/offload.rs`
    // (`lookup_or_compile`'s "if self.print_decisions" block, and
    // `try_dispatch`'s "ran on device" / "fell back to CPU" lines) under
    // the module-path target `cratonvm_vm::runtime::offload` (crate name is
    // `cratonvm-vm` per vm/Cargo.toml `[package] name`; Cargo/rustc turns
    // `-` into `_` for the actual crate identifier used as a tracing
    // target). The WARN-only default filter above swallows all of that
    // unless the user separately exports RUST_LOG — the flag shouldn't
    // require knowing that.
    //
    // Add a low-priority default that opens up this one target. This is
    // *not* a global verbosity bump: EnvFilter picks the most specific
    // matching directive per callsite by target length, independent of add
    // order, so any RUST_LOG directive for a different target (crate-wide,
    // a sibling module, or this same target at a different level written
    // with a different target string) composes normally on top of this.
    // The one exception is a RUST_LOG directive for this *exact* target
    // string (e.g. `RUST_LOG=cratonvm_vm::runtime::offload=error`): that
    // ties in specificity with the directive added below, and EnvFilter
    // resolves same-target ties by last-added-wins rather than stacking
    // them — since this runs after `from_default_env()` has already parsed
    // RUST_LOG, this directive would win that narrow case. Not worth extra
    // machinery to special-case an already-obscure override.
    //
    // NOTE: the workspace pins `tracing` with the `release_max_level_info`
    // feature (Cargo.toml ~line 46), which compiles `tracing::debug!` down
    // to a no-op in release builds. So in a release build only the
    // INFO-level analyzer-verdict lines from `lookup_or_compile` can ever
    // appear here, by design/compile-time elision — the DEBUG-level "ran on
    // device" / "fell back to CPU" lines in `try_dispatch` simply aren't in
    // the binary to enable. Asking for `debug` below is still correct: it's
    // the right ceiling for debug builds, and a harmless no-op ceiling in
    // release builds.
    #[cfg(feature = "gpu")]
    {
        if args.print_gpu_decisions {
            env_filter = env_filter.add_directive("cratonvm_vm::runtime::offload=debug".parse()?);
        }
    }

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .init();

    // --nojit: surface as the CRATONVM_DISABLE_JIT env var so the
    // already-existing kill-switch in `vm/src/runtime/env_cache.rs`
    // observes it on first read. Must happen *before* any code path
    // that calls `env_cache::disable_jit()` (interpreter / JIT
    // dispatcher) — the cache uses `OnceLock`, so a late `set_var`
    // would be ignored. Setting it here, immediately after clap
    // parsing, is well before `Vm::new(config)` runs any bytecode.
    //
    // We're still on the main thread with no cratonvm-spawned threads
    // yet, so the documented `set_var` race against concurrent readers
    // (which is why it became unsafe in edition 2024) cannot fire here.
    if args.nojit {
        std::env::set_var("CRATONVM_DISABLE_JIT", "1");
    }

    // --enable-native-access: open the process-wide Panama FFI gate
    // (`native-builtins/src/panama.rs::NATIVE_ACCESS_ENABLED`), which is
    // default-CLOSED after the security fix. Mirrors the modern JDK
    // `--enable-native-access` flag; the optional module value is
    // accepted-and-ignored because the gate is a single coarse
    // process-wide toggle rather than a per-module grant.
    //
    // Applied here, immediately after clap parsing and alongside the other
    // global config toggles, so it runs on the main thread well before
    // `Vm::new(config)` executes any bytecode — no Panama downcall,
    // `MemorySegment.ofAddress`, or `reinterpret` can observe the closed
    // gate once the user opted in.
    if args.enable_native_access.is_some() {
        cratonvm_native_builtins::panama::set_native_access_enabled(true);
    }

    // -XX:+ShowCodeDetailsInExceptionMessages (JEP 358): publish to the
    // env_cache so the interpreter's helpful-NPE opcode gate observes it on
    // first read, before Vm::new(config) runs any bytecode (same OnceLock
    // timing rationale as --nojit above). CRATONVM_HELPFUL_NPE_OPCODES, if set,
    // still overrides it.
    // Tri-state: an absent flag resolves to the default-on (matching
    // `VmConfig::default`); `-XX:-...` (→ `=false`) opts out.
    cratonvm_vm::runtime::env_cache::set_show_code_details_in_exception_messages(
        args.show_code_details_in_exception_messages.unwrap_or(true),
    );

    // GPU handlers — only compiled when the `gpu` Cargo feature is on.
    // Without the feature, the CPU execution path below is reached
    // unconditionally and unchanged.
    #[cfg(feature = "gpu")]
    {
        if args.gpu_info {
            match cuda_bridge::probe() {
                Ok(caps) => {
                    println!(
                        "device {}: {} (sm_{}{}), {:.2} GiB",
                        caps.ordinal,
                        caps.name,
                        caps.compute_major,
                        caps.compute_minor,
                        (caps.total_global_mem as f64) / (1024.0 * 1024.0 * 1024.0)
                    );
                }
                Err(e) => {
                    println!("no CUDA device available: {e}");
                }
            }
            return Ok(());
        }

        if args.gpu {
            match cuda_bridge::probe() {
                Ok(caps) => {
                    info!(
                        "gpu offload enabled on device {}: {} (sm_{}{})",
                        args.gpu_device, caps.name, caps.compute_major, caps.compute_minor
                    );
                }
                Err(e) => {
                    eprintln!(
                        "[cratonvm-cli] --gpu requested but no CUDA driver available ({e}); \
                         running on CPU"
                    );
                    args.gpu = false;
                }
            }
        }
    }

    // Strip a literal `--` separator that clap parked in the trailing
    // positional list. We pass `--` through to clap so it knows where
    // program args begin (so tokens like `-mp` aren't mis-parsed as
    // short flags), but the Java program must not see `--` itself in
    // its String[] args. Without this, jboss-modules' Main.main reads
    // args[0] == "--" instead of "-mp" and fails inside getServiceName
    // with NullPointerException.
    if args.class_name.as_deref() == Some("--") {
        args.class_name = None;
    }
    // Preserve every `--` that survives clap, including a trailing one.
    //
    // clap already consumes the first lone `--` as its option-parsing
    // terminator, so in the normal-class and `-jar` launch modes no launcher
    // `--` ever reaches `args.args` — any `--` left there is genuinely a
    // program argument and MUST be delivered to `main` verbatim (stock
    // `java Main a -- b` gives the program `["a", "--", "b"]`, and stock
    // `java Main a --` gives the program `["a", "--"]`; many CLI tools use
    // their own `--` end-of-options convention).
    //
    // [LOW arg-parse fix (1)] A previous version unconditionally popped a
    // trailing `--` here, on the theory that the JBoss-Modules `-mp` path
    // (`normalize_java_launcher_argv` prepends an extra `--` so `-mp` isn't
    // mis-parsed as the `-m`/`-p` short-flag cluster) could leak a spurious
    // trailing `--` as the LAST element of `args.args`. That artifact no
    // longer occurs: `-mp` is in `VALUE_TAKING_OPTS`, so
    // `insert_program_args_separator` consumes its operand instead of
    // injecting a separator, and the `-mp` token itself lands in
    // `class_name` (not `args.args`) — see the
    // `launcher_trailing_double_dash_artifact_is_popped` regression test.
    // The unconditional pop therefore had no remaining legitimate target and
    // instead silently dropped a genuine user trailing `--` (e.g.
    // `java Main a --`), violating JDK launcher semantics. Leave a trailing
    // `--` in place so it reaches the program's `String[] args` verbatim.

    // Validate: exactly one of class_name or --jar must be provided
    if args.class_name.is_none() && args.jar.is_none() {
        bail!("No class name or --jar specified. Usage: cratonvm <class> or cratonvm --jar <file.jar>");
    }
    // When both --jar and positional arguments are given, treat the
    // positionals as program args (Java-style).  This matches the stock
    // `java -jar foo.jar arg1 arg2` behaviour.
    if args.class_name.is_some() && args.jar.is_some() {
        // Shuffle: move class_name into args[0], then args, then the rest.
        let cn = args.class_name.take().unwrap();
        let mut new_args = vec![cn];
        new_args.extend(std::mem::take(&mut args.args));
        args.args = new_args;
    }

    // Resolve class name and classpath based on launch mode. If a WAR/EAR is
    // staged as a temporary `.jar` copy, keep the cleanup guard alive until
    // `run()` exits so lazy class loading can still read it.
    let mut _staged_archive_cleanup: Option<StagedArchiveCleanup> = None;
    let (class_name, classpath) = if let Some(jar_path_str) = &args.jar {
        // -jar mode: read Main-Class from manifest, build classpath from JAR + manifest Class-Path
        let jar_path = std::path::Path::new(jar_path_str);
        if !jar_path.exists() {
            bail!("JAR file not found: {}", jar_path.display());
        }

        let manifest = ClassPath::read_jar_manifest(jar_path)
            .ok_or_else(|| anyhow::anyhow!("Cannot read manifest from {}", jar_path.display()))?;

        // JN3: ClassPath::new only accepts entries whose extension is `.jar`
        // (or `.jmod`/`modules`). WARs, EARs, and other Java archive types
        // are silently dropped — so `--jar jenkins.war` would never have
        // its contents indexed and `executable.Main` (the launcher class
        // declared in `Main-Class`) could not be resolved.
        //
        // Work around this in the CLI by materialising any non-`.jar`
        // archive as a sibling temp file with a `.jar` extension and
        // adding that path to the classpath instead. The original `jar_path`
        // is still used for manifest parsing (which doesn't care about the
        // extension), and the `Class-Path` manifest header is resolved
        // relative to the original file's parent so sibling lookups still
        // work.
        let (cp_entry_for_archive, staged_cleanup) = classpath_entry_for_archive(jar_path)?;
        _staged_archive_cleanup = staged_cleanup;

        // Build classpath: JAR (or staged .jar copy) itself + manifest Class-Path entries.
        // The manifest's Class-Path header is still resolved relative to the
        // user-supplied path so sibling JARs are found at their real locations.
        let mut cp = vec![cp_entry_for_archive.to_string_lossy().into_owned()];
        cp.extend(manifest.resolve_class_path(jar_path));

        // KC26: For Quarkus applications, the RunnerClassLoader normally loads
        // classes from jars listed in quarkus-application.dat. Since we can't
        // fully emulate that complex bootstrap, add all application jars to
        // the VM classpath so ClassLoader.loadClass can find them. Canonicalise
        // jar_path first so a bare basename (user cd'd into lib/) still resolves
        // its parent. Probe BOTH the jar's own dir AND its parent, since
        // Keycloak packaging puts quarkus-run.jar in lib/ (one level deep),
        // whereas the canonical Quarkus packaging puts it at the project root.
        //
        // PERF: the multi-dir `canonicalize` + `read_dir` walk below is
        // only meaningful for Quarkus packagings, but it used to run on
        // EVERY `-jar` startup — ~10 stat/read_dir syscalls even for a
        // trivial HelloWorld jar. Gate it behind a cheap "is this actually
        // a Quarkus app" sniff so non-Quarkus jars skip the walk entirely.
        // The sniff is a handful of `Path::exists()` stats next to the jar
        // (and one dir-parent up), which is far cheaper than canonicalising
        // and reading 5 candidate dirs across 2 roots. Real Quarkus apps
        // always ship one of these signature artifacts, so their behaviour
        // is unchanged.
        if quarkus_signature_present(jar_path) {
            let canon_jar =
                std::fs::canonicalize(jar_path).unwrap_or_else(|_| jar_path.to_path_buf());
            let jar_dir = canon_jar.parent().map(|p| p.to_path_buf());
            let mut roots = Vec::new();
            if let Some(d) = jar_dir.as_ref() {
                roots.push(d.clone());
                if let Some(pp) = d.parent() {
                    roots.push(pp.to_path_buf());
                }
            }
            let app_dirs = ["app", "quarkus", "lib/main", "lib/boot", "lib/deployment"];
            let mut seen = std::collections::HashSet::new();
            for root in &roots {
                for dir_name in &app_dirs {
                    let dir = root.join(dir_name);
                    let canon_dir = std::fs::canonicalize(&dir).unwrap_or(dir.clone());
                    if !seen.insert(canon_dir.clone()) {
                        continue;
                    }
                    if canon_dir.is_dir() {
                        if let Ok(entries) = std::fs::read_dir(&canon_dir) {
                            for entry in entries.flatten() {
                                let p = entry.path();
                                if p.extension().map_or(false, |e| e == "jar") {
                                    cp.push(p.to_string_lossy().into_owned());
                                }
                            }
                        }
                    }
                }
            }
        }

        let main_class = manifest.main_class.ok_or_else(|| {
            anyhow::anyhow!("no main manifest attribute, in {}", jar_path.display())
        })?;

        if args.classpath.is_some() {
            eprintln!("Warning: -cp/-classpath is ignored when --jar is used");
        }

        let class_name = main_class.replace('.', "/");
        let cp = expand_aggregate_jars(cp);
        (class_name, cp)
    } else {
        // Class name mode
        let cn = args.class_name.as_ref().unwrap().replace('.', "/");
        let cp = if let Some(cp) = args.classpath.as_ref() {
            VmConfig::parse_classpath(cp)
        } else if let Ok(env_cp) = std::env::var("CLASSPATH") {
            VmConfig::parse_classpath(&env_cp)
        } else {
            Vec::new()
        };
        let cp = expand_aggregate_jars(cp);
        (cn, cp)
    };

    // -Xverify:* takes precedence over --noverify when both are present
    // (matches HotSpot, where the more specific flag wins).
    let xverify_mode = if let Some(spec) = args.xverify.as_deref() {
        match cratonvm_vm::config::XverifyMode::parse(spec) {
            Some(m) => Some(m),
            None => {
                eprintln!(
                    "Warning: ignoring unknown -Xverify mode {spec:?}; expected none|remote|all"
                );
                None
            }
        }
    } else {
        None
    };

    // Task #53: the launcher prefers real-JDK boot (JMOD) when a JDK is
    // detected on the host (JAVA_HOME / CRATONVM_JAVA_HOME / `java` on
    // PATH); otherwise it falls back to the synthetic stubs. The library
    // path (`VmConfig::default`) stays synthetic so embedded callers and
    // the in-tree test suite are unaffected.
    let mut config = VmConfig::with_host_jdk_default()
        .with_classpath(classpath)
        .with_verbose_class_loading(args.verbose_class)
        .with_verbose_gc(args.verbose_gc)
        .with_skip_verification(args.noverify);

    // Record the user-supplied `-jar` path so `java.class.path` is set
    // to the bare jar (HotSpot contract), not the manifest-expanded
    // transitive classpath. See `VmConfig::launcher_jar` for the full
    // rationale — Liberty/Quarkus boot launchers reflect on this.
    if let Some(jar) = args.jar.as_deref() {
        config = config.with_launcher_jar(jar.to_string());
    }

    if let Some(mode) = xverify_mode {
        config = config.with_xverify_mode(mode);
    }

    // Forward the GPU-offload CLI flags into VmConfig. Only compiled
    // when the `gpu` Cargo feature is on; without the feature these
    // fields do not exist on VmConfig (see vm/src/config.rs).
    #[cfg(feature = "gpu")]
    {
        config.gpu_offload_enabled = args.gpu;
        config.gpu_device_ordinal = args.gpu_device;
        config.gpu_min_work = args.gpu_min_work;
        config.print_gpu_decisions = args.print_gpu_decisions;
    }

    if let Some(bcp) = &args.boot_classpath {
        config = config.with_boot_classpath(VmConfig::parse_classpath(bcp));
    }
    if let Some(jh) = &args.java_home {
        // Fail fast when --java-home points at a path that doesn't exist.
        // Without this, `resolve_java_home` silently returns None, the boot
        // classpath ends up empty, and the first JDK-class reference (e.g.
        // `INVOKESTATIC java/lang/Boolean.parseBoolean`) surfaces as a
        // confusing `NoSuchMethodError` instead of a clear configuration error.
        let p = std::path::Path::new(jh);
        if !p.is_dir() {
            anyhow::bail!(
                "--java-home path does not exist or is not a directory: {jh}\n\
                 Provide a valid JDK installation (must contain `jmods/` or `lib/modules`)."
            );
        }
        config = config.with_java_home(jh.clone());
    }

    // Container/cgroup awareness (HotSpot's `-XX:+UseContainerSupport`, on by
    // default). When enabled, read the cgroup memory/CPU limits once so the
    // ergonomic default heap is sized off the container limit and
    // `Runtime.availableProcessors()` honors the CPU quota. `-XX:-UseContainer
    // Support` skips detection entirely, so every limit reverts to host values.
    // On non-Linux hosts `detect_container()` reports "not containerized" with
    // all limits `None`, so this is a no-op there.
    let container_info = if args.disable_container_support {
        None
    } else {
        Some(cratonvm_vm::runtime::container::detect_container())
    };
    let container_mem_limit = container_info.as_ref().and_then(|i| i.memory_limit);
    // CPU count for Runtime.availableProcessors() / the JMX OS bean. Only set
    // when a cgroup quota was actually detected; `None` ⇒ report host count.
    config.container_effective_processors =
        container_info.as_ref().and_then(|i| i.effective_cpu_count);

    if let Some(max_heap_str) = &args.max_heap {
        let size = parse_size(max_heap_str)
            .with_context(|| format!("Invalid heap size: {max_heap_str}"))?;
        config = config.with_max_heap_size(size);
    } else if let Some(ergo) = ergonomic_default_max_heap(container_mem_limit) {
        // No explicit -Xmx: size the heap like a stock JDK (1/4 of host RAM, or
        // of the cgroup limit inside a container) instead of the fixed 256 MB
        // library default, so Spring/Mockito/JUnit workloads don't thrash GC
        // into a pseudo-hang. See `ergonomic_default_max_heap`.
        if args.verbose_gc {
            let basis = if container_mem_limit.is_some() {
                "1/4 container memory limit"
            } else {
                "1/4 physical RAM"
            };
            eprintln!(
                "[cratonvm] ergonomic default max heap: {} MB ({basis}; \
                 set -Xmx or CRATONVM_DEFAULT_HEAP_ERGONOMICS=0 to override)",
                ergo / (1024 * 1024)
            );
        }
        config = config.with_max_heap_size(ergo);
    }

    // CDS configuration
    if let Some(archive_path) = &args.shared_archive_file {
        config.shared_archive_file = Some(archive_path.clone());
    }
    config.cds_mode = match args.xshare.as_str() {
        "on" => cratonvm_vm::config::CdsMode::On,
        "auto" => cratonvm_vm::config::CdsMode::Auto,
        "dump" => cratonvm_vm::config::CdsMode::Dump,
        "off" => cratonvm_vm::config::CdsMode::Off,
        other => {
            // Warn on a typo rather than silently defaulting to Off.
            eprintln!(
                "Warning: ignoring unknown -Xshare mode {other:?}; expected on|auto|dump|off"
            );
            cratonvm_vm::config::CdsMode::Off
        }
    };

    // Synthetic JDK override (task #53): the launcher already picked the
    // host-driven default via `with_host_jdk_default()` above (real JDK
    // when detected, synthetic otherwise). Three cases override that:
    //
    //   * `--synthetic-jdk` flag → force synthetic (explicit opt-in)
    //   * `--java-home` CLI arg → force real-JDK (user pointed at a JDK)
    //   * `JAVA_HOME` already on `VmConfig` → force real-JDK
    //
    // The `--synthetic-jdk` flag wins over `--java-home` so users can
    // explicitly compare synthetic vs. real-JDK behaviour against the
    // same install.
    if args.synthetic_jdk {
        config.use_synthetic_jdk = true;
    } else if args.java_home.is_some() || config.java_home.is_some() {
        config.use_synthetic_jdk = false;
    }

    // AOT configuration
    config.aot_mode = match args.aot_mode.as_str() {
        "training" => cratonvm_vm::config::AotMode::Training,
        "production" => cratonvm_vm::config::AotMode::Production,
        "off" => cratonvm_vm::config::AotMode::Off,
        other => {
            // Warn on a typo rather than silently defaulting to Off.
            eprintln!(
                "Warning: ignoring unknown -XX:AOTMode value {other:?}; \
                 expected off|training|production"
            );
            cratonvm_vm::config::AotMode::Off
        }
    };
    if let Some(cache_path) = &args.aot_cache {
        // AOTCache serves as input in production mode and output in training mode
        match config.aot_mode {
            cratonvm_vm::config::AotMode::Production => {
                config.aot_cache_input = Some(cache_path.clone());
            }
            cratonvm_vm::config::AotMode::Training => {
                if config.aot_cache_output.is_none() {
                    config.aot_cache_output = Some(cache_path.clone());
                }
            }
            _ => {
                config.aot_cache_input = Some(cache_path.clone());
            }
        }
    }
    if let Some(output_path) = &args.aot_cache_output {
        config.aot_cache_output = Some(output_path.clone());
    }

    // Garbage-collector selection (`-XX:+UseG1GC` / `-XX:-UseG1GC` / any other
    // `-XX:+Use*GC`, normalized to `--XX:UseGc <name>`). Absent → keep the
    // default (`Generational`, the safety net during collector maturation). A
    // recognized selector (`g1` | `z`/`zgc` | `generational`) sets `gc_algorithm`; an
    // unsupported collector (Serial/Parallel/Shenandoah/Epsilon) warns and
    // falls back to Generational so a `java` drop-in keeps booting. G1 is
    // wired into the safepoint driver; ZGC-real is a non-moving STW backend. See
    // docs/feature-designs/concurrent-gc-maturation.md §3.1.
    if let Some(sel) = &args.gc_selector {
        match cratonvm_vm::config::parse_gc_algorithm(sel) {
            Some(algo) => config.gc_algorithm = algo,
            None => {
                eprintln!(
                    "Warning: unsupported garbage collector -XX:+Use{sel}GC; CratonVM \
                     implements G1 (-XX:+UseG1GC), ZGC-real (-XX:+UseZGC), and the \
                     default Generational collector. Falling back to Generational."
                );
                config.gc_algorithm = cratonvm_vm::config::GcAlgorithm::Generational;
            }
        }
    } else if args.nojit {
        // Interpreter-only workloads can maintain a large cohort in rapid
        // native/socket transitions. The legacy Generational collector's
        // cooperative STW protocol can strand that cohort before the next
        // bytecode poll (Tomcat's JNDI realm shutdown is a reproducer). G1
        // owns the same workload without that transition deadlock. Keep an
        // explicit collector choice authoritative; this is only the no-JIT
        // default.
        config.gc_algorithm = cratonvm_vm::config::GcAlgorithm::G1;
    }

    // G1 tuning knobs (§7 item 4). Parsed from the normalized `--XX:*` value
    // args and stored on the config; applied to `G1CollectorConfig` only when
    // the G1 backend is selected (vm_init via `G1ConfigOverrides`). A malformed
    // value warns and is ignored (keeps the collector default), matching the
    // lenient-with-warning policy used for the GC selector.
    if let Some(s) = &args.g1_ihop {
        match s.parse::<u8>() {
            Ok(p) if (1..=100).contains(&p) => config.g1_ihop_percent = Some(p),
            _ => eprintln!(
                "Warning: ignoring -XX:InitiatingHeapOccupancyPercent={s} (expected 1..=100)"
            ),
        }
    }
    if let Some(s) = &args.g1_region_size {
        match parse_size(s) {
            Some(sz) if sz > 0 => config.g1_region_size = Some(sz),
            _ => eprintln!("Warning: ignoring -XX:G1HeapRegionSize={s} (expected a byte size)"),
        }
    }
    if let Some(s) = &args.g1_max_pause {
        match s.parse::<u64>() {
            Ok(ms) if ms > 0 => config.g1_max_gc_pause_ms = Some(ms),
            _ => eprintln!(
                "Warning: ignoring -XX:MaxGCPauseMillis={s} (expected a positive integer)"
            ),
        }
    }
    if let Some(s) = &args.g1_string_dedup {
        config.g1_string_dedup = Some(s == "true");
    }
    if let Some(s) = &args.max_direct_memory {
        match parse_size(s) {
            Some(sz) if sz > 0 => config.max_direct_memory_size = Some(sz),
            _ => eprintln!("Warning: ignoring -XX:MaxDirectMemorySize={s} (expected a byte size)"),
        }
    }

    // Missing native audit. NEW-10: `--dump-missing-natives FILE` and
    // T2.1.3: `--dump-missing-natives-grouped FILE` both implicitly
    // enable audit mode so the user doesn't have to pass the flag
    // separately.
    config.audit_missing_natives = args.audit_missing_natives
        || args.dump_missing_natives.is_some()
        || args.dump_missing_natives_grouped.is_some();

    // -XX:±ShowCodeDetailsInExceptionMessages — record on the VmConfig too, so
    // the resolved config reflects the flag (env_cache was already set from
    // `args` above for the interpreter gate). Absent → keep the default-on
    // `VmConfig` value; an explicit flag (`=true`/`=false`) overrides it.
    config.show_code_details_in_exception_messages = args
        .show_code_details_in_exception_messages
        .unwrap_or(config.show_code_details_in_exception_messages);

    // JDWP debug server
    if let Some(port) = args.jdwp_port {
        config = config.with_jdwp(port, args.jdwp_suspend);
    }

    // JPMS module system flags
    if let Some(mp) = &args.module_path {
        config.module_path = VmConfig::parse_classpath(mp);
    }
    for s in &args.add_reads {
        config.add_reads.extend(VmConfig::parse_add_reads(s));
    }
    for s in &args.add_exports {
        if let Some(tuple) = VmConfig::parse_add_exports(s) {
            config.add_exports.push(tuple);
        } else {
            eprintln!(
                "Warning: invalid --add-exports format: {s} (expected module/package=target)"
            );
        }
    }
    for s in &args.add_opens {
        if let Some(tuple) = VmConfig::parse_add_exports(s) {
            config.add_opens.push(tuple);
        } else {
            eprintln!("Warning: invalid --add-opens format: {s} (expected module/package=target)");
        }
    }
    config.add_modules = args.add_modules.clone();

    // System properties from -Dkey=value flags
    config.system_properties = system_properties;

    // Container support (enabled by default, disabled with --XX:-UseContainerSupport)
    if args.disable_container_support {
        config = config.with_container_support(false);
    }

    // Unified logging (-Xlog)
    if let Some(xlog_spec) = &args.xlog {
        config = config.with_xlog_spec(xlog_spec.clone());
    }

    // T6.1.2 — `-XX:+HeapDumpOnOutOfMemoryError` / `-XX:HeapDumpPath=...`.
    // The interpreter's OOM path already honors these config fields (see
    // `maybe_dump_heap_on_oom` in `vm/src/runtime/interpreter.rs`); we
    // only need to thread the CLI values through.
    if let Some(flag) = hotspot_flags.heap_dump_on_oom {
        config.heap_dump_on_oom = flag;
    }
    if let Some(path) = hotspot_flags.heap_dump_path.clone() {
        config.heap_dump_path = Some(path);
    }

    // T6.3.3 — `-agentlib:`, `-agentpath:`, `-javaagent:`. The options
    // are stashed here and handed to the JVMTI `AgentRegistry` at VM
    // startup; registering them in a dedicated config field lets the
    // startup path load them in the canonical Agent_OnLoad order.
    //
    // WP2.4-C: split out the `-javaagent:` JAR specs into Java-agent
    // descriptors. The two sets are disjoint by syntax (`-javaagent:`
    // points at a JAR + `Premain-Class` manifest attribute, while
    // `-agentlib:` / `-agentpath:` point at native libraries with a
    // C `Agent_OnLoad` entry point) so we can route them independently.
    let mut java_agents: Vec<cratonvm_vm::runtime::agent_loader::LoadedAgent> = Vec::new();
    if !hotspot_flags.agent_options.is_empty() {
        let mut native_agent_opts: Vec<String> = Vec::new();
        for opt in &hotspot_flags.agent_options {
            if opt.starts_with("-javaagent:") {
                match cratonvm_vm::runtime::agent_loader::parse_javaagent_spec(opt) {
                    Ok(agent) => java_agents.push(agent),
                    Err(e) => {
                        // Per the `java.lang.instrument` package spec, a
                        // misconfigured agent should fail loudly enough
                        // that the operator notices, but the VM should
                        // still try to run the application — match the
                        // HotSpot warn-and-continue behaviour.
                        eprintln!("Warning: ignoring {opt}: {e}");
                    }
                }
            } else {
                native_agent_opts.push(opt.clone());
            }
        }
        config.jvmti_agent_options = native_agent_opts;
    }

    // Validate the class name before proceeding
    validate_class_name(&class_name)?;

    info!("Starting CratonVM");
    info!("Class: {class_name}");
    info!("Classpath: {:?}", config.classpath);

    // Create VM and execute main method
    let mut vm = Vm::new(config);

    // BUG-03 — publish the main thread's TLAB address now that `vm` is at its
    // final, address-stable location on the `main-vm` thread. This lets the
    // cross-thread STW JIT root scan recover the main thread's un-retired
    // reserved TLAB tail if it is forcibly stopped while executing JIT code
    // (workers / foreign threads publish theirs at their own start). Casting to
    // a raw pointer ends the borrow immediately, so the subsequent registry
    // call does not conflict.
    {
        let main_tlab = &vm.main_thread.tlab as *const _ as usize;
        let main_tid = vm.main_thread.thread_id;
        vm.shared
            .threads
            .thread_registry
            .set_tlab_addr(main_tid, main_tlab);
        // xt-hardening (2026-07-03): publish main's OS thread id for the
        // takeover's counted-set excusal (workers publish at their start).
        vm.shared
            .threads
            .thread_registry
            .set_os_tid_current(main_tid);
    }

    // T19.H1: optional watchdog that dumps interpreter frames and aborts when
    // the user explicitly bounds execution. Normal Java programs may be
    // long-running services, so no watchdog is armed by default. Set
    // `--stack-dump-on-timeout=N` or `CRATONVM_DEFAULT_WATCHDOG_SEC=N` to opt
    // in; `--stack-dump-on-timeout=0` and `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`
    // disable the env-default path.
    //
    // Native-call and dispatch rings are recorded only for explicit diagnostic
    // requests because they add hot-path work on every native-method entry.
    let explicit_watchdog = matches!(args.stack_dump_on_timeout, Some(s) if s > 0);
    let ring_recording_requested = explicit_watchdog
        || std::env::var("CRATONVM_ENABLE_NATIVE_RING").ok().as_deref() == Some("1");
    let default_watchdog_env = if std::env::var("CRATONVM_DISABLE_DEFAULT_WATCHDOG")
        .ok()
        .as_deref()
        == Some("1")
    {
        None
    } else {
        std::env::var("CRATONVM_DEFAULT_WATCHDOG_SEC").ok()
    };
    let effective_watchdog =
        resolve_watchdog_timeout(args.stack_dump_on_timeout, default_watchdog_env.as_deref());
    // Shared "run() completed" flag for the stack-dump watchdog. When `run()`
    // returns (normally OR via `?`/early-return), the RAII guard below sets
    // this to `true`; the watchdog checks it after its deadline sleep and
    // again immediately before `abort()`, exiting cleanly without aborting a
    // run that finished just after a tight deadline. Only meaningful when a
    // watchdog is actually armed, but harmless otherwise.
    let watchdog_completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // RAII guard: its Drop runs on every exit path of `run()` (normal return,
    // early `return`, and `?` propagation), so the completed flag is always
    // set once we leave this function. Instantiated right after the watchdog
    // is spawned (see below).
    struct WatchdogCompletionGuard {
        flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    impl Drop for WatchdogCompletionGuard {
        fn drop(&mut self) {
            self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    // Holds the guard alive until `run()` returns. `None` when no watchdog is
    // armed (nothing to cancel).
    let mut _watchdog_completion_guard: Option<WatchdogCompletionGuard> = None;

    if ring_recording_requested {
        cratonvm_native_api::native_ring::enable(true);
        vm.shared.natives.native_methods.flush_native_ring_names();
        cratonvm_vm::dispatch_trace::enable();
    }

    if let Some(secs) = effective_watchdog {
        // Enable the native-call ring buffer so the watchdog's "0 Java
        // threads dumped" fallback can show the last ~64 native methods
        // every thread entered. Without this, the ring's
        // `dump_to_stderr` reports "recording disabled" and a hang in
        // pure Rust runtime code has no actionable diagnostic.
        // Recording cost (single relaxed AtomicBool load on entry, plus
        // a parking_lot::Mutex when set) is negligible per call but adds
        // up across a full run, so we only arm it when a diagnostic was
        // EXPLICITLY requested (see `ring_recording_requested` above). The
        // A watchdog armed through the env default still aborts + dumps Java
        // frames; ring detail is reserved for runs that asked for it.
        if ring_recording_requested {
            cratonvm_native_api::native_ring::enable(true);
            vm.shared.natives.native_methods.flush_native_ring_names();
            // T19.H1 — also enable the dispatch-trace ring. The native-call
            // ring records only opaque fn-pointers from two dispatch sites;
            // the dispatch trace records *named* class.method.desc for every
            // bytecode-method entry and every `safe_native_call`, which is
            // the actionable diagnostic for "main thread is in native code".
            cratonvm_vm::dispatch_trace::enable();
        }
        let shared_for_watchdog = std::sync::Arc::clone(&vm.shared);
        // RKC16N.5 — capture the audit-dump paths into the watchdog
        // thread so a hung run still produces a missing-natives
        // census. Without this, the only flush path is the
        // clean-shutdown branch at the end of `main()`, and every
        // watchdog-killed run loses the JSON we use for KC16/KC26
        // boot debugging.
        let watchdog_dump_path = args.dump_missing_natives.clone();
        let watchdog_dump_grouped_path = args.dump_missing_natives_grouped.clone();
        let watchdog_completed_for_thread = std::sync::Arc::clone(&watchdog_completed);
        std::thread::Builder::new()
            .name("cratonvm-stack-watchdog".into())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(secs));
                // Cancellation: if `run()` already finished (e.g. it completed
                // just after a tight deadline), don't dump or abort — exit
                // cleanly. Checked here right after the deadline sleep, and
                // again immediately before `abort()` below.
                if watchdog_completed_for_thread.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                // Banner first so the user can tell we got this far.
                eprintln!(
                    "=== T19.H1 watchdog: deadline of {secs}s elapsed; \
                     requesting thread stack dumps ==="
                );
                shared_for_watchdog.request_stack_dump();

                // Give interpreter threads a short grace period to hit
                // the hot-loop check and flush their dumps. We poll the
                // ack counter so we exit the grace period as soon as
                // every reachable thread has responded.
                let grace_start = std::time::Instant::now();
                let grace = std::time::Duration::from_secs(3);
                while grace_start.elapsed() < grace {
                    let acks = shared_for_watchdog.stack_dump_ack_count();
                    // We can't know the exact thread count without
                    // holding the registry lock, but the registry only
                    // grows, so reaching any positive ack count and
                    // then stabilising is the signal we want. Short
                    // sleeps keep the abort latency bounded.
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    let acks2 = shared_for_watchdog.stack_dump_ack_count();
                    if acks > 0 && acks == acks2 {
                        break;
                    }
                }

                let total_acks = shared_for_watchdog.stack_dump_ack_count();
                eprintln!(
                    "=== T19.H1 watchdog: {total_acks} thread(s) dumped; \
                     aborting process ==="
                );

                // KC-watchdog-native: when zero Java threads ack'd a dump,
                // every interpreter thread is parked in native (Rust) code
                // — the most common cause being a JNI / native-method loop
                // or a deadlock on a Rust mutex inside the runtime. The
                // Java-frame dump produces nothing actionable, so fall
                // back to:
                //   1. The watchdog thread's own native backtrace (cheap
                //      and tells you WHERE in the runtime the watchdog
                //      is reached from — usually right after the sleep,
                //      which is uninteresting, but confirms the watchdog
                //      thread didn't itself deadlock).
                //   2. The PID + a hint to attach an external debugger
                //      (cdb / WinDbg / `rust-lldb -p <pid>`) for full
                //      thread coverage. We can't safely walk other
                //      threads' native stacks from a portable Rust
                //      thread without OS-specific facilities (Windows
                //      `MiniDumpWriteDump`, Linux `ptrace`, etc.).
                if total_acks == 0 {
                    let pid = std::process::id();
                    eprintln!(
                        "=== T19.H1 watchdog: no Java threads responded \
                         — main thread is in native (Rust) code. \
                         pid={pid}. Attach a native debugger before the \
                         3s post-dump grace ends to capture the hang \
                         site (Windows: `cdb -p {pid}` then `~* k`; \
                         Linux: `gdb -p {pid}` then `thread apply all bt`). \
                         ==="
                    );
                    let bt = std::backtrace::Backtrace::force_capture();
                    eprintln!(
                        "--- T19.H1 watchdog native backtrace (watchdog \
                         thread; FYI only) ---\n{bt}\n--- end native \
                         backtrace ---"
                    );
                    // PERF: ring recording is opt-in now (see
                    // `ring_recording_requested`). If it wasn't requested,
                    // the ring dumps below will say "recording disabled";
                    // tell the operator how to capture them next time so
                    // the diagnostic isn't a dead end.
                    if !ring_recording_requested {
                        eprintln!(
                            "=== T19.H1 watchdog: native-call/dispatch \
                             rings were not recording (opt-in). Re-run with \
                             `--stack-dump-on-timeout=N` or \
                             `CRATONVM_ENABLE_NATIVE_RING=1` to capture the \
                             last native methods leading up to the hang. ==="
                        );
                    }
                    // KC-watchdog-native: dump the native-call ring
                    // buffer. The last entry with `STILL-IN-NATIVE`
                    // marks the hang site.
                    cratonvm_native_api::native_ring::dump_to_stderr();
                    // T19.H1: dump the dispatch-trace ring too — it
                    // records *named* class.method.desc for the last
                    // 256 bytecode-method entries and native dispatches
                    // (across all threads), so the last few NAT/BC
                    // entries pinpoint the hung native and its caller.
                    cratonvm_vm::dispatch_trace::dump_to_stderr_unconditional(
                        "watchdog-native-hang",
                    );
                }

                // RKC16N.5 — flush the missing-natives audit BEFORE
                // `process::abort()` so a hung or watchdog-killed run
                // still produces the diagnostic JSON. Errors are
                // logged but never unwrap; abort still happens.
                shared_for_watchdog.dump_missing_natives();
                if let Some(path) = watchdog_dump_path.as_deref() {
                    match shared_for_watchdog.dump_missing_natives_json(path) {
                        Ok(()) => eprintln!(
                            "=== T19.H1 watchdog: missing-natives audit \
                             flushed (json={path}) ==="
                        ),
                        Err(e) => eprintln!(
                            "=== T19.H1 watchdog: failed to flush \
                             missing-natives JSON to {path}: {e} ==="
                        ),
                    }
                }
                if let Some(path) = watchdog_dump_grouped_path.as_deref() {
                    match shared_for_watchdog.dump_missing_natives_grouped_json(path) {
                        Ok(()) => eprintln!(
                            "=== T19.H1 watchdog: missing-natives audit \
                             flushed (grouped json={path}) ==="
                        ),
                        Err(e) => eprintln!(
                            "=== T19.H1 watchdog: failed to flush \
                             grouped missing-natives JSON to {path}: {e} ==="
                        ),
                    }
                }

                // Flush the stderr handle so our banners land before
                // the abort kills the process.
                use std::io::Write;
                let _ = std::io::stderr().flush();

                // Final cancellation check: `run()` may have completed during
                // the post-dump grace window. Don't abort a finished run.
                if watchdog_completed_for_thread.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                std::process::abort();
            })
            .context("failed to spawn stack-dump watchdog thread")?;
        // Arm the RAII guard so the completed flag is set on every exit path
        // of `run()` once the watchdog is live.
        _watchdog_completion_guard = Some(WatchdogCompletionGuard {
            flag: std::sync::Arc::clone(&watchdog_completed),
        });
        eprintln!("[cratonvm] stack-dump watchdog armed: will dump + abort after {secs}s");
    }

    // T14: Run System.initPhase1() when booting from real JDK classes
    // BEFORE loading the user main class.
    //
    // In HotSpot this is called from Threads::create_vm() after the
    // bootstrap classloader is initialized but before any user class is
    // resolved. It sets up system properties, encodings, and the standard
    // I/O streams (System.in/out/err).
    //
    // Previously the main class was loaded first, which forced eager
    // resolution of `java/lang/Object`, `String`, etc. under an
    // uninitialised system-properties / charset subsystem. A
    // NoClassDefFoundError originating in the half-bootstrapped JDK then
    // surfaced as the confusing "Could not find or load main class …"
    // instead of a clean bootstrap error.
    if vm.shared.config.java_home.is_some() {
        match vm.invoke("java/lang/System", "initPhase1", "()V", &[]) {
            Ok(_) => {
                tracing::info!("System.initPhase1() completed");
                // WP1.3: initPhase1 just finished — advance to level 2.
                vm.shared.set_init_level(2);
            }
            Err(e) => {
                // T14: initPhase1 may fail mid-bootstrap on real JDK 25 due to
                // subsystems we don't fully emulate (e.g. Unsafe accessors hitting
                // uninitialized reference slots).  The fallback path uses our
                // synthetic System.in/out/err so stdout/stderr still work.
                if let cratonvm_vm::error::MethodCallFailed::ExceptionThrown(exc_ref) = &e {
                    let exc_class_id = vm.shared.mem.heap.class_id_of(*exc_ref);
                    let exc_class_name = vm
                        .shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(exc_class_id)
                        .map(|c| c.name.to_string())
                        .unwrap_or_else(|| format!("unknown({})", exc_class_id));
                    tracing::info!(
                        "System.initPhase1() fell back to synthetic streams ({exc_class_name})"
                    );
                } else {
                    tracing::info!("System.initPhase1() fell back to synthetic streams");
                    tracing::debug!("initPhase1 details: {e:?}");
                }
                // initPhase1 may have partially initialized classes and populated stale
                // invoke/resolution cache entries (e.g. real JDK PrintStream bytecode
                // cached for a synthetic 1-slot object). Clear both caches so the next
                // invocation re-resolves cleanly via the native-override registry.
                vm.main_thread.invoke_cache.clear();
                vm.shared.classes.resolution_cache.write().clear();
                // WP1.3: even though initPhase1 threw mid-flight, the
                // early system-properties / stream installation ran
                // before the failure — enough for callers gated on
                // level 2 (e.g. `java.class.path` availability) to
                // proceed.  Bump anyway so downstream `initLevel()`
                // observers don't stall at 1.
                vm.shared.set_init_level(2);
            }
        }

        // WP1.3: initPhase2 / initPhase3 are pure-Java methods on
        // `java.lang.System` that finalise modules + classpath and
        // install `ClassLoader.scl`.  We don't run them end-to-end in
        // cratonvm (the real-JDK module graph resolution pulls in
        // subsystems we don't implement), but many callers key on
        // `initLevel() >= 3` to decide whether
        // `ClassLoader.getSystemClassLoader()` may read the `scl`
        // field directly.  We leave the level at 2 here — bumping
        // past it would send those callers down a null-deref path.
        // The CLI bumps to 4 below, just before `main()`, once the
        // initPhase2 gate no longer matters.
        //
        // INTENTIONAL (reviewed): skipping initPhase2/3 here is a deliberate
        // boot-sequencing choice, NOT a silent wrong-result stub. The
        // level-management contract is preserved end-to-end (level held at 2
        // until the gate is moot, then advanced to 3→4 below and the real
        // `jdk.internal.misc.VM.initLevel(4)` field is set), so observers see a
        // consistent boot state rather than a fabricated value. Running the
        // real initPhase2/3 is gated on module-system subsystems we do not yet
        // implement; if/when those land this skip should be revisited.
    }

    // WP1.3: right before `main()` starts, advance to level 4 —
    // HotSpot's "VM fully initialized" state.  This is the signal
    // that `ClassLoader.getSystemClassLoader()` may return `scl` if
    // it is populated (in cratonvm it usually isn't, so callers fall
    // through to `getBuiltinAppClassLoader()` without harm), that
    // `Thread.currentThread().getName()` is safe, and that every
    // subsystem keyed on `VM.awaitInitLevel(4)` can proceed.  We
    // also briefly pass through level 3 so any probe between 2 and
    // 4 observes a level 3 transition.
    vm.shared.set_init_level(3);
    vm.shared.set_init_level(4);

    // spring-bug-05: advance the REAL `jdk.internal.misc.VM.initLevel` static
    // field. `set_init_level` above only updates CratonVM's internal counter and
    // the `VM.initLevel()` *method* native — but `VM.isModuleSystemInited()`
    // (Proxy.java's gate at ProxyBuilder) reads the static *field* directly
    // (`initLevel >= MODULE_SYSTEM_INITED`). Since we never run the real
    // `System.initPhase2/3` (which would call `VM.initLevel(int)`), the field
    // stays 0 and every JDK dynamic Proxy throws `InternalError: Proxy is not
    // supported until module system is fully initialized`. The setter
    // `VM.initLevel(I)V` is real bytecode (not natively shadowed): invoking it
    // sets the field to SYSTEM_BOOTED (4) and wakes `awaitInitLevel` waiters,
    // exactly as a fully-booted HotSpot would. Real-JDK mode only; errors are
    // swallowed (synthetic mode has no such class/method).
    if vm.shared.config.java_home.is_some() {
        let _ = vm.invoke(
            "jdk/internal/misc/VM",
            "initLevel",
            "(I)V",
            &[Value::Int(4)],
        );
    }

    // Now (after `initPhase1` has set up system properties / encodings
    // / standard streams) it's safe to resolve and load the user main
    // class. See the comment on the `initPhase1` block above for why
    // this ordering matters in real-JDK mode.
    if let Err(e) = vm.load_class(&class_name) {
        // `class_name` is in internal slash form here; the inner error `e`
        // typically embeds that same slash-form name, so render both dotted so
        // the message doesn't show the class two ways (`com.example.Main` then
        // `class not found: com/example/Main`). HotSpot reports the binary
        // (dotted) name throughout.
        bail!(
            "Could not find or load main class {}: {}",
            class_name.replace('/', "."),
            e.to_string().replace('/', ".")
        );
    }

    // Build String[] args array for main(String[]).
    let java_args: Vec<Value> = args
        .args
        .iter()
        .map(|a| Value::Object(Some(create_java_string(&vm.shared, a))))
        .collect();

    // Resolve the `java/lang/String` class id for the args array's
    // element type. Previously this was hard-coded to `ClassId::new(0)`
    // (which is `java/lang/Object` — the base reference array element
    // type), but JLS / JVMS require `main(String[])` to receive a
    // `[Ljava/lang/String;` array, NOT `[Ljava/lang/Object;`. Code
    // that inspects `args.getClass().getComponentType()` (e.g. test
    // harnesses, generic helpers) observes the wrong component class
    // when ClassId(0) is used.
    //
    // `load_class_concurrent` is idempotent and the boot classloader
    // resolves `String` extremely early, so this is effectively a
    // hash-table lookup.
    let string_array_class_id = vm
        .shared
        .load_class_concurrent("java/lang/String")
        .unwrap_or_else(|_| cratonvm_vm::ClassId::new(0));
    let args_array = vm.shared.mem.heap.alloc_array(
        string_array_class_id,
        cratonvm_vm::memory::heap::ArrayElementType::Reference,
        java_args.len(),
    );
    for (i, val) in java_args.into_iter().enumerate() {
        vm.shared
            .mem
            .heap
            .set_array_element(args_array, i, val)
            .map_err(|idx| {
                anyhow::anyhow!("Failed to set args array element {i} (index {idx} out of bounds)")
            })?;
    }

    // Pre-allocate the singleton java.lang.OutOfMemoryError while the heap is
    // still fresh, so a later 100%-full-heap OOM (in either user code or a
    // premain) can be thrown without allocating the throwable — which would
    // otherwise hard-abort in the non-fallible String allocator. Idempotent and
    // best-effort: if the class isn't loadable yet it leaves the slot empty and
    // the OOM paths keep their prior behaviour.
    cratonvm_vm::runtime::exceptions::ensure_singleton_oom(&vm.shared, &mut vm.main_thread);

    // WP2.4-C — run every `-javaagent:` agent's `premain(String,
    // Instrumentation)` hook BEFORE the application's `main`. Per the
    // `java.lang.instrument` package spec, agent failures are warnings
    // (logged inside the dispatcher) unless the agent throws a fatal
    // `Error`, in which case we abort here.
    if !java_agents.is_empty() {
        let res = cratonvm_vm::runtime::agent_loader::invoke_premains(
            &vm.shared,
            &mut vm.main_thread,
            &java_agents,
        );
        if let Err(e) = res {
            bail!("javaagent premain aborted VM: {e}");
        }
    }

    // Invoke main(String[])
    let main_start = std::time::Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        vm.invoke(
            &class_name,
            "main",
            "([Ljava/lang/String;)V",
            &[Value::Object(Some(args_array))],
        )
    }));
    let main_elapsed = main_start.elapsed();
    tracing::info!("main() completed in {:.2}s", main_elapsed.as_secs_f64());
    let result = match result {
        Ok(r) => r,
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            bail!("main() panicked: {msg}");
        }
    };

    // NEW-10: before reporting the invocation result, write the
    // missing-natives audit log to the user-specified JSON path. We
    // do this unconditionally (regardless of Ok/Err) so a crashing
    // program still produces a census file. Any I/O error surfaces
    // as a warning — the primary invocation result takes precedence.
    if let Some(path) = &args.dump_missing_natives {
        match vm.shared.dump_missing_natives_json(path) {
            Ok(()) => {
                let count = vm.shared.get_missing_natives().len();
                eprintln!("[cratonvm] wrote {count} missing-native entries to {path}");
            }
            Err(e) => {
                eprintln!(
                    "[cratonvm] warning: could not write missing-natives JSON to {path}: {e}"
                );
            }
        }
    }

    // Synthetic-stub census: full native registry with kind tags.
    if let Some(path) = &args.dump_native_registry {
        match vm.shared.dump_native_registry_json(path) {
            Ok((n_intrinsic, n_bridge, n_stub)) => {
                eprintln!(
                    "[cratonvm] wrote native registry census to {path} \
                     (intrinsic={n_intrinsic}, bridge={n_bridge}, \
                     synthetic-stub={n_stub})"
                );
            }
            Err(e) => {
                eprintln!(
                    "[cratonvm] warning: could not write native registry JSON to {path}: {e}"
                );
            }
        }
    }

    // T2.1.3: grouped-by-module census.
    if let Some(path) = &args.dump_missing_natives_grouped {
        match vm.shared.dump_missing_natives_grouped_json(path) {
            Ok(()) => {
                let grouped = vm.shared.classify_missing_natives_by_module();
                let total: usize = grouped.values().map(|v| v.len()).sum();
                eprintln!(
                    "[cratonvm] wrote {total} missing-native entries across \
                     {} modules to {path}",
                    grouped.len()
                );
            }
            Err(e) => {
                eprintln!(
                    "[cratonvm] warning: could not write grouped missing-natives \
                     JSON to {path}: {e}"
                );
            }
        }
    }

    // Interpreter intrinsic-table stats. `CRATONVM_INTRINSIC_STATS=1` prints
    // the steady-state intrinsic-dispatch hit count on shutdown — the
    // counter that verifies acceptance criterion §9 of
    // docs/feature_roadmap_interpreter_intrinsic_table.md.
    if matches!(
        std::env::var("CRATONVM_INTRINSIC_STATS").as_deref(),
        Ok("1")
    ) {
        eprintln!(
            "[cratonvm] interpreter intrinsic dispatches: {}",
            cratonvm_vm::runtime::interpreter::intrinsic_hit_count()
        );
    }

    // WS1 diagnostic: final JIT-dispatch-helper profile dump on shutdown
    // (env-gated inside `dump_now` callers; `enabled()` re-checked here).
    if cratonvm_vm::jit::helpers::mic_prof::enabled() {
        cratonvm_vm::jit::helpers::mic_prof::dump_now();
        eprintln!(
            "[MIC_PROF] gc_collections={} total_dispatches={}",
            vm.shared.mem.heap.collection_count(),
            cratonvm_vm::dispatch_trace::total_dispatches()
        );
    }

    // B6: Silent-exit guard. If main() returned Ok but the VM has recorded
    // one or more swallowed errors during class init / invokedynamic / native
    // calls, surface a WARN to stderr so users (and CI) don't mistake a
    // silent exit for a successful run. Exit code stays 0 for compatibility
    // with programs that legitimately produce no stdout.
    if matches!(result, Ok(_)) {
        let swallowed = vm
            .shared
            .debug
            .swallow_counter
            .load(std::sync::atomic::Ordering::Relaxed);
        if swallowed > 0 {
            eprintln!(
                "WARN: main() completed with {swallowed} swallowed VM error(s) \
                 (class-init / invokedynamic / native). Re-run with \
                 RUST_LOG=warn (already default) to see each site, or \
                 CRATONVM_STRICT_SWALLOWS=1 to escalate the first swallow to a \
                 panic for diagnosis."
            );
        }
    }

    // §5 acceptance metric — aggregate G1 pause summary (p50/p99/max young +
    // mixed) to stderr at shutdown when GC stats are requested. Driven by
    // `--verbose:gc` or the `CRATONVM_GC_STATS` env knob so a gauntlet runner
    // can collect the table without `RUST_LOG`. No-op for the generational
    // collector and when no G1 collection ran.
    if args.verbose_gc || std::env::var_os("CRATONVM_GC_STATS").is_some() {
        vm.shared.mem.heap.print_gc_summary();
    }

    // T19.K1 — wait for non-daemon threads before exiting.
    //
    // Per the JVM specification, the VM keeps running until every
    // non-daemon thread has terminated. Daemon threads (GC workers,
    // event loops, finalisers) are best-effort: when the last
    // non-daemon thread finishes, the VM exits, abandoning any
    // remaining daemons.
    //
    // For HelloWorld and any program that doesn't `Thread.start()`
    // a user thread, `wait_for_non_daemon_threads` returns
    // immediately (the snapshot is empty). For Quarkus / Keycloak,
    // the embedded HTTP listener and Vert.x worker pool are
    // non-daemon; this loop blocks until they exit.
    //
    // We only wait when `main()` returned cleanly (`Ok`). On
    // exception we propagate to the existing error-printing path
    // which calls `bail!()` and lets the process exit with non-zero
    // status — same as HotSpot's "Exception in thread \"main\"".
    // Waiting for daemons or worker threads after a fatal error
    // would just delay the stack trace.
    //
    // The deadline parameter is `None` (wait indefinitely): a
    // legitimate non-daemon thread could be a long-running service
    // and bounding the wait would surprise users. CI runs that need
    // to bound execution can use the existing
    // `--stack-dump-on-timeout` watchdog which `abort()`s the
    // process from a separate thread — this loop will be
    // interrupted by the watchdog's `process::abort()` call.
    if matches!(result, Ok(_)) {
        // T19.K1 — diagnostic only when the wait is actually
        // observable (i.e. there ARE non-daemon threads). HelloWorld
        // and any program that doesn't `Thread.start()` a user
        // thread skips this message and exits silently. Long-running
        // apps (Quarkus, Keycloak, embedded Jetty) print one line so
        // the user can tell the wait is what's holding the process
        // alive — useful when a CI run mysteriously sits at "main
        // returned" forever.
        let pending = vm
            .shared
            .threads
            .thread_registry
            .alive_non_daemon_thread_ids()
            .len();
        if pending > 0 {
            eprintln!(
                "[cratonvm] main() returned; VM held alive by {pending} \
                 non-daemon thread(s) (JVM-spec behaviour). Send SIGINT/\
                 SIGTERM, use System.exit(), or pass --stack-dump-on-timeout \
                 to bound execution."
            );
        }
        // GC-barrier fix: this thread never runs Java bytecode again once it
        // reaches this wait (it is parked in a raw `pthread_join` loop, not
        // Java-level parking), so it can never cooperatively reach an
        // interpreter safepoint and call `arrive_and_wait`. Publish the main
        // thread's roots and enter the full per-thread blocked-region protocol
        // for the whole wait, mirroring native socket/pipe waits.
        vm.begin_main_thread_blocking_region("vm-main:wait-non-daemon");
        let joined = vm
            .shared
            .threads
            .thread_registry
            .wait_for_non_daemon_threads(None);
        vm.end_main_thread_blocking_region();
        if joined > 0 {
            tracing::info!("cratonvm: joined {joined} non-daemon thread(s) after main() returned");
        }
    }

    match result {
        Ok(_) => Ok(()),
        Err(MethodCallFailed::InternalError(e)) => {
            bail!("Error in thread \"main\" {e}");
        }
        Err(MethodCallFailed::ExceptionThrown(exc_ref)) => {
            // Try to read exception class name and message, plus the cause chain
            // so users can see the underlying reason for wrapper exceptions like
            // ExceptionInInitializerError or InvocationTargetException.
            //
            // Also render the captured Java-side stack trace from the
            // `stackTrace` field on each Throwable when present. NOTE: in the
            // current cratonvm, `Throwable.fillInStackTrace` (see
            // `native-builtins/src/lang_misc.rs`) only stashes frames into the
            // VM-wide Throwable trace registry keyed by identity
            // hash — it does NOT populate the heap-side `stackTrace` /
            // `backtrace` field. The Java code only writes that field lazily
            // when something calls `Throwable.getStackTrace()`. For unhandled
            // exceptions that escape `main()`, that has typically never
            // happened, so the renderer below will usually find a null array
            // and emit no `\tat ...` lines. Promoting the synthetic capture
            // to populate the heap field (or wiring this CLI to read from
            // `throwable_stacks` directly) is roadmap item T2.2.18 — see
            // `docs/roadmap-100.md` line 471.
            //
            // INTENTIONAL (reviewed): omitting the `\tat ...` frames here is an
            // acceptable, honest degradation — NOT a wrong-result stub. The
            // renderer prints the real exception class, message, and the full
            // `Caused by:` cause chain (all read from live heap fields); only
            // the per-frame stack-trace lines are absent when the heap-side
            // `stackTrace` array was never materialised. We never fabricate
            // synthetic frames, so what is printed is always faithful; the
            // missing frames are a known limitation tracked by T2.2.18, not a
            // silent incorrect value.
            let mut cur = exc_ref;
            let mut lines: Vec<String> = Vec::new();
            let mut prefix = "Exception in thread \"main\"";
            // Cap the cause-chain walk so a self-referential or pathologically
            // deep chain can't loop forever. When the cap is hit with an
            // unrendered cause still pending, a marker line is emitted (see the
            // `next_cause` handling at the end of the loop) so deeply-wrapped
            // exceptions are not silently truncated.
            const MAX_CAUSE_DEPTH: usize = 8;
            for depth in 0..MAX_CAUSE_DEPTH {
                let cid = vm.shared.mem.heap.class_id_of(cur);
                // PERF: resolve the class name AND the Throwable field indices
                // under a single read guard. These were two back-to-back
                // `class_manager.read()` calls; both are pure reads with no
                // intervening work, so one guard is behavior-identical and
                // avoids a redundant lock/unlock per cause-chain iteration.
                //
                // Find fields by name so we work regardless of layout.
                // Also probe `target` (used by InvocationTargetException
                // in lieu of Throwable.cause — see its `getCause()` override)
                // so that `Caused by:` chains still walk through the wrapper.
                let (cname, msg_idx, cause_idx, stack_idx, target_idx) = {
                    let cm = vm.shared.classes.class_manager.read();
                    let cname = cm
                        .get_class(cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    let mut msg_i: Option<usize> = None;
                    let mut cause_i: Option<usize> = None;
                    let mut stack_i: Option<usize> = None;
                    let mut target_i: Option<usize> = None;
                    // Walk from Throwable down
                    let mut walk = Some(cid);
                    while let Some(k) = walk {
                        if let Some(cls) = cm.get_class(k) {
                            let mut inst = 0usize;
                            for f in &cls.fields {
                                if !f.is_static() {
                                    let abs = cls.first_field_index + inst;
                                    if &*f.name == "detailMessage" && msg_i.is_none() {
                                        msg_i = Some(abs);
                                    }
                                    if &*f.name == "cause" && cause_i.is_none() {
                                        cause_i = Some(abs);
                                    }
                                    if &*f.name == "stackTrace" && stack_i.is_none() {
                                        stack_i = Some(abs);
                                    }
                                    if &*f.name == "target" && target_i.is_none() {
                                        target_i = Some(abs);
                                    }
                                    inst += 1;
                                }
                            }
                            walk = cls.superclass;
                        } else {
                            break;
                        }
                    }
                    (cname, msg_i, cause_i, stack_i, target_i)
                };
                let message = if let Some(i) = msg_idx {
                    let v = vm.shared.mem.heap.get_field(cur, i);
                    if let Value::Object(Some(s)) = v {
                        cratonvm_vm::vm::read_java_string(&vm.shared.mem.heap, s)
                            .unwrap_or_default()
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                let line = if message.is_empty() {
                    format!("{prefix} {cname}")
                } else {
                    format!("{prefix} {cname}: {message}")
                };
                lines.push(line);

                // Render `\tat ...` frames from `stackTrace[]` if populated.
                // Each element is a `StackTraceElement` with fields, in
                // declaration order: declaringClass (String), methodName
                // (String), fileName (String, nullable), lineNumber (int).
                // We resolve those four field indices the same way as the
                // Throwable fields above so we work regardless of layout.
                let mut emitted_frames = false;
                if let Some(si) = stack_idx {
                    let stack_val = vm.shared.mem.heap.get_field(cur, si);
                    if let Value::Object(Some(arr)) = stack_val {
                        let len = vm.shared.mem.heap.array_length(arr);
                        if len > 0 {
                            emitted_frames = true;
                            // Resolve StackTraceElement field indices once
                            // from the first non-null element's class.
                            let mut ste_idx: Option<(usize, usize, usize, usize)> = None;
                            for i in 0..len {
                                let elem =
                                    vm.shared.mem.heap.get_array_element(arr, i).ok().and_then(
                                        |v| {
                                            if let Value::Object(Some(o)) = v {
                                                Some(o)
                                            } else {
                                                None
                                            }
                                        },
                                    );
                                let Some(elem_ref) = elem else { continue };
                                if ste_idx.is_none() {
                                    let ecid = vm.shared.mem.heap.class_id_of(elem_ref);
                                    let cm = vm.shared.classes.class_manager.read();
                                    let mut dc: Option<usize> = None;
                                    let mut mn: Option<usize> = None;
                                    let mut fn_: Option<usize> = None;
                                    let mut ln: Option<usize> = None;
                                    let mut walk = Some(ecid);
                                    while let Some(k) = walk {
                                        if let Some(cls) = cm.get_class(k) {
                                            let mut inst = 0usize;
                                            for f in &cls.fields {
                                                if !f.is_static() {
                                                    let abs = cls.first_field_index + inst;
                                                    match &*f.name {
                                                        "declaringClass" if dc.is_none() => {
                                                            dc = Some(abs)
                                                        }
                                                        "methodName" if mn.is_none() => {
                                                            mn = Some(abs)
                                                        }
                                                        "fileName" if fn_.is_none() => {
                                                            fn_ = Some(abs)
                                                        }
                                                        "lineNumber" if ln.is_none() => {
                                                            ln = Some(abs)
                                                        }
                                                        _ => {}
                                                    }
                                                    inst += 1;
                                                }
                                            }
                                            walk = cls.superclass;
                                        } else {
                                            break;
                                        }
                                    }
                                    if let (Some(a), Some(b), Some(c), Some(d)) = (dc, mn, fn_, ln)
                                    {
                                        ste_idx = Some((a, b, c, d));
                                    }
                                }
                                let Some((dc, mn, fn_, ln)) = ste_idx else {
                                    continue;
                                };
                                let read_str = |idx: usize| -> Option<String> {
                                    match vm.shared.mem.heap.get_field(elem_ref, idx) {
                                        Value::Object(Some(s)) => {
                                            cratonvm_vm::vm::read_java_string(
                                                &vm.shared.mem.heap,
                                                s,
                                            )
                                        }
                                        _ => None,
                                    }
                                };
                                let class_name =
                                    read_str(dc).unwrap_or_else(|| "<unknown>".to_string());
                                let method_name =
                                    read_str(mn).unwrap_or_else(|| "<unknown>".to_string());
                                let file_name = read_str(fn_);
                                let line_no = match vm.shared.mem.heap.get_field(elem_ref, ln) {
                                    Value::Int(i) => i,
                                    _ => -1,
                                };
                                // HotSpot format:
                                //   \tat <class>.<method>(<file>:<line>)
                                // If fileName is null/empty, use "Unknown Source".
                                // If lineNumber < 0, omit ":<line>".
                                let location = match (file_name.as_deref(), line_no) {
                                    (Some(f), n) if !f.is_empty() && n >= 0 => format!("{f}:{n}"),
                                    (Some(f), _) if !f.is_empty() => f.to_string(),
                                    _ => "Unknown Source".to_string(),
                                };
                                lines.push(format!("\tat {class_name}.{method_name}({location})"));
                            }
                        }
                    }
                }

                // Fallback: when `Throwable.stackTrace[]` was never populated
                // (the array is null or empty — the typical case for an
                // exception that escapes `main()` without anyone calling
                // `getStackTrace()`), pull frames from the VM-wide registry
                // keyed by identity hash
                // — that's where `Throwable.fillInStackTrace` actually
                // stashes the captured frames in this VM. See
                // `vm/src/vm/vm_init.rs::Vm::throwable_stack_for`.
                if !emitted_frames {
                    if let Some(frames) = vm.throwable_stack_for(cur) {
                        for frame in frames {
                            let location = match (frame.file.as_deref(), frame.line) {
                                (Some(f), n) if !f.is_empty() && n >= 0 => format!("{f}:{n}"),
                                (Some(f), _) if !f.is_empty() => f.to_string(),
                                _ => "Unknown Source".to_string(),
                            };
                            lines.push(format!(
                                "\tat {}.{}({})",
                                frame.class, frame.method, location
                            ));
                        }
                    }
                }

                // Follow cause. Throwable.cause is the canonical chain link,
                // but InvocationTargetException stores the wrapped exception
                // in its own `target` field and its `getCause()` override
                // returns that — so the heap-level `cause` is null/self while
                // the real cause lives in `target`. Probe both.
                let mut next_cause = {
                    let mut next = None;
                    if let Some(i) = cause_idx {
                        if let Value::Object(Some(c)) = vm.shared.mem.heap.get_field(cur, i) {
                            if c != cur {
                                next = Some(c);
                            }
                        }
                    }
                    if next.is_none() {
                        if let Some(i) = target_idx {
                            if let Value::Object(Some(t)) = vm.shared.mem.heap.get_field(cur, i) {
                                if t != cur {
                                    next = Some(t);
                                }
                            }
                        }
                    }
                    next
                };
                if next_cause.is_none() && cname == "java/lang/reflect/InvocationTargetException" {
                    let ite_decl = vm
                        .shared
                        .classes
                        .class_manager
                        .read()
                        .get_loaded_class_id("java/lang/reflect/InvocationTargetException");
                    for (meth, desc) in [
                        ("getTargetException", "()Ljava/lang/Throwable;"),
                        ("getCause", "()Ljava/lang/Throwable;"),
                    ] {
                        if next_cause.is_some() {
                            break;
                        }
                        let invoke_res = if let Some(ite_cid) = ite_decl {
                            invoke_on_class_shared_no_retarget(
                                &vm.shared,
                                &mut vm.main_thread,
                                ite_cid,
                                meth,
                                desc,
                                &[Value::Object(Some(cur))],
                            )
                        } else {
                            invoke_on_class_shared(
                                &vm.shared,
                                &mut vm.main_thread,
                                cid,
                                meth,
                                desc,
                                &[Value::Object(Some(cur))],
                            )
                        };
                        match invoke_res {
                            Ok(Some(Value::Object(Some(t)))) if t != cur => {
                                next_cause = Some(t);
                            }
                            Ok(_) => {}
                            Err(e) => {
                                lines.push(format!(
                                    "[cratonvm-cli] InvocationTargetException.{meth}() failed: {e:?}"
                                ));
                            }
                        }
                    }
                }
                // When walking through PropertyBatchUpdateException, also
                // surface the nested PropertyAccessExceptions array contents.
                // The standard `getMessage()` joins their messages with ";",
                // but Spring's BeanCreationException wrapper inlines that
                // joined string before any per-sub-cause framing is rendered,
                // so the actual offending property (and the IAE/NPE thrown by
                // the setter) is lost in plain `Caused by:` chains.  Drill one
                // level so each sub-cause's class + message + cause chain is
                // visible — this is the only stable surface for diagnosing
                // setter-injection failures because PBUE itself doesn't
                // `initCause` the first sub-exception.
                if cname == "org/springframework/beans/PropertyBatchUpdateException" {
                    // Find the propertyAccessExceptions field by name.
                    let arr_idx = {
                        let cm = vm.shared.classes.class_manager.read();
                        let mut found: Option<usize> = None;
                        let mut walk = Some(cid);
                        while let Some(k) = walk {
                            if let Some(cls) = cm.get_class(k) {
                                let mut inst = 0usize;
                                for f in &cls.fields {
                                    if !f.is_static() {
                                        let abs = cls.first_field_index + inst;
                                        if &*f.name == "propertyAccessExceptions" && found.is_none()
                                        {
                                            found = Some(abs);
                                        }
                                        inst += 1;
                                    }
                                }
                                walk = cls.superclass;
                            } else {
                                break;
                            }
                        }
                        found
                    };
                    if let Some(ai) = arr_idx {
                        match vm.shared.mem.heap.get_field(cur, ai) {
                            Value::Object(Some(arr)) => {
                                let n = vm.shared.mem.heap.array_length(arr);
                                lines.push(format!(
                                    "[cratonvm-cli] PropertyBatchUpdateException.propertyAccessExceptions length={n}"
                                ));
                                for i in 0..n {
                                    let elem = vm.shared.mem.heap.get_array_element(arr, i).ok();
                                    if let Some(Value::Object(Some(eref))) = elem {
                                        let ecid = vm.shared.mem.heap.class_id_of(eref);
                                        // PERF: one read guard for the sub-exception class
                                        // name and its field indices (back-to-back reads).
                                        // Read detailMessage and cause from this sub-exception
                                        let (ename, smsg, scause, spname) = {
                                            let cm = vm.shared.classes.class_manager.read();
                                            let ename = cm
                                                .get_class(ecid)
                                                .map(|c| c.name.to_string())
                                                .unwrap_or_else(|| "?".to_string());
                                            let mut mi: Option<usize> = None;
                                            let mut ci: Option<usize> = None;
                                            let mut pn: Option<usize> = None;
                                            let mut walk = Some(ecid);
                                            while let Some(k) = walk {
                                                if let Some(cls) = cm.get_class(k) {
                                                    let mut inst = 0usize;
                                                    for f in &cls.fields {
                                                        if !f.is_static() {
                                                            let abs = cls.first_field_index + inst;
                                                            match &*f.name {
                                                                "detailMessage" if mi.is_none() => {
                                                                    mi = Some(abs)
                                                                }
                                                                "cause" if ci.is_none() => {
                                                                    ci = Some(abs)
                                                                }
                                                                "propertyName" if pn.is_none() => {
                                                                    pn = Some(abs)
                                                                }
                                                                _ => {}
                                                            }
                                                            inst += 1;
                                                        }
                                                    }
                                                    walk = cls.superclass;
                                                } else {
                                                    break;
                                                }
                                            }
                                            let read_s = |idx: Option<usize>| -> String {
                                                idx.and_then(|i| {
                                                    match vm.shared.mem.heap.get_field(eref, i) {
                                                        Value::Object(Some(s)) => {
                                                            cratonvm_vm::vm::read_java_string(
                                                                &vm.shared.mem.heap,
                                                                s,
                                                            )
                                                        }
                                                        _ => None,
                                                    }
                                                })
                                                .unwrap_or_default()
                                            };
                                            (ename, read_s(mi), ci, read_s(pn))
                                        };
                                        lines.push(format!(
                                            "[cratonvm-cli]   [{i}] {ename} property='{spname}' message={smsg:?}"
                                        ));
                                        if let Some(frames) = vm.throwable_stack_for(eref) {
                                            if !frames.is_empty() {
                                                lines.push(format!(
                                                    "[cratonvm-cli]       ({} captured frames)",
                                                    frames.len()
                                                ));
                                                for frame in frames.iter().take(12) {
                                                    let loc =
                                                        match (frame.file.as_deref(), frame.line) {
                                                            (Some(f), n)
                                                                if !f.is_empty() && n >= 0 =>
                                                            {
                                                                format!("{f}:{n}")
                                                            }
                                                            (Some(f), _) if !f.is_empty() => {
                                                                f.to_string()
                                                            }
                                                            _ => "Unknown Source".to_string(),
                                                        };
                                                    lines.push(format!(
                                                        "\t\tat {}.{}({})",
                                                        frame.class, frame.method, loc
                                                    ));
                                                }
                                            }
                                        }
                                        // Follow cause(s) for this sub-exception (one level deep,
                                        // up to 6 deep just in case).
                                        let mut sub_cur = scause.and_then(|ci| {
                                            if let Value::Object(Some(c)) =
                                                vm.shared.mem.heap.get_field(eref, ci)
                                            {
                                                if c != eref {
                                                    Some(c)
                                                } else {
                                                    None
                                                }
                                            } else {
                                                None
                                            }
                                        });
                                        for _d in 0..6 {
                                            let Some(sc) = sub_cur else { break };
                                            let sc_cid = vm.shared.mem.heap.class_id_of(sc);
                                            // PERF: one read guard for the sub-cause class
                                            // name and its field indices (back-to-back reads).
                                            let (sc_name, sc_msg, sc_cause_idx) = {
                                                let cm = vm.shared.classes.class_manager.read();
                                                let sc_name = cm
                                                    .get_class(sc_cid)
                                                    .map(|c| c.name.to_string())
                                                    .unwrap_or_else(|| "?".to_string());
                                                let mut mi: Option<usize> = None;
                                                let mut ci: Option<usize> = None;
                                                let mut walk = Some(sc_cid);
                                                while let Some(k) = walk {
                                                    if let Some(cls) = cm.get_class(k) {
                                                        let mut inst = 0usize;
                                                        for f in &cls.fields {
                                                            if !f.is_static() {
                                                                let abs =
                                                                    cls.first_field_index + inst;
                                                                match &*f.name {
                                                                    "detailMessage"
                                                                        if mi.is_none() =>
                                                                    {
                                                                        mi = Some(abs)
                                                                    }
                                                                    "cause" if ci.is_none() => {
                                                                        ci = Some(abs)
                                                                    }
                                                                    _ => {}
                                                                }
                                                                inst += 1;
                                                            }
                                                        }
                                                        walk = cls.superclass;
                                                    } else {
                                                        break;
                                                    }
                                                }
                                                let m = mi
                                                    .and_then(|i| {
                                                        match vm.shared.mem.heap.get_field(sc, i) {
                                                            Value::Object(Some(s)) => {
                                                                cratonvm_vm::vm::read_java_string(
                                                                    &vm.shared.mem.heap,
                                                                    s,
                                                                )
                                                            }
                                                            _ => None,
                                                        }
                                                    })
                                                    .unwrap_or_default();
                                                (sc_name, m, ci)
                                            };
                                            lines.push(format!("[cratonvm-cli]       Caused by: {sc_name}: {sc_msg}"));
                                            if let Some(frames) = vm.throwable_stack_for(sc) {
                                                if !frames.is_empty() {
                                                    lines.push(format!("[cratonvm-cli]         ({} captured frames)", frames.len()));
                                                    for frame in frames.iter().take(16) {
                                                        let loc = match (
                                                            frame.file.as_deref(),
                                                            frame.line,
                                                        ) {
                                                            (Some(f), n)
                                                                if !f.is_empty() && n >= 0 =>
                                                            {
                                                                format!("{f}:{n}")
                                                            }
                                                            (Some(f), _) if !f.is_empty() => {
                                                                f.to_string()
                                                            }
                                                            _ => "Unknown Source".to_string(),
                                                        };
                                                        lines.push(format!(
                                                            "\t\t\tat {}.{}({})",
                                                            frame.class, frame.method, loc
                                                        ));
                                                    }
                                                }
                                            }
                                            sub_cur = sc_cause_idx.and_then(|ci| {
                                                if let Value::Object(Some(c)) =
                                                    vm.shared.mem.heap.get_field(sc, ci)
                                                {
                                                    if c != sc {
                                                        Some(c)
                                                    } else {
                                                        None
                                                    }
                                                } else {
                                                    None
                                                }
                                            });
                                        }
                                    } else {
                                        lines.push(format!("[cratonvm-cli]   [{i}] <null>"));
                                    }
                                }
                            }
                            Value::Object(None) => {
                                lines.push("[cratonvm-cli] PropertyBatchUpdateException.propertyAccessExceptions = null".into());
                            }
                            _ => {}
                        }
                    } else {
                        lines.push("[cratonvm-cli] PropertyBatchUpdateException.propertyAccessExceptions field NOT FOUND on class".into());
                    }
                }
                if let Some(c) = next_cause {
                    // If this is the last iteration the cap allows, the cause
                    // `c` would never be rendered — emit a marker so deeply
                    // wrapped exceptions are not silently cut off.
                    if depth + 1 >= MAX_CAUSE_DEPTH {
                        lines.push("\t... (deeper causes truncated)".to_string());
                        break;
                    }
                    cur = c;
                    prefix = "Caused by:";
                    continue;
                }
                break;
            }
            let had_caused_by = lines.iter().any(|l| l.starts_with("Caused by:"));
            if !had_caused_by
                && lines
                    .iter()
                    .any(|l| l.contains("java/lang/reflect/InvocationTargetException"))
            {
                if let Some(frames) = vm.throwable_stack_for(exc_ref) {
                    if !frames.is_empty() {
                        lines.push(
                            "[cratonvm-cli] Throwable stack (fillInStackTrace) for InvocationTargetException:"
                                .to_string(),
                        );
                        for frame in frames.iter().take(24) {
                            let location = match (frame.file.as_deref(), frame.line) {
                                (Some(f), n) if !f.is_empty() && n >= 0 => format!("{f}:{n}"),
                                (Some(f), _) if !f.is_empty() => f.to_string(),
                                _ => "Unknown Source".to_string(),
                            };
                            lines.push(format!(
                                "\tat {}.{}({})",
                                frame.class, frame.method, location
                            ));
                        }
                    }
                }
            }
            // Missing-stack-trace diagnostic (2026-05-21).
            //
            // When an exception escapes `main()` and NEITHER the heap-side
            // `Throwable.stackTrace[]` NOR the VM-wide retained trace
            // capture produced a single `\tat ...` frame, the bare
            // `Exception in thread "main" <class>: <msg>` line is useless
            // for diagnosis — exactly the keycloak26 `NullPointerException:
            // charset` symptom. Rather than exit silently, emit an explicit
            // marker so the failure mode is unambiguous, retry the
            // identity-hash trace lookup for the head exception, and point
            // the operator at the live-stack diagnostic env var.
            let emitted_any_frame = lines.iter().any(|l| l.starts_with("\tat "));
            if !emitted_any_frame {
                lines.push(
                    "[cratonvm-cli] (no Java stack frames were captured for this exception)"
                        .to_string(),
                );
                // Last-ditch: dump whatever the head exception's
                // retained trace entry holds, even if the cause-chain
                // walk above skipped it.
                match vm.throwable_stack_for(exc_ref) {
                    Some(frames) if !frames.is_empty() => {
                        lines.push(format!(
                            "[cratonvm-cli] recovered {} captured frame(s) from the trace store:",
                            frames.len()
                        ));
                        for frame in frames.iter().take(40) {
                            let location = match (frame.file.as_deref(), frame.line) {
                                (Some(f), n) if !f.is_empty() && n >= 0 => format!("{f}:{n}"),
                                (Some(f), _) if !f.is_empty() => f.to_string(),
                                _ => "Unknown Source".to_string(),
                            };
                            lines.push(format!(
                                "\tat {}.{}({})",
                                frame.class, frame.method, location
                            ));
                        }
                    }
                    _ => {
                        lines.push(
                            "[cratonvm-cli] the retained trace store has no entry for this \
                             throwable either — the exception was likely thrown on a \
                             non-main thread, or its constructor was shadowed by a native \
                             that skipped fillInStackTrace."
                                .to_string(),
                        );
                        lines.push(
                            "[cratonvm-cli] re-run with CRATONVM_DBG_CHARSET=1 to dump the \
                             full live Java thread stack at throw time (NullPointerException: \
                             charset), or CRATONVM_DBG_ATHROW=1 for every exception throw."
                                .to_string(),
                        );
                    }
                }
            }
            bail!("{}", lines.join("\n"));
        }
    }
}

fn main() {
    // Hardware-fault diagnostics. On Windows a SEGV/access violation is a
    // structured exception that bypasses the Rust panic hook below entirely;
    // without this, a native fault (e.g. the JIT-dispatch SEGV) kills the
    // process with empty stderr and a bare STATUS_ACCESS_VIOLATION exit code.
    // This registers a vectored exception handler that prints the faulting PC
    // + a symbolized backtrace and then lets the process die as before. It
    // does NOT install a panic hook, so the visibility-first hook set up just
    // below is preserved. No-op on non-Windows targets.
    cratonvm_vm::runtime::crash_handler::install_hardware_fault_handler();

    // Diagnostic: CRATONVM_SYMBOLIZE=0x11BF183,0xAB10E resolves exe-relative
    // RVAs (as printed by the VEH crash report) against THIS binary's symbols
    // in a clean context, then exits. Used to symbolize a multi-threaded crash
    // whose racy teardown truncated the in-handler symbolization.
    if let Ok(spec) = std::env::var("CRATONVM_SYMBOLIZE") {
        let rvas: Vec<usize> = spec
            .split(',')
            .filter_map(|s| {
                let s = s.trim().trim_start_matches("0x").trim_start_matches("0X");
                usize::from_str_radix(s, 16).ok()
            })
            .collect();
        for (rva, name) in cratonvm_vm::runtime::crash_handler::symbolize_rvas(&rvas) {
            match name {
                Some(n) => println!("0x{:X}\t{}", rva, n),
                None => println!("0x{:X}\t<unresolved>", rva),
            }
        }
        std::process::exit(0);
    }

    // Self-test hook for the hardware-fault handler: when CRATONVM_TEST_SEGV=1,
    // deliberately trigger an access violation right after installing the
    // handler so the VEH path (faulting PC + symbolized backtrace) can be
    // validated without needing to reproduce a real crash. Gated behind an env
    // var so it never affects normal runs.
    if std::env::var("CRATONVM_TEST_SEGV").as_deref() == Ok("1") {
        eprintln!("[cratonvm] CRATONVM_TEST_SEGV=1: forcing an access violation");
        // SAFETY: intentional null/wild dereference to exercise the fault
        // handler. This is dead code on every normal run.
        unsafe {
            let p = 0xdead_beef_usize as *mut u8;
            std::ptr::write_volatile(p, 0);
        }
    }

    // I1 — Visibility-first panic hook.
    //
    // The previous T14 hook silenced **every** Rust panic by routing it to
    // `tracing::debug!`. That worked for the well-known initPhase1
    // bootstrap-path panics (unaligned-pointer reads, transient null
    // dereferences) which the outer `safe_native_call` already logs once
    // via a user-facing warning — but it also silenced *real* panics that
    // escaped a `catch_unwind`. Because the tracing subscriber installed
    // by `run()` filters at WARN+, debug-level panic notices were never
    // written to stderr, and any genuine VM crash showed up in the logs
    // with an empty stderr and an exit code derived from the OS abort
    // (rc=-1 on Windows when the runner kills the process; STATUS_ACCESS_
    // VIOLATION when the abort fires from native code). The CGLIB probe
    // hit exactly this surface: an interpreter loop hung inside
    // `String.indexOf`, the watchdog never fired (the user did not pass
    // `--stack-dump-on-timeout`), and the failure was indistinguishable
    // from a successful run that produced no output.
    //
    // The new hook routes everything to `stderr` directly — the only sink
    // that is guaranteed to survive every other failure mode (tracing
    // subscriber not initialized, WARN-level filter, panic firing from a
    // worker thread before `run()` builds the subscriber). For
    // bootstrap-path panics that are still expected to be quiet, the
    // outer `safe_native_call` continues to swallow them via its own
    // `catch_unwind` — the hook fires *before* `catch_unwind` catches
    // the unwind, but only the recovery path knows the panic was caught,
    // so we always emit at hook time. The cost is a couple of extra
    // stderr lines on the (rare) bootstrap-panic path; the gain is that
    // every crash is now traceable.
    //
    // Honors `RUST_BACKTRACE=1` / `RUST_BACKTRACE=full` the same way the
    // default Rust hook does (we use `std::backtrace::Backtrace::capture`
    // which respects the env var).
    let _default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        use std::io::Write;
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<panic payload was not a string>".to_string());
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>");

        // Mirror `safe_native_call`'s well-known bootstrap-path quiet
        // list (vm/src/vm/vm_exec.rs:253). These are caught one frame
        // up and surfaced via a single user-facing warning; we route
        // them to `tracing::debug!` so the full panic doesn't spam the
        // user's stderr during initPhase1. Anything *not* in this list
        // gets the full visibility treatment.
        // Keycloak Round 71: only quiet the bootstrap-class panics while
        // bootstrap is actually running (init level < 4). Once `main()`
        // is executing, an unaligned/null pointer panic from a native is
        // a real fault that must be visible — silently demoting it to
        // debug produces the "Keycloak exits in 5s with no output"
        // failure mode where 19 caught NPEs corrupt picocli state and
        // `parseAndRun` returns without ever calling `start-dev`.
        let in_bootstrap = cratonvm_native_api::init_level::get_init_level() < 4;
        let is_known_bootstrap_quiet =
            (msg.contains("unaligned pointer") || msg.contains("null pointer")) && in_bootstrap;

        if is_known_bootstrap_quiet {
            if let Some(loc) = info.location() {
                tracing::debug!(
                    target: "cratonvm::panic",
                    file = loc.file(),
                    line = loc.line(),
                    column = loc.column(),
                    thread = thread_name,
                    "bootstrap-path panic (caught upstream): {msg}",
                );
            } else {
                tracing::debug!(
                    target: "cratonvm::panic",
                    thread = thread_name,
                    "bootstrap-path panic (caught upstream): {msg}",
                );
            }
            return;
        }

        // Visible path: write directly to stderr because tracing may
        // be filtering at WARN+ and we cannot rely on the subscriber
        // having been initialised (e.g. when the panic fires before
        // `run()` builds it).
        let mut stderr = std::io::stderr().lock();
        if let Some(loc) = info.location() {
            let _ = writeln!(
                stderr,
                "thread '{thread_name}' panicked at {}:{}:{}:\n{msg}",
                loc.file(),
                loc.line(),
                loc.column(),
            );
        } else {
            let _ = writeln!(stderr, "thread '{thread_name}' panicked:\n{msg}");
        }
        // Backtrace only when explicitly requested — matches the stock
        // Rust hook semantics so users opting out of backtrace still see
        // the panic message but no overhead.
        let bt = std::backtrace::Backtrace::capture();
        if bt.status() == std::backtrace::BacktraceStatus::Captured {
            let _ = writeln!(stderr, "stack backtrace:\n{bt}");
        } else {
            let _ = writeln!(
                stderr,
                "note: run with `RUST_BACKTRACE=1` environment variable \
                 to display a backtrace",
            );
        }
        let _ = stderr.flush();
        // ALSO mirror through tracing at WARN level so log aggregators
        // that key on tracing still see the panic. We do this *after*
        // the direct stderr write so the user-visible message lands
        // even when the tracing subscriber is unavailable or filtering
        // it out.
        if let Some(loc) = info.location() {
            tracing::warn!(
                target: "cratonvm::panic",
                file = loc.file(),
                line = loc.line(),
                column = loc.column(),
                thread = thread_name,
                "panic: {msg}",
            );
        } else {
            tracing::warn!(target: "cratonvm::panic", thread = thread_name, "panic: {msg}");
        }
    }));

    // The interpreter uses recursive Rust calls for Java method invocations.
    // Deep Java call stacks (e.g. Quarkus bootstrap, binary-trees-style
    // recursion under JIT dispatch) can exceed the default 8 MB Rust stack.
    // 64 MB cleared every workload up to and including QuickBenchLong's
    // first four kernels but `binaryTrees(18)` (~524 k recursive
    // invocations through `jit_invoke_dispatch` / interpreter fallback
    // helpers, each adding one Rust frame) drove the main-vm thread past
    // it on some platforms — manifesting as `thread 'main-vm' has
    // overflowed its stack` (rc=139 on Linux) before reaching the GC
    // safepoint that would have triggered an OOME. Bump to 128 MB so
    // even the deepest recursive workloads have headroom; the upper
    // bound is virtual-address-space-only on 64-bit OSes (no commit
    // until the page is touched), so the practical cost is zero.
    // DBG (CRATONVM_DBG_HEARTBEAT=<ms>): forensic liveness heartbeat, armed
    // from the launcher thread (not main-vm) so it keeps writing even if
    // main-vm itself hangs or dies without unwinding. See its own doc
    // comment for what it's for.
    cratonvm_vm::runtime::heartbeat_watch::arm_from_env();

    let builder = std::thread::Builder::new()
        .name("main-vm".into())
        .stack_size(128 * 1024 * 1024);
    let handler = builder
        .spawn(|| {
            // DBG (CRATONVM_DBG_HANGWALK=<secs>): arm the native-stack-walk
            // watchdog on THIS (main-vm) thread — the one that runs the
            // interpreter — not the launcher thread that just joins it.
            cratonvm_vm::runtime::stwhang_watch::arm_from_env();
            // Diagnosability (Keycloak Gap 9): the boot can exit SILENTLY — `run()`
            // returns `Ok` (e.g. waitForExit returned / VM main finished) or an `Err`
            // whose `Display` ({e:#}) renders empty, so the prior `eprintln!("{e:#}")`
            // could print nothing before `exit(1)`. Always surface the outcome
            // (Display AND Debug) and flush stderr so a startup failure that ends the
            // process is never invisible. Additive logging only — no behaviour change.
            use std::io::Write as _;
            match run() {
                Ok(()) => {
                    eprintln!("[cratonvm] main-vm run() returned Ok — VM main exiting normally");
                    let _ = std::io::stderr().flush();
                }
                Err(e) => {
                    eprintln!("[cratonvm] main-vm run() returned Err: {e:#}");
                    eprintln!("[cratonvm] main-vm run() Err (debug): {e:?}");
                    let _ = std::io::stderr().flush();
                    std::process::exit(1);
                }
            }
        })
        .expect("failed to spawn main-vm thread");
    handler.join().unwrap_or_else(|e| {
        eprintln!("main-vm thread panicked: {:?}", e);
        std::process::exit(1);
    });
}

/// Parse a JVM-style memory size string (e.g., "256m", "1g", "1024k").
fn parse_size(s: &str) -> Option<usize> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let (num_str, multiplier) = match s.as_bytes().last()? {
        b'k' | b'K' => (&s[..s.len() - 1], 1024),
        b'm' | b'M' => (&s[..s.len() - 1], 1024 * 1024),
        b'g' | b'G' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };

    // Use checked_mul so a huge value (e.g. `99999999999g`) fails the parse
    // rather than wrapping silently in release / panicking in debug. A parsed
    // size of 0 is also rejected as invalid (a 0-byte heap is meaningless).
    let n = num_str.parse::<usize>().ok()?;
    let bytes = n.checked_mul(multiplier)?;
    if bytes == 0 {
        return None;
    }
    Some(bytes)
}

/// Total physical RAM in bytes, or `None` if it can't be determined.
///
/// Used for HotSpot-style ergonomic default-heap sizing when the user did not
/// pass an explicit `-Xmx`. Mirrors the platform probes in
/// `vm::runtime::crash_handler` but returns the raw byte count.
fn physical_ram_bytes() -> Option<u64> {
    #[cfg(target_os = "windows")]
    {
        #[repr(C)]
        struct MemoryStatusEx {
            dw_length: u32,
            dw_memory_load: u32,
            ull_total_phys: u64,
            ull_avail_phys: u64,
            ull_total_page_file: u64,
            ull_avail_page_file: u64,
            ull_total_virtual: u64,
            ull_avail_virtual: u64,
            ull_avail_extended_virtual: u64,
        }
        extern "system" {
            fn GlobalMemoryStatusEx(lp_buffer: *mut MemoryStatusEx) -> i32;
        }
        let mut status = MemoryStatusEx {
            dw_length: std::mem::size_of::<MemoryStatusEx>() as u32,
            dw_memory_load: 0,
            ull_total_phys: 0,
            ull_avail_phys: 0,
            ull_total_page_file: 0,
            ull_avail_page_file: 0,
            ull_total_virtual: 0,
            ull_avail_virtual: 0,
            ull_avail_extended_virtual: 0,
        };
        let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
        if ok != 0 && status.ull_total_phys > 0 {
            return Some(status.ull_total_phys);
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        let content = std::fs::read_to_string("/proc/meminfo").ok()?;
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                let kb = rest.split_whitespace().next()?.parse::<u64>().ok()?;
                return Some(kb * 1024);
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        String::from_utf8(out.stdout)
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// HotSpot-style ergonomic default max heap, applied only when the user did
/// not pass an explicit `-Xmx`.
///
/// Approximates a stock JDK's `-XX:MaxRAMPercentage=25` ergonomics: max heap =
/// 1/4 of physical RAM, **floored** at the historical 256 MB default (so this
/// only ever *raises* the heap above the prior baseline) and **capped** at
/// `MAX_ERGONOMIC_HEAP`. Without it, real-world apps (Spring / Mockito /
/// ByteBuddy / JUnit) thrash GC at 256 MB and look like a hang where HotSpot —
/// which auto-sizes — finishes fine (e.g. buildpack `LifecycleTests`).
///
/// The cap exists because CratonVM's generational heap **eagerly commits** its
/// arenas (`Arena::new` → `vec![0u8; cap]`): an uncapped 1/4-of-RAM heap (e.g.
/// 16 GB on a 64 GB host) would charge ~16 GB of commit per process. The cap
/// keeps the default's commit bounded while still giving GC-heavy workloads
/// enough room. (If the heap is ever made lazily-committed, the cap can grow
/// or be removed to fully match HotSpot.)
///
/// When running under `-XX:+UseContainerSupport` inside a memory-constrained
/// container, the basis for the fraction is the **cgroup memory limit** rather
/// than host RAM — HotSpot's `MaxRAMPercentage` applies to the container limit,
/// not the host total, so on a 64 GB host with `--memory=512m` the default heap
/// is sized off 512 MB, not 64 GB. `container_mem_limit` is the detected cgroup
/// limit (or `None` when uncontained / container support is off); the basis is
/// then `min(physical RAM, cgroup limit)`.
///
/// Opt out with `CRATONVM_DEFAULT_HEAP_ERGONOMICS=0` (fixed 256 MB default), or
/// override the cap with `CRATONVM_DEFAULT_HEAP_MAX_MB=<N>`. An explicit `-Xmx`
/// always wins over all of this.
fn ergonomic_default_max_heap(container_mem_limit: Option<u64>) -> Option<usize> {
    if std::env::var("CRATONVM_DEFAULT_HEAP_ERGONOMICS").as_deref() == Ok("0") {
        return None;
    }
    let cap = std::env::var("CRATONVM_DEFAULT_HEAP_MAX_MB")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|mb| mb.saturating_mul(1024 * 1024))
        .unwrap_or(MAX_ERGONOMIC_HEAP);
    let phys = physical_ram_bytes()?;
    // Inside a memory-constrained container, size from the smaller of host RAM
    // and the cgroup limit so the default heap never overshoots the container.
    let basis = match container_mem_limit {
        Some(limit) => phys.min(limit),
        None => phys,
    };
    Some(clamp_ergonomic_heap(basis, cap))
}

/// Floor for the ergonomic default heap (the historical 256 MB baseline).
const ERGONOMIC_HEAP_FLOOR: u64 = 256 * 1024 * 1024;
/// Default cap for the ergonomic default heap (4 GiB). See
/// [`ergonomic_default_max_heap`] for why the cap exists (eager arena commit).
const MAX_ERGONOMIC_HEAP: u64 = 4 * 1024 * 1024 * 1024;

/// Pure clamp for the ergonomic default heap: take 1/4 of `basis`, cap it at
/// `cap` (itself floored so a tiny `CRATONVM_DEFAULT_HEAP_MAX_MB` can't drop
/// below the 256 MB floor), floor it at 256 MB, and finally bound it by `basis`
/// itself so a tiny container is never handed more than its whole limit.
fn clamp_ergonomic_heap(basis: u64, cap: u64) -> usize {
    let quarter = basis / 4;
    let capped = quarter.min(cap.max(ERGONOMIC_HEAP_FLOOR));
    let floored = capped.max(ERGONOMIC_HEAP_FLOOR);
    let bounded = floored.min(basis);
    usize::try_from(bounded).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Ergonomic default-heap clamp — pure math, exercised directly so the
    // container-vs-host basis logic is covered without a real cgroupfs.
    // -----------------------------------------------------------------------

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    #[test]
    fn ergo_clamp_quarter_of_large_host() {
        // 16 GiB basis → 1/4 = 4 GiB, exactly the default cap.
        assert_eq!(
            clamp_ergonomic_heap(16 * GIB, MAX_ERGONOMIC_HEAP),
            4 * GIB as usize
        );
    }

    #[test]
    fn ergo_clamp_capped_for_huge_host() {
        // 64 GiB basis → 1/4 = 16 GiB, capped to the 4 GiB default.
        assert_eq!(
            clamp_ergonomic_heap(64 * GIB, MAX_ERGONOMIC_HEAP),
            4 * GIB as usize
        );
    }

    #[test]
    fn ergo_clamp_floored_small_basis() {
        // 4 GiB basis → 1/4 = 1 GiB (above the 256 MiB floor).
        assert_eq!(
            clamp_ergonomic_heap(4 * GIB, MAX_ERGONOMIC_HEAP),
            GIB as usize
        );
        // 512 MiB basis → 1/4 = 128 MiB, raised to the 256 MiB floor.
        assert_eq!(
            clamp_ergonomic_heap(512 * MIB, MAX_ERGONOMIC_HEAP),
            (256 * MIB) as usize
        );
    }

    #[test]
    fn ergo_clamp_never_exceeds_basis() {
        // A 256 MiB container: 1/4 = 64 MiB, the floor would push it to
        // 256 MiB — but it must never exceed the basis itself, so it stays
        // at exactly 256 MiB (not above), and a 200 MiB basis stays at 200.
        assert_eq!(
            clamp_ergonomic_heap(256 * MIB, MAX_ERGONOMIC_HEAP),
            (256 * MIB) as usize
        );
        assert_eq!(
            clamp_ergonomic_heap(200 * MIB, MAX_ERGONOMIC_HEAP),
            (200 * MIB) as usize
        );
    }

    #[test]
    fn ergo_clamp_custom_cap_floored() {
        // A tiny cap override can't drop the result below the 256 MiB floor.
        assert_eq!(
            clamp_ergonomic_heap(16 * GIB, 64 * MIB),
            (256 * MIB) as usize
        );
        // A 2 GiB cap bites on a big host (8 GiB → 1/4 = 2 GiB).
        assert_eq!(clamp_ergonomic_heap(8 * GIB, 2 * GIB), (2 * GIB) as usize);
    }

    #[test]
    fn watchdog_is_not_armed_by_default() {
        assert_eq!(resolve_watchdog_timeout(None, None), None);
    }

    #[test]
    fn watchdog_env_default_is_opt_in() {
        assert_eq!(resolve_watchdog_timeout(None, Some("300")), Some(300));
        assert_eq!(resolve_watchdog_timeout(None, Some("0")), None);
        assert_eq!(resolve_watchdog_timeout(None, Some("not-a-number")), None);
    }

    #[test]
    fn watchdog_explicit_timeout_wins_and_zero_disables() {
        assert_eq!(resolve_watchdog_timeout(Some(7), Some("300")), Some(7));
        assert_eq!(resolve_watchdog_timeout(Some(0), Some("300")), None);
    }

    // -----------------------------------------------------------------------
    // insert_program_args_separator tests — `java`-launcher positional
    // semantics: tokens after the program selector are program args.
    // -----------------------------------------------------------------------

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sep_jar_then_program_help_is_program_arg() {
        // `-jar foo.jar --help`: `--help` must become a program arg.
        let out = insert_program_args_separator(argv(&["java", "-jar", "foo.jar", "--help"]));
        assert_eq!(out, argv(&["java", "-jar", "foo.jar", "--", "--help"]));
    }

    #[test]
    fn sep_jar_with_leading_opts() {
        // Launcher options before `-jar` stay in the leading section.
        let out = insert_program_args_separator(argv(&[
            "java",
            "--java-home",
            "C:/jdk",
            "-jar",
            "app.jar",
            "--list-modules",
            "--version",
        ]));
        assert_eq!(
            out,
            argv(&[
                "java",
                "--java-home",
                "C:/jdk",
                "-jar",
                "app.jar",
                "--",
                "--list-modules",
                "--version",
            ])
        );
    }

    #[test]
    fn sep_bare_main_class_then_program_args() {
        // `-cp bench Main --help 0`: `Main` selects the program; everything
        // after it (including `--help`) is a program arg.
        let out =
            insert_program_args_separator(argv(&["java", "-cp", "bench", "Main", "--help", "0"]));
        assert_eq!(
            out,
            argv(&["java", "-cp", "bench", "Main", "--", "--help", "0"])
        );
    }

    #[test]
    fn sep_no_program_left_unchanged() {
        // `java --version` with no program: nothing to delimit; the
        // launcher must still handle `--version` itself.
        let inp = argv(&["java", "--version"]);
        assert_eq!(insert_program_args_separator(inp.clone()), inp);
        let inp = argv(&["java", "--help"]);
        assert_eq!(insert_program_args_separator(inp.clone()), inp);
    }

    #[test]
    fn normalize_version_short_forms() {
        assert_eq!(
            normalize_java_launcher_argv(argv(&["java", "-version"])),
            argv(&["java", "--version"])
        );
        assert_eq!(
            normalize_java_launcher_argv(argv(&["java", "-v"])),
            argv(&["java", "--version"])
        );
    }

    #[test]
    fn sep_explicit_separator_respected() {
        // An explicit `--` already delimits program args; copy verbatim.
        let inp = argv(&["java", "-jar", "a.jar", "--", "--help"]);
        assert_eq!(insert_program_args_separator(inp.clone()), inp);
    }

    #[test]
    fn sep_value_token_not_mistaken_for_main_class() {
        // The classpath string after `-cp` is a value, not the main class.
        let out = insert_program_args_separator(argv(&["java", "-cp", "lib.jar", "Main", "arg1"]));
        assert_eq!(out, argv(&["java", "-cp", "lib.jar", "Main", "--", "arg1"]));
    }

    #[test]
    fn sep_inline_jar_value() {
        // `--jar=foo.jar` inline form: `--version` after it is a program arg.
        let out = insert_program_args_separator(argv(&["java", "--jar=foo.jar", "--version"]));
        assert_eq!(out, argv(&["java", "--jar=foo.jar", "--", "--version"]));
    }

    // -----------------------------------------------------------------------
    // parse_size tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_memory_sizes() {
        assert_eq!(parse_size("256m"), Some(256 * 1024 * 1024));
        assert_eq!(parse_size("1g"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size("1024k"), Some(1024 * 1024));
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn parse_size_whitespace() {
        assert_eq!(parse_size("  256m  "), Some(256 * 1024 * 1024));
        assert_eq!(parse_size("   "), None);
    }

    #[test]
    fn parse_size_invalid() {
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size("m"), None);
        assert_eq!(parse_size("-1m"), None);
    }

    #[test]
    fn parse_size_overflow() {
        // Multiplying by the suffix factor must not overflow `usize`:
        // an oversized input returns None instead of panicking/wrapping.
        assert_eq!(parse_size("999999999999g"), None);
        assert_eq!(parse_size(&format!("{}g", usize::MAX)), None);
    }

    // -----------------------------------------------------------------------
    // validate_class_name tests
    // -----------------------------------------------------------------------

    #[test]
    fn valid_simple_class_name() {
        assert!(validate_class_name("Main").is_ok());
    }

    #[test]
    fn valid_fully_qualified_class_name() {
        assert!(validate_class_name("com/example/Main").is_ok());
    }

    #[test]
    fn valid_class_name_with_underscore_and_dollar() {
        assert!(validate_class_name("com/_internal/$Helper").is_ok());
        assert!(validate_class_name("$Proxy0").is_ok());
        assert!(validate_class_name("_Private").is_ok());
    }

    #[test]
    fn valid_class_name_with_digits() {
        assert!(validate_class_name("com/example/Test123").is_ok());
    }

    #[test]
    fn invalid_empty_class_name() {
        let err = validate_class_name("").unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn invalid_class_name_starts_with_digit() {
        let err = validate_class_name("com/123bad/Main").unwrap_err();
        assert!(err.to_string().contains("must start with"));
    }

    #[test]
    fn invalid_class_name_double_slash() {
        let err = validate_class_name("com//Main").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn invalid_class_name_leading_slash() {
        let err = validate_class_name("/com/Main").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn invalid_class_name_trailing_slash() {
        let err = validate_class_name("com/Main/").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn invalid_class_name_special_chars() {
        let err = validate_class_name("com/ex@mple/Main").unwrap_err();
        assert!(err.to_string().contains("invalid character"));
    }

    #[test]
    fn invalid_class_name_spaces() {
        let err = validate_class_name("com/my class/Main").unwrap_err();
        assert!(err.to_string().contains("invalid character"));
    }

    #[test]
    fn invalid_class_name_hyphen() {
        let err = validate_class_name("com/my-pkg/Main").unwrap_err();
        assert!(err.to_string().contains("invalid character"));
    }

    // -----------------------------------------------------------------------
    // extract_system_properties tests
    // -----------------------------------------------------------------------

    #[test]
    fn extract_d_properties() {
        let raw = vec![
            "cratonvm".to_string(),
            "-Djboss.home.dir=C:/craton/kc16".to_string(),
            "-Dmy.flag".to_string(),
            "com.example.Main".to_string(),
        ];
        let (filtered, props) = extract_system_properties(raw);
        assert_eq!(filtered, vec!["cratonvm", "com.example.Main"]);
        assert_eq!(
            props,
            vec![
                ("jboss.home.dir".to_string(), "C:/craton/kc16".to_string()),
                ("my.flag".to_string(), String::new()),
            ]
        );
    }

    #[test]
    fn extract_d_no_properties() {
        let raw = vec!["cratonvm".to_string(), "Main".to_string()];
        let (filtered, props) = extract_system_properties(raw);
        assert_eq!(filtered, vec!["cratonvm", "Main"]);
        assert!(props.is_empty());
    }

    #[test]
    fn extract_d_value_with_equals() {
        // -Dkey=val=ue  →  key = "val=ue"
        let raw = vec!["cratonvm".to_string(), "-Dpath=a=b".to_string()];
        let (_, props) = extract_system_properties(raw);
        assert_eq!(props, vec![("path".to_string(), "a=b".to_string())]);
    }

    #[test]
    fn normalize_classpath_and_jar_for_clap() {
        let raw = vec![
            "cratonvm".to_string(),
            "-classpath".to_string(),
            "a;b".to_string(),
            "-cp=c:d".to_string(),
            "-jar".to_string(),
            "app.jar".to_string(),
            "arg1".to_string(),
        ];
        assert_eq!(
            normalize_java_launcher_argv(raw),
            vec![
                "cratonvm",
                "--classpath",
                "a;b",
                "--classpath",
                "c:d",
                "--jar",
                "app.jar",
                "arg1",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
    }

    // -----------------------------------------------------------------------
    // T6 extract_hotspot_flags tests
    // -----------------------------------------------------------------------

    #[test]
    fn hotspot_flag_enables_heap_dump_on_oom() {
        let raw = vec![
            "cratonvm".to_string(),
            "-XX:+HeapDumpOnOutOfMemoryError".to_string(),
            "Main".to_string(),
        ];
        let (filtered, flags) = extract_hotspot_flags(raw);
        assert_eq!(filtered, vec!["cratonvm", "Main"]);
        assert_eq!(flags.heap_dump_on_oom, Some(true));
    }

    #[test]
    fn hotspot_flag_disables_heap_dump_on_oom() {
        let raw = vec![
            "cratonvm".to_string(),
            "-XX:-HeapDumpOnOutOfMemoryError".to_string(),
            "Main".to_string(),
        ];
        let (_, flags) = extract_hotspot_flags(raw);
        assert_eq!(flags.heap_dump_on_oom, Some(false));
    }

    #[test]
    fn hotspot_flag_extracts_heap_dump_path() {
        let raw = vec![
            "cratonvm".to_string(),
            "-XX:HeapDumpPath=/tmp/heap.hprof".to_string(),
            "Main".to_string(),
        ];
        let (filtered, flags) = extract_hotspot_flags(raw);
        assert_eq!(filtered, vec!["cratonvm", "Main"]);
        assert_eq!(flags.heap_dump_path.as_deref(), Some("/tmp/heap.hprof"));
    }

    #[test]
    fn hotspot_flag_preserves_all_agent_tokens() {
        let raw = vec![
            "cratonvm".to_string(),
            "-agentlib:jdwp=transport=dt_socket,server=y,address=5005".to_string(),
            "-agentpath:/opt/myagent.so=trace".to_string(),
            "-javaagent:/opt/bytebuddy.jar".to_string(),
            "Main".to_string(),
        ];
        let (filtered, flags) = extract_hotspot_flags(raw);
        assert_eq!(filtered, vec!["cratonvm", "Main"]);
        assert_eq!(flags.agent_options.len(), 3);
        assert!(flags.agent_options[0].starts_with("-agentlib:jdwp="));
        assert!(flags.agent_options[1].starts_with("-agentpath:/opt/myagent.so"));
        assert!(flags.agent_options[2].starts_with("-javaagent:/opt/bytebuddy.jar"));
    }

    #[test]
    fn hotspot_flag_passes_unknown_through() {
        let raw = vec![
            "cratonvm".to_string(),
            "--foo".to_string(),
            "Main".to_string(),
        ];
        let (filtered, flags) = extract_hotspot_flags(raw);
        assert_eq!(filtered, vec!["cratonvm", "--foo", "Main"]);
        assert!(flags.agent_options.is_empty());
        assert!(flags.heap_dump_on_oom.is_none());
        assert!(flags.heap_dump_path.is_none());
    }

    #[test]
    fn heap_dump_flags_survive_full_launcher_pipeline() {
        let argv0 = argv(&[
            "java",
            "-XX:+HeapDumpOnOutOfMemoryError",
            "-XX:HeapDumpPath=/tmp/heap.hprof",
            "Main",
        ]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, flags) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must accept heap dump flags");

        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
        assert_eq!(flags.heap_dump_on_oom, Some(true));
        assert_eq!(flags.heap_dump_path.as_deref(), Some("/tmp/heap.hprof"));
    }

    // -----------------------------------------------------------------------
    // expand_aggregate_jars tests
    // -----------------------------------------------------------------------

    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("cratonvm-cli-{label}-{pid}-{id}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn expand_missing_netty_all_substitutes_split_jars() {
        let dir = unique_temp_dir("netty-all");
        // Create split jars next to where netty-all.jar would live.
        for name in ["netty-common.jar", "netty-transport.jar", "other.jar"] {
            std::fs::write(dir.join(name), b"pk").unwrap();
        }
        let missing = dir.join("netty-all.jar").to_string_lossy().into_owned();
        let expanded = expand_aggregate_jars(vec![missing.clone()]);
        // netty-all.jar should be gone; the two netty split jars should be
        // present; "other.jar" should NOT have been pulled in.
        assert!(
            !expanded.iter().any(|e| e == &missing),
            "missing aggregate jar must be stripped, got {expanded:?}"
        );
        let has = |n: &str| expanded.iter().any(|e| e.to_ascii_lowercase().ends_with(n));
        assert!(
            has("netty-common.jar"),
            "missing netty-common: {expanded:?}"
        );
        assert!(
            has("netty-transport.jar"),
            "missing netty-transport: {expanded:?}"
        );
        assert!(
            !has("other.jar"),
            "unexpected 'other.jar' in result: {expanded:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn expand_preserves_existing_aggregate() {
        let dir = unique_temp_dir("netty-all-real");
        let aggregate = dir.join("netty-all.jar");
        std::fs::write(&aggregate, b"pk").unwrap();
        // A sibling split jar also exists; we must NOT add it because the
        // aggregate is present and authoritative.
        std::fs::write(dir.join("netty-common.jar"), b"pk").unwrap();
        let entry = aggregate.to_string_lossy().into_owned();
        let expanded = expand_aggregate_jars(vec![entry.clone()]);
        assert_eq!(expanded, vec![entry]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn non_jar_archive_staging_cleanup_removes_temp_copy() {
        let dir = unique_temp_dir("stage-war");
        let archive = dir.join("app.war");
        std::fs::write(&archive, b"fake archive").unwrap();

        let (entry, cleanup) = classpath_entry_for_archive(&archive).unwrap();
        let cleanup = cleanup.expect("non-.jar archive should be staged");
        assert_ne!(entry, archive);
        assert_eq!(entry.extension().and_then(|e| e.to_str()), Some("jar"));
        assert!(
            entry.exists(),
            "staged copy should exist while guard is live"
        );
        assert_eq!(std::fs::read(&entry).unwrap(), b"fake archive");

        drop(cleanup);
        assert!(
            !entry.exists(),
            "staged copy should be removed on guard drop"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    // -----------------------------------------------------------------------
    // quarkus_signature_present tests — PERF gate for the multi-dir
    // classpath walk. A plain jar must NOT trip the sniff; the canonical
    // Quarkus signature artifacts (and the Keycloak one-level-deep layout)
    // MUST.
    // -----------------------------------------------------------------------

    #[test]
    fn quarkus_sniff_false_for_plain_jar() {
        let dir = unique_temp_dir("qsniff-plain");
        let jar = dir.join("hello.jar");
        std::fs::write(&jar, b"pk").unwrap();
        assert!(
            !quarkus_signature_present(&jar),
            "a plain jar with no Quarkus artifacts must skip the walk"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quarkus_sniff_true_for_quarkus_run_jar_sibling() {
        let dir = unique_temp_dir("qsniff-run");
        let jar = dir.join("app.jar");
        std::fs::write(&jar, b"pk").unwrap();
        std::fs::write(dir.join("quarkus-run.jar"), b"pk").unwrap();
        assert!(
            quarkus_signature_present(&jar),
            "quarkus-run.jar next to the jar must enable the walk"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quarkus_sniff_true_for_quarkus_subdir() {
        let dir = unique_temp_dir("qsniff-subdir");
        let jar = dir.join("app.jar");
        std::fs::write(&jar, b"pk").unwrap();
        std::fs::create_dir_all(dir.join("quarkus")).unwrap();
        assert!(
            quarkus_signature_present(&jar),
            "a quarkus/ subdir must enable the walk"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quarkus_sniff_true_for_parent_dir_signature() {
        // Keycloak packaging puts quarkus-run.jar one level up from the
        // runner jar (which lives in lib/). The sniff probes the parent.
        let dir = unique_temp_dir("qsniff-parent");
        let lib = dir.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let jar = lib.join("quarkus-run.jar");
        std::fs::write(&jar, b"pk").unwrap();
        // Signature artifact lives in the PARENT (dir), not lib/.
        std::fs::write(dir.join("quarkus-application.dat"), b"x").unwrap();
        assert!(
            quarkus_signature_present(&jar),
            "a signature in the jar's parent dir must enable the walk"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn expand_keeps_missing_entry_when_no_siblings() {
        let dir = unique_temp_dir("netty-lonely");
        let missing = dir.join("netty-all.jar").to_string_lossy().into_owned();
        let expanded = expand_aggregate_jars(vec![missing.clone()]);
        // No split jars exist, so the entry is kept as-is (so downstream
        // logging still surfaces the missing-jar message).
        assert_eq!(expanded, vec![missing]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn expand_ignores_non_aggregate_names() {
        let dir = unique_temp_dir("no-match");
        let missing = dir
            .join("does-not-exist.jar")
            .to_string_lossy()
            .into_owned();
        let expanded = expand_aggregate_jars(vec![missing.clone()]);
        assert_eq!(expanded, vec![missing]);
        let _ = std::fs::remove_dir_all(dir);
    }

    // -----------------------------------------------------------------------
    // HotSpot single-dash compatibility rewrites — C36 acceptance.
    //
    // Stock `java -Xmx256m -classpath x Main` must parse, otherwise the
    // `[[bin]] name = "java"` alias is useless for Maven Surefire / Gradle.
    // -----------------------------------------------------------------------

    #[test]
    fn hotspot_xmx_inline_rewrites_to_clap_long() {
        let raw = argv(&["java", "-Xmx256m", "-classpath", "x", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(
            out,
            argv(&["java", "--Xmx", "256m", "--classpath", "x", "Main"])
        );
    }

    #[test]
    fn hotspot_xmx_separate_token_rewrites_to_clap_long() {
        let raw = argv(&["java", "-Xmx", "1g", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "--Xmx", "1g", "Main"]));
    }

    #[test]
    fn hotspot_xms_inline_is_dropped_keeping_class_name() {
        // `-Xms512m` (inline value) is accepted-and-ignored: the whole token
        // is dropped and the main-class name is untouched.
        let raw = argv(&["java", "-Xms512m", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "Main"]));
    }

    #[test]
    fn hotspot_xms_separate_token_drops_value_not_class_name() {
        // B1 regression: `-Xms 512m` (separate value token) must drop BOTH
        // the flag and its value. Previously only `-Xms` was dropped, leaving
        // `512m` to be mistaken for the main-class positional and shifting
        // `Main` into a program arg. Maven Surefire / Gradle forks emit this
        // form. The value sits adjacent here because `-Xms` is in
        // VALUE_TAKING_OPTS, so we mirror the full pre-clap pipeline.
        let stage1 = insert_program_args_separator(argv(&["java", "-Xms", "512m", "Main"]));
        let out = normalize_java_launcher_argv(stage1);
        // `512m` is gone; `Main` survives as the (only) main-class positional,
        // followed by the launcher-inserted `--` program-args separator.
        assert_eq!(out, argv(&["java", "Main", "--"]));
    }

    #[test]
    fn hotspot_xms_separate_token_passes_clap_after_full_pipeline() {
        // B1 acceptance: stock `java -Xms512m -Xmx256m -classpath x Main`
        // with the separate-token `-Xms 512m` form must parse end-to-end with
        // `Main` resolved as the main class (not the heap-size value `512m`).
        let argv0: Vec<String> = argv(&[
            "java",
            "-Xms",
            "512m",
            "-Xmx",
            "256m",
            "-classpath",
            "x",
            "Main",
        ]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed =
            Args::try_parse_from(stage4).expect("clap must accept HotSpot separate-token -Xms");
        assert_eq!(parsed.max_heap.as_deref(), Some("256m"));
        assert_eq!(parsed.classpath.as_deref(), Some("x"));
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    }

    #[test]
    fn hotspot_xshare_colon_rewrites_to_clap_long() {
        let raw = argv(&["java", "-Xshare:on", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "--Xshare", "on", "Main"]));
    }

    #[test]
    fn hotspot_xverify_none_collapses_to_noverify_flag() {
        let raw = argv(&["java", "-Xverify:none", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "--noverify", "Main"]));
    }

    #[test]
    fn hotspot_xverify_remote_rewrites_to_clap_long() {
        let raw = argv(&["java", "-Xverify:remote", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "--Xverify", "remote", "Main"]));
    }

    #[test]
    fn hotspot_xbootclasspath_inline_rewrites_to_clap_long() {
        let raw = argv(&["java", "-Xbootclasspath:/opt/boot", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(
            out,
            argv(&["java", "--Xbootclasspath", "/opt/boot", "Main"])
        );
    }

    #[test]
    fn hotspot_xbootclasspath_append_and_prepend_normalise() {
        // /a (append) and /p (prepend) collapse to a plain replace — cratonvm
        // does not model the three boot-CP positions separately.
        let raw = argv(&["java", "-Xbootclasspath/a:/opt/extra", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(
            out,
            argv(&["java", "--Xbootclasspath", "/opt/extra", "Main"])
        );

        let raw = argv(&["java", "-Xbootclasspath/p:/opt/pre", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "--Xbootclasspath", "/opt/pre", "Main"]));
    }

    #[test]
    fn hotspot_xlog_colon_rewrites_to_clap_long() {
        let raw = argv(&["java", "-Xlog:gc*=info", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "--Xlog", "gc*=info", "Main"]));
    }

    #[test]
    fn hotspot_xx_shared_archive_file_rewrites_to_clap_long() {
        let raw = argv(&["java", "-XX:SharedArchiveFile=app.jsa", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(
            out,
            argv(&["java", "--XX:SharedArchiveFile", "app.jsa", "Main"])
        );
    }

    #[test]
    fn hotspot_xx_aot_flags_rewrite_to_clap_long() {
        let raw = argv(&[
            "java",
            "-XX:AOTMode=training",
            "-XX:AOTCache=in.aot",
            "-XX:AOTCacheOutput=out.aot",
            "Main",
        ]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(
            out,
            argv(&[
                "java",
                "--XX:AOTMode",
                "training",
                "--XX:AOTCache",
                "in.aot",
                "--XX:AOTCacheOutput",
                "out.aot",
                "Main",
            ])
        );
    }

    #[test]
    fn hotspot_xx_use_container_support_toggle() {
        // `-XX:-UseContainerSupport` -> clap long form.
        let out = normalize_java_launcher_argv(argv(&["java", "-XX:-UseContainerSupport", "Main"]));
        assert_eq!(out, argv(&["java", "--XX:-UseContainerSupport", "Main"]));
        // `-XX:+UseContainerSupport` is the default; gets dropped.
        let out = normalize_java_launcher_argv(argv(&["java", "-XX:+UseContainerSupport", "Main"]));
        assert_eq!(out, argv(&["java", "Main"]));
    }

    #[test]
    fn hotspot_xx_useg1gc_normalizes_to_selector() {
        // `-XX:+UseG1GC` -> `--XX:UseGc G1` (value option, adjacent value).
        let out = normalize_java_launcher_argv(argv(&["java", "-XX:+UseG1GC", "Main"]));
        assert_eq!(out, argv(&["java", "--XX:UseGc", "G1", "Main"]));
        // `-XX:-UseG1GC` explicitly reverts to the default Generational.
        let out = normalize_java_launcher_argv(argv(&["java", "-XX:-UseG1GC", "Main"]));
        assert_eq!(out, argv(&["java", "--XX:UseGc", "Generational", "Main"]));
    }

    #[test]
    fn hotspot_xx_unsupported_gc_is_forwarded_not_dropped() {
        // Selectors are forwarded verbatim; support validation happens at
        // config-apply time (`parse_gc_algorithm`), not here.
        for (flag, name) in [
            ("-XX:+UseParallelGC", "Parallel"),
            ("-XX:+UseSerialGC", "Serial"),
            ("-XX:+UseShenandoahGC", "Shenandoah"),
        ] {
            let out = normalize_java_launcher_argv(argv(&["java", flag, "Main"]));
            assert_eq!(out, argv(&["java", "--XX:UseGc", name, "Main"]), "{flag}");
        }
    }

    #[test]
    fn hotspot_xx_usezgc_normalizes_to_selector() {
        let out = normalize_java_launcher_argv(argv(&["java", "-XX:+UseZGC", "Main"]));
        assert_eq!(out, argv(&["java", "--XX:UseGc", "Z", "Main"]));
    }

    #[test]
    fn hotspot_usezgc_reaches_clap_as_selector_after_pipeline() {
        let argv0: Vec<String> = argv(&["java", "-XX:+UseZGC", "Main"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must accept -XX:+UseZGC");
        assert_eq!(parsed.gc_selector.as_deref(), Some("Z"));
    }

    #[test]
    fn hotspot_xx_non_gc_use_flags_not_mistaken_for_selector() {
        // `-XX:+Use*` flags that do NOT end in `GC` must not become a GC
        // selector. Supported non-GC flags keep their own mapping.
        let out =
            normalize_java_launcher_argv(argv(&["java", "-XX:+UseStringDeduplication", "Main"]));
        assert_eq!(out, argv(&["java", "--XX:StringDedup", "true", "Main"]));

        let out =
            normalize_java_launcher_argv(argv(&["java", "-XX:-UseStringDeduplication", "Main"]));
        assert_eq!(out, argv(&["java", "--XX:StringDedup", "false", "Main"]));

        let out = normalize_java_launcher_argv(argv(&["java", "-XX:+UseCompressedOops", "Main"]));
        assert_eq!(out, argv(&["java", "Main"]));
    }

    #[test]
    fn hotspot_useg1gc_reaches_clap_as_selector_after_pipeline() {
        // End-to-end: `-XX:+UseG1GC` survives the full pre-clap pipeline and
        // lands in `Args::gc_selector`, the field the config-apply step reads.
        let argv0: Vec<String> = argv(&["java", "-XX:+UseG1GC", "-classpath", "x", "Main"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must accept -XX:+UseG1GC");
        assert_eq!(parsed.gc_selector.as_deref(), Some("G1"));
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    }

    #[test]
    fn hotspot_string_dedup_reaches_clap_after_pipeline() {
        let argv0: Vec<String> = argv(&["java", "-XX:+UseStringDeduplication", "Main"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must accept string dedup flag");
        assert_eq!(parsed.g1_string_dedup.as_deref(), Some("true"));
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    }

    #[test]
    fn hotspot_repeated_gc_flags_last_wins() {
        // HotSpot honours the last `-XX:+Use*GC`; clap's `Set` action keeps the
        // last value, so a `-XX:+UseParallelGC -XX:+UseG1GC` pair selects G1.
        let argv0: Vec<String> = argv(&["java", "-XX:+UseParallelGC", "-XX:+UseG1GC", "Main"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must accept repeated GC flags");
        assert_eq!(parsed.gc_selector.as_deref(), Some("G1"));
    }

    #[test]
    fn hotspot_xx_audit_missing_natives_toggle() {
        let out = normalize_java_launcher_argv(argv(&["java", "-XX:+AuditMissingNatives", "Main"]));
        assert_eq!(out, argv(&["java", "--XX:AuditMissingNatives", "Main"]));
        // Disabled form drops the flag (default is off).
        let out = normalize_java_launcher_argv(argv(&["java", "-XX:-AuditMissingNatives", "Main"]));
        assert_eq!(out, argv(&["java", "Main"]));
    }

    #[test]
    fn hotspot_xx_show_code_details_toggle() {
        // `-XX:+ShowCodeDetailsInExceptionMessages` -> the explicit `=true` form.
        let out = normalize_java_launcher_argv(argv(&[
            "java",
            "-XX:+ShowCodeDetailsInExceptionMessages",
            "Main",
        ]));
        assert_eq!(
            out,
            argv(&[
                "java",
                "--XX:ShowCodeDetailsInExceptionMessages=true",
                "Main"
            ])
        );
        // Disabled form -> the explicit `=false` form (the default is now on,
        // so opting out must be representable, not merely dropped).
        let out = normalize_java_launcher_argv(argv(&[
            "java",
            "-XX:-ShowCodeDetailsInExceptionMessages",
            "Main",
        ]));
        assert_eq!(
            out,
            argv(&[
                "java",
                "--XX:ShowCodeDetailsInExceptionMessages=false",
                "Main"
            ])
        );
    }

    #[test]
    fn hotspot_noverify_rewrites_to_clap_long() {
        let raw = argv(&["java", "-noverify", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "--noverify", "Main"]));
    }

    #[test]
    fn hotspot_xmx_after_separator_is_program_arg() {
        // Tokens past `--` belong to the Java program, not the launcher.
        let raw = argv(&["java", "Main", "--", "-Xmx256m"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "Main", "--", "-Xmx256m"]));
    }

    #[test]
    fn hotspot_xmx_passes_clap_after_full_pipeline() {
        // C36 acceptance test: stock `java -Xmx256m -classpath x Main`
        // must parse end-to-end. Exercises the entire pre-clap pipeline
        // exactly as `run()` would.
        let argv0: Vec<String> = argv(&["java", "-Xmx256m", "-classpath", "x", "Main"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must accept HotSpot -Xmx");
        assert_eq!(parsed.max_heap.as_deref(), Some("256m"));
        assert_eq!(parsed.classpath.as_deref(), Some("x"));
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    }

    #[test]
    fn nojit_flag_is_accepted_by_clap() {
        // README documents `--nojit`; clap must accept it.
        let parsed = Args::try_parse_from(argv(&["cratonvm", "--nojit", "Main"]))
            .expect("clap must accept --nojit");
        assert!(parsed.nojit);
    }

    #[test]
    fn enable_native_access_bare_flag_does_not_consume_main_class() {
        let argv0: Vec<String> = argv(&["java", "--enable-native-access", "Main", "arg"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must parse bare native-access flag");

        assert_eq!(parsed.enable_native_access.as_deref(), Some("ALL-UNNAMED"));
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
        assert_eq!(parsed.args, argv(&["arg"]));
    }

    #[test]
    fn enable_native_access_equals_value_is_still_accepted() {
        let parsed = Args::try_parse_from(argv(&[
            "cratonvm",
            "--enable-native-access=java.base",
            "Main",
        ]))
        .expect("clap must parse native-access value with equals");

        assert_eq!(parsed.enable_native_access.as_deref(), Some("java.base"));
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    }

    #[test]
    fn hotspot_xss_inline_is_accepted_and_ignored() {
        // B3 regression: single-dash `-Xss<size>` (thread stack size —
        // Surefire/Gradle pass this routinely) was not in the handled set, so
        // it fell through to the final `else`, reached clap verbatim, and clap
        // rejected it as "unexpected argument '-X'". The catch-all `-X` arm now
        // accepts-and-ignores it (the whole token is dropped), same as `-XX:`.
        let raw = argv(&["java", "-Xss512k", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "Main"]));
    }

    #[test]
    fn hotspot_misc_x_flags_are_accepted_and_ignored() {
        // Other no-value `-X` knobs HotSpot accepts: `-Xint`, `-Xbatch`,
        // `-Xrs`, `-XshowSettings`, `-Xnoclassgc`. All drop out, leaving the
        // class name (and any later args) intact.
        let raw = argv(&["java", "-Xint", "-Xbatch", "-Xrs", "-Xnoclassgc", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "Main"]));
    }

    #[test]
    fn hotspot_server_flag_passes_clap_after_full_pipeline() {
        // WildFly HostController still launches child JVMs with `-server`.
        // HotSpot accepts it as a VM-selection hint; CratonVM ignores it but
        // must not let clap parse it as short flags (`-s -e ...`).
        let argv0: Vec<String> = argv(&["java", "-server", "-classpath", "x", "Main"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must accept HotSpot -server");
        assert_eq!(parsed.classpath.as_deref(), Some("x"));
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    }

    #[test]
    fn hotspot_xss_passes_clap_after_full_pipeline() {
        // B3 acceptance: stock `java -Xss512k -classpath x Main` must parse
        // end-to-end with `Main` resolved as the main class — previously clap
        // aborted on the unrecognized `-Xss512k`.
        let argv0: Vec<String> = argv(&["java", "-Xss512k", "-classpath", "x", "Main"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed =
            Args::try_parse_from(stage4).expect("clap must accept HotSpot -Xss after pipeline");
        assert_eq!(parsed.classpath.as_deref(), Some("x"));
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    }

    #[test]
    fn user_double_dash_reaches_program_args() {
        // B2 regression: a user `--` between program args
        // (`java Main a -- b`) must be delivered to the program verbatim as
        // `["a", "--", "b"]`. The launcher's own separator is consumed by clap
        // (it never reaches `parsed.args`), so the interior `--` here is purely
        // user data and must survive the whole pipeline.
        let argv0: Vec<String> = argv(&["java", "Main", "a", "--", "b"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must parse user `--` argv");
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
        // The interior user `--` is present in the trailing program args.
        assert_eq!(parsed.args, argv(&["a", "--", "b"]));
        // After [LOW arg-parse fix (1)] `run()` no longer pops any trailing
        // `--`; here the last arg is `b` anyway, so the program sees
        // ["a","--","b"] verbatim (the trailing-`--` case is covered by
        // `trailing_user_double_dash_is_preserved_through_pipeline`).
        assert_ne!(parsed.args.last().map(String::as_str), Some("--"));
    }

    #[test]
    fn classpath_class_and_program_args_pipeline() {
        // Regression guard for cli_main_args integration test.
        // `cratonvm --classpath <dir> PrintArgs alpha beta gamma` must produce
        // class_name=PrintArgs and args=["alpha","beta","gamma"].
        let argv0: Vec<String> = argv(&[
            "cratonvm",
            "--classpath",
            "/tmp/dir",
            "PrintArgs",
            "alpha",
            "beta",
            "gamma",
        ]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must parse classpath+args argv");
        assert_eq!(parsed.class_name.as_deref(), Some("PrintArgs"));
        assert_eq!(parsed.args, argv(&["alpha", "beta", "gamma"]));
    }

    #[test]
    fn launcher_trailing_double_dash_artifact_is_popped() {
        // B2 regression guard: `-mp` is listed in VALUE_TAKING_OPTS, so
        // `insert_program_args_separator` treats `/modules` as `-mp`'s value
        // and does NOT inject a spurious `"--"` between them.
        //
        // The pipeline result for `java -mp /modules`:
        //   normalize inserts `"--"` before `-mp` → clap sees ["java", "--", "-mp", "/modules"]
        //   class_name = Some("-mp")  (first positional after "--")
        //   args = ["/modules"]       (trailing_var_arg gets the rest)
        //
        // The old trailing `"--"` artifact (from when `/modules` was wrongly
        // treated as the main-class name) no longer occurs. The guard pop in
        // `run()` is a no-op.
        let argv0: Vec<String> = argv(&["java", "-mp", "/modules"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must parse `-mp` argv");
        // `-mp` is consumed as the class_name (first positional after `--`).
        assert_eq!(parsed.class_name.as_deref(), Some("-mp"));
        let mut prog = parsed.args;
        // No trailing "--" artifact — VALUE_TAKING_OPTS prevents the spurious injection.
        assert_ne!(prog.last().map(String::as_str), Some("--"));
        // The guard pop in `run()` is a no-op; args are already clean.
        if prog.last().map(String::as_str) == Some("--") {
            prog.pop();
        }
        // Only `/modules` remains in args; `-mp` went to class_name.
        assert_eq!(prog, argv(&["/modules"]));
    }

    // -----------------------------------------------------------------------
    // [LOW arg-parse fix (1)] Trailing user `--` is preserved as a program arg.
    // -----------------------------------------------------------------------

    #[test]
    fn trailing_user_double_dash_is_preserved_through_pipeline() {
        // `java Main a --` per JDK launcher semantics gives the program
        // `["a", "--"]`. The launcher's own boundary separator is consumed by
        // clap; the interior+trailing user `--` here is genuine program data
        // and the trailing one must NOT be popped (the old unconditional pop in
        // `run()` silently dropped it).
        let argv0: Vec<String> = argv(&["java", "Main", "a", "--"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must parse trailing `--` argv");
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
        // The trailing user `--` survives clap as the last program arg.
        assert_eq!(parsed.args.last().map(String::as_str), Some("--"));
        // Mirror `run()`'s post-clap handling: the pop is gone, so the trailing
        // `--` reaches the program verbatim.
        assert_eq!(parsed.args, argv(&["a", "--"]));
    }

    // -----------------------------------------------------------------------
    // [LOW arg-parse fix (2)] A value of a value-taking option that starts with
    // `-D` is NOT hijacked as a `-Dkey=value` system property.
    // -----------------------------------------------------------------------

    #[test]
    fn dminus_value_of_value_opt_is_not_a_system_property() {
        // `--Xlog -Dgc` — `-Dgc` is the VALUE of `--Xlog`, not a system
        // property. It must be passed through to clap (as `--Xlog`'s operand)
        // and must NOT appear in the extracted props list.
        let (filtered, props) =
            extract_system_properties(argv(&["java", "--Xlog", "-Dgc", "Main"]));
        assert_eq!(filtered, argv(&["java", "--Xlog", "-Dgc", "Main"]));
        assert!(
            props.is_empty(),
            "value token must not be parsed as -D prop"
        );
    }

    #[test]
    fn dminus_genuine_property_still_extracted_in_option_position() {
        // A genuine `-Dkey=value` in an OPTION position (not following a
        // value-taking option) is still extracted — the fix only protects the
        // value slot, it doesn't disable `-D` handling.
        let (filtered, props) =
            extract_system_properties(argv(&["java", "-Dfoo=bar", "--classpath", "x", "Main"]));
        // `-Dfoo=bar` removed; `--classpath x Main` survive (x is the value of
        // --classpath and is not a -D candidate anyway).
        assert_eq!(filtered, argv(&["java", "--classpath", "x", "Main"]));
        assert_eq!(props, vec![("foo".to_string(), "bar".to_string())]);
    }

    #[test]
    fn dminus_value_starting_with_dminus_for_classpath() {
        // Pathological but legal: a classpath entry literally starting with
        // `-D` (e.g. a directory named `-Dweird`). It is `--classpath`'s value
        // and must survive as-is, not become a fabricated system property.
        let (filtered, props) =
            extract_system_properties(argv(&["java", "--classpath", "-Dweird", "Main"]));
        assert_eq!(filtered, argv(&["java", "--classpath", "-Dweird", "Main"]));
        assert!(props.is_empty());
    }

    #[test]
    fn dminus_after_separator_is_program_arg_not_property() {
        // Unchanged behaviour guard: `-Dfoo=bar` after `--` belongs to the
        // program (jboss-modules style) and is never extracted.
        let (filtered, props) =
            extract_system_properties(argv(&["java", "Main", "--", "-Dfoo=bar"]));
        assert_eq!(filtered, argv(&["java", "Main", "--", "-Dfoo=bar"]));
        assert!(props.is_empty());
    }

    // -----------------------------------------------------------------------
    // [LOW arg-parse fix (3)] An unrecognized `-X` flag must not swallow the
    // following main-class token.
    // -----------------------------------------------------------------------

    #[test]
    fn unknown_x_flag_does_not_swallow_following_main_class() {
        // `-Xunknown Main`: the unknown `-X` flag is dropped (`i += 1`), and
        // the following bare token `Main` is NOT consumed as its value — it
        // survives so it can be resolved as the main class.
        let out = normalize_java_launcher_argv(argv(&["java", "-Xunknown", "Main"]));
        assert_eq!(out, argv(&["java", "Main"]));
    }

    #[test]
    fn unknown_x_flag_before_class_resolves_class_through_pipeline() {
        // End-to-end: an unknown separate-looking `-X` flag immediately before
        // the main class must still resolve `Main` (not absorb it). Exercises
        // the full pre-clap pipeline as `run()` would.
        let argv0: Vec<String> = argv(&["java", "-Xint", "-classpath", "x", "Main"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed =
            Args::try_parse_from(stage4).expect("clap must accept unknown -X before main class");
        assert_eq!(parsed.classpath.as_deref(), Some("x"));
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    }

    #[test]
    fn unknown_x_flag_directly_before_class_no_classpath() {
        // The minimal swallow scenario with no intervening options:
        // `java -Xint Main` must resolve `Main` as the main class.
        let argv0: Vec<String> = argv(&["java", "-Xint", "Main"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must accept `-Xint Main`");
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    }
}
