use anyhow::{bail, Context, Result};
use clap::Parser;
use rustjvm_vm::error::MethodCallFailed;
use rustjvm_vm::types::Value;
use rustjvm_vm::vm::{
    create_java_string, invoke_on_class_shared, invoke_on_class_shared_no_retarget, Vm,
};
use rustjvm_vm::{ClassPath, VmConfig};
use tracing::info;

// AUDIT 2026-05-16: extracted magic numbers into named constants.
/// Default watchdog timeout in seconds. Bumped from 45 → 120 to cover
/// WildFly bootstrap (clinit cascade through SimpleAttributeDef) on
/// cold-cache JDK 25.
const DEFAULT_WATCHDOG_SEC: u64 = 120;
/// Grace period in seconds after the watchdog fires before SIGKILL.
const WATCHDOG_GRACE_SEC: u64 = 3;
/// Default Java thread stack size in bytes (64 MiB).
const DEFAULT_JAVA_STACK_SIZE: usize = 64 * 1024 * 1024;
/// Maximum depth of `Throwable.getCause()` chain we render before
/// stopping (defensive against malicious or pathological cycles).
const MAX_CAUSE_CHAIN_DEPTH: usize = 8;

/// RustJVM — A Java Virtual Machine implemented in Rust.
///
/// Executes Java programs by loading and interpreting `.class` files.
///
/// Usage: rustjvm [OPTIONS] <CLASS_NAME> [ARGS]...
///        rustjvm [OPTIONS] --jar <FILE.jar> [ARGS]...
#[derive(Parser, Debug)]
#[command(name = "rustjvm", version, about)]
struct Args {
    /// The fully qualified class name to execute (e.g., com.example.Main).
    class_name: Option<String>,

    /// Execute a JAR file. The main class is read from META-INF/MANIFEST.MF.
    /// When -jar is used, the -cp/-classpath flag is ignored; the classpath
    /// comes from the JAR itself and its manifest Class-Path attribute.
    #[arg(long = "jar", value_name = "FILE")]
    jar: Option<String>,

    /// Classpath: directories and JAR files to search for classes.
    #[arg(short = 'c', long = "classpath", alias = "cp")]
    classpath: Option<String>,

    /// Maximum heap size (e.g., 256m, 1g).
    #[arg(long = "Xmx", value_name = "SIZE")]
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
    /// When omitted and JAVA_HOME is set, real JDK classes are loaded.
    #[arg(long = "synthetic-jdk")]
    synthetic_jdk: bool,

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
/// —...).  When a user passes a classpath entry that points at the aggregate JAR
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
fn expand_aggregate_jars(entries: Vec<String>) -> Vec<String> {
    // (aggregate_file_name, split_prefix) pairs.  The split prefix is
    // matched case-insensitively against sibling file names.
    const AGGREGATES: &[(&str, &str)] = &[("netty-all.jar", "netty-")];

    let mut out: Vec<String> = Vec::with_capacity(entries.len());
    for entry in entries {
        let path = std::path::Path::new(&entry);
        // If the entry already exists on disk (or is a directory), keep it.
        if path.exists() {
            out.push(entry);
            continue;
        }
        let file_name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_ascii_lowercase(),
            None => {
                out.push(entry);
                continue;
            }
        };
        let matched = AGGREGATES
            .iter()
            .find(|(name, _)| file_name == *name);
        let Some((_, split_prefix)) = matched else {
            out.push(entry);
            continue;
        };
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

/// Rewrite common HotSpot launcher spellings so clap can parse them.
///
/// Surefire / tooling often invokes `java -classpath —...` and `java -jar —...`.
/// Our clap schema uses `--classpath` / `--jar`; bare `-classpath` used to be
/// misparsed as `-c` with value `lasspath`, breaking Maven test runs.
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
            filtered.push(arg);
            continue;
        }
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

fn run() -> Result<()> {
    // Initialize tracing. B6: route WARN+ diagnostics to stderr so silent
    // swallow sites surface without polluting the program's stdout (which
    // Java's System.out also writes to).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::WARN.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    // Extract -Dkey=value system properties before clap parsing
    let raw_args: Vec<String> = normalize_java_launcher_argv(std::env::args().collect());
    let (filtered_args, system_properties) = extract_system_properties(raw_args);
    // T6 CLI compat: strip HotSpot-style flags before clap so their
    // non-standard spellings (`-XX:+Foo`, `-agentlib:`) don't confuse it.
    let (filtered_args, hotspot_flags) = extract_hotspot_flags(filtered_args);
    let mut args = Args::parse_from(filtered_args);

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
    args.args.retain(|a| a != "--");

    // Validate: exactly one of class_name or --jar must be provided
    if args.class_name.is_none() && args.jar.is_none() {
        bail!("No class name or --jar specified. Usage: rustjvm <class> or rustjvm --jar <file.jar>");
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

    // Resolve class name and classpath based on launch mode
    let (class_name, classpath) = if let Some(jar_path_str) = &args.jar {
        // -jar mode: read Main-Class from manifest, build classpath from JAR + manifest Class-Path
        let jar_path = std::path::Path::new(jar_path_str);
        if !jar_path.exists() {
            bail!("JAR file not found: {}", jar_path.display());
        }

        let manifest = ClassPath::read_jar_manifest(jar_path)
            .ok_or_else(|| anyhow::anyhow!("Cannot read manifest from {}", jar_path.display()))?;

        // Build classpath: JAR itself + manifest Class-Path entries
        let mut cp = vec![jar_path.to_string_lossy().into_owned()];
        cp.extend(manifest.resolve_class_path(jar_path));

        // KC26: For Quarkus applications, the RunnerClassLoader normally loads
        // classes from jars listed in quarkus-application.dat. Since we can't
        // fully emulate that complex bootstrap, add all application jars to
        // the VM classpath so ClassLoader.loadClass can find them.
        if let Some(parent) = jar_path.parent().and_then(|p| p.parent()) {
            let app_dirs = ["lib/lib/main", "lib/lib/boot", "lib/quarkus", "lib/app"];
            for dir_name in &app_dirs {
                let dir = parent.join(dir_name);
                if dir.is_dir() {
                    if let Ok(entries) = std::fs::read_dir(&dir) {
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

        let main_class = manifest
            .main_class
            .ok_or_else(|| anyhow::anyhow!("no main manifest attribute, in {}", jar_path.display()))?;

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
        match rustjvm_vm::config::XverifyMode::parse(spec) {
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

    let mut config = VmConfig::new()
        .with_classpath(classpath)
        .with_verbose_class_loading(args.verbose_class)
        .with_verbose_gc(args.verbose_gc)
        .with_skip_verification(args.noverify);

    if let Some(mode) = xverify_mode {
        config = config.with_xverify_mode(mode);
    }

    if let Some(bcp) = &args.boot_classpath {
        config = config.with_boot_classpath(VmConfig::parse_classpath(bcp));
    }
    if let Some(jh) = &args.java_home {
        config = config.with_java_home(jh.clone());
    }

    if let Some(max_heap_str) = &args.max_heap {
        let size = parse_size(max_heap_str)
            .with_context(|| format!("Invalid heap size: {max_heap_str}"))?;
        config = config.with_max_heap_size(size);
    }

    // CDS configuration
    if let Some(archive_path) = &args.shared_archive_file {
        config.shared_archive_file = Some(archive_path.clone());
    }
    config.cds_mode = match args.xshare.as_str() {
        "on" => rustjvm_vm::config::CdsMode::On,
        "auto" => rustjvm_vm::config::CdsMode::Auto,
        "dump" => rustjvm_vm::config::CdsMode::Dump,
        _ => rustjvm_vm::config::CdsMode::Off,
    };

    // Synthetic JDK mode: default to real JDK when a JDK is available.
    // Check explicit --java-home, JAVA_HOME env, or java on PATH.
    if args.synthetic_jdk {
        config.use_synthetic_jdk = true;
    } else if config.java_home.is_some() || args.java_home.is_some() {
        config.use_synthetic_jdk = false;
    } else {
        // Auto-detect: if a JDK is available via env or PATH, use real JDK
        if rustjvm_vm::config::resolve_java_home_public(None).is_some() {
            config.use_synthetic_jdk = false;
        }
    }

    // AOT configuration
    config.aot_mode = match args.aot_mode.as_str() {
        "training" => rustjvm_vm::config::AotMode::Training,
        "production" => rustjvm_vm::config::AotMode::Production,
        _ => rustjvm_vm::config::AotMode::Off,
    };
    if let Some(cache_path) = &args.aot_cache {
        // AOTCache serves as input in production mode and output in training mode
        match config.aot_mode {
            rustjvm_vm::config::AotMode::Production => {
                config.aot_cache_input = Some(cache_path.clone());
            }
            rustjvm_vm::config::AotMode::Training => {
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

    // Missing native audit. NEW-10: `--dump-missing-natives FILE` and
    // T2.1.3: `--dump-missing-natives-grouped FILE` both implicitly
    // enable audit mode so the user doesn't have to pass the flag
    // separately.
    config.audit_missing_natives = args.audit_missing_natives
        || args.dump_missing_natives.is_some()
        || args.dump_missing_natives_grouped.is_some();

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
            eprintln!("Warning: invalid --add-exports format: {s} (expected module/package=target)");
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
    let mut java_agents: Vec<rustjvm_vm::runtime::agent_loader::LoadedAgent> = Vec::new();
    if !hotspot_flags.agent_options.is_empty() {
        let mut native_agent_opts: Vec<String> = Vec::new();
        for opt in &hotspot_flags.agent_options {
            if opt.starts_with("-javaagent:") {
                match rustjvm_vm::runtime::agent_loader::parse_javaagent_spec(opt) {
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

    info!("Starting RustJVM");
    info!("Class: {class_name}");
    info!("Classpath: {:?}", config.classpath);

    // Create VM and execute main method
    let mut vm = Vm::new(config);

    // T19.H1 — optional watchdog that dumps every interpreter thread's
    // frame chain and aborts the process if the main method hasn't
    // completed within the configured deadline. Triggered by the
    // `--stack-dump-on-timeout=SECONDS` CLI flag.
    //
    // I1 — make hangs visible by default.
    //
    // When neither `--stack-dump-on-timeout` is supplied nor the
    // `RUSTJVM_DISABLE_DEFAULT_WATCHDOG` env var is set, install a
    // conservative 45-second default. This guarantees a hung VM emits
    // **something** to stderr before the surrounding harness kills the
    // process — the previous default of "no watchdog" produced empty
    // stderr + Windows TerminateProcess rc=-1 from the bench runners,
    // which made hangs (e.g. CGLIB's `String.indexOf` looping inside
    // `TypeUtils.parseSignature`) visually indistinguishable from a
    // segfault.
    //
    // 45 seconds is long enough for HelloWorld, the smoke tests in
    // tests/integration_test.rs, the wave2-* probes, and the JDK
    // bootstrap warm-up to complete, but short enough to fire before
    // every existing probe runner's 60s `TIMEOUT_SEC`. Long-running
    // services (Keycloak, Quarkus, WildFly) should pass an explicit
    // `--stack-dump-on-timeout=N` (with N suitably large) or set
    // `RUSTJVM_DISABLE_DEFAULT_WATCHDOG=1` — the same way they pass
    // explicit `-Xmx` instead of relying on heap defaults.
    // `RUSTJVM_STACK_DUMP_TIMEOUT` is a high-priority override that lets
    // an orchestrator force a short watchdog deadline (e.g. 20s) so the
    // ring + thread stacks get dumped before an outer test timeout
    // SIGKILLs the process. It wins over `RUSTJVM_DEFAULT_WATCHDOG_SEC`
    // but is still trumped by an explicit `--stack-dump-on-timeout`
    // CLI flag (the user's explicit intent always wins).
    let env_override_timeout = std::env::var("RUSTJVM_STACK_DUMP_TIMEOUT")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0);
    let effective_watchdog = match args.stack_dump_on_timeout {
        Some(s) if s > 0 => Some(s),
        Some(_) => None, // explicit `--stack-dump-on-timeout=0` disables
        None => {
            if std::env::var("RUSTJVM_DISABLE_DEFAULT_WATCHDOG").ok().as_deref()
                == Some("1")
            {
                None
            } else if let Some(s) = env_override_timeout {
                Some(s)
            } else {
                Some(
                    std::env::var("RUSTJVM_DEFAULT_WATCHDOG_SEC")
                        .ok()
                        .and_then(|s| s.parse::<u64>().ok())
                        // AUDIT 2026-05-16: validate the env-supplied
                        // watchdog into a sane range. Previously an
                        // unvalidated parse meant `RUSTJVM_DEFAULT_WATCHDOG_SEC=0`
                        // or huge integers could cripple the hang
                        // detector. Bound to [1, 86400] (1 day).
                        .filter(|&s| (1..=86_400).contains(&s))
                        // Bumped from 45 → 120: WildFly bootstrap was making
                        // forward progress through clinit cascade (64 distinct
                        // stack snapshots dumped in the 3s grace window) but
                        // 45s was insufficient for Module/AS/SimpleAttributeDef
                        // chain on cold-cache JDK. 120s matches typical CI
                        // budget for boot tests while still catching real
                        // hangs.
                        .unwrap_or(DEFAULT_WATCHDOG_SEC),
                )
            }
        }
    };
    if let Some(secs) = effective_watchdog {
        let shared_for_watchdog = std::sync::Arc::clone(&vm.shared);
        // RKC16N.5 — capture the audit-dump paths into the watchdog
        // thread so a hung run still produces a missing-natives
        // census. Without this, the only flush path is the
        // clean-shutdown branch at the end of `main()`, and every
        // watchdog-killed run loses the JSON we use for KC16/KC26
        // boot debugging.
        let watchdog_dump_path = args.dump_missing_natives.clone();
        let watchdog_dump_grouped_path = args.dump_missing_natives_grouped.clone();
        std::thread::Builder::new()
            .name("rustjvm-stack-watchdog".into())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(secs));
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

                // Always dump the dispatch_trace ring on watchdog fire,
                // even when `RUSTJVM_DBG_LETSGO` wasn't set: the header
                // alone confirms the dump path executed, and when the
                // env var *was* set the last few entries usually point
                // straight at the looping/hung method.
                rustjvm_vm::dispatch_trace::dump_to_stderr_unconditional(
                    "watchdog",
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
                    // KC-watchdog-native: dump the native-call ring
                    // buffer. The last entry with `STILL-IN-NATIVE`
                    // marks the hang site.
                    rustjvm_native_api::native_ring::dump_to_stderr();
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

                std::process::abort();
            })
            .context("failed to spawn stack-dump watchdog thread")?;
        eprintln!(
            "[rustjvm] stack-dump watchdog armed: will dump + abort after {secs}s"
        );
    }

    // Load the main class
    if let Err(e) = vm.load_class(&class_name) {
        bail!(
            "Could not find or load main class {}: {e}",
            class_name.replace('/', ".")
        );
    }

    // Build String[] args array for main(String[])
    let java_args: Vec<Value> = args
        .args
        .iter()
        .map(|a| Value::Object(Some(create_java_string(&vm.shared, a))))
        .collect();

    // Resolve the String[] class id for the args array.
    // ClassId(0) is java/lang/Object — the base reference array element type.
    let string_array_class_id = rustjvm_vm::ClassId::new(0);
    let args_array = vm.shared.heap.alloc_array(
        string_array_class_id,
        rustjvm_vm::memory::heap::ArrayElementType::Reference,
        java_args.len(),
    );
    for (i, val) in java_args.into_iter().enumerate() {
        vm.shared
            .heap
            .set_array_element(args_array, i, val)
            .map_err(|idx| anyhow::anyhow!("Failed to set args array element {i} (index {idx} out of bounds)"))?;
    }

    // T14: Run System.initPhase1() when booting from real JDK classes.
    // In HotSpot this is called from Threads::create_vm() after the
    // bootstrap classloader is initialized. It sets up system properties,
    // encodings, and the standard I/O streams (System.in/out/err).
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
                if let rustjvm_vm::error::MethodCallFailed::ExceptionThrown(exc_ref) = &e {
                    let exc_class_id = vm.shared.heap.class_id_of(*exc_ref);
                    let exc_class_name = vm.shared.class_manager.read()
                        .get_class(exc_class_id)
                        .map(|c| c.name.to_string())
                        .unwrap_or_else(|| format!("unknown({})", exc_class_id));
                    tracing::info!("System.initPhase1() fell back to synthetic streams ({exc_class_name})");
                } else {
                    tracing::info!("System.initPhase1() fell back to synthetic streams");
                    tracing::debug!("initPhase1 details: {e:?}");
                }
                // initPhase1 may have partially initialized classes and populated stale
                // invoke/resolution cache entries (e.g. real JDK PrintStream bytecode
                // cached for a synthetic 1-slot object). Clear both caches so the next
                // invocation re-resolves cleanly via the native-override registry.
                vm.main_thread.invoke_cache.clear();
                vm.shared.resolution_cache.write().clear();
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
        // rustjvm (the real-JDK module graph resolution pulls in
        // subsystems we don't implement), but many callers key on
        // `initLevel() >= 3` to decide whether
        // `ClassLoader.getSystemClassLoader()` may read the `scl`
        // field directly.  We leave the level at 2 here — bumping
        // past it would send those callers down a null-deref path.
        // The CLI bumps to 4 below, just before `main()`, once the
        // initPhase2 gate no longer matters.
    }

    // WP1.3: right before `main()` starts, advance to level 4 —
    // HotSpot's "VM fully initialized" state.  This is the signal
    // that `ClassLoader.getSystemClassLoader()` may return `scl` if
    // it is populated (in rustjvm it usually isn't, so callers fall
    // through to `getBuiltinAppClassLoader()` without harm), that
    // `Thread.currentThread().getName()` is safe, and that every
    // subsystem keyed on `VM.awaitInitLevel(4)` can proceed.  We
    // also briefly pass through level 3 so any probe between 2 and
    // 4 observes a level 3 transition.
    vm.shared.set_init_level(3);
    vm.shared.set_init_level(4);

    // WP2.4-C — run every `-javaagent:` agent's `premain(String,
    // Instrumentation)` hook BEFORE the application's `main`. Per the
    // `java.lang.instrument` package spec, agent failures are warnings
    // (logged inside the dispatcher) unless the agent throws a fatal
    // `Error`, in which case we abort here.
    if !java_agents.is_empty() {
        let res = rustjvm_vm::runtime::agent_loader::invoke_premains(
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
            let msg = panic.downcast_ref::<&str>().map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            bail!("main() panicked: {msg}");
        }
    };

    // [MAIN-RETURN] diagnostic — gate on RUSTJVM_DBG_MAIN_RETURN=1 so it
    // does not pollute clean runs. When set, prints whether main() returned
    // Ok / Err::Internal(msg) / Err::Exception, plus how long it ran and
    // how many non-daemon threads are still alive. Used to distinguish a
    // silent main-return (Spring's `ApplicationFailedEvent` consumed the
    // throwable) from an external kill (watchdog timeout, OS signal).
    if std::env::var_os("RUSTJVM_DBG_MAIN_RETURN").is_some() {
        let swallowed = vm
            .shared
            .swallow_counter
            .load(std::sync::atomic::Ordering::Relaxed);
        let kind = match &result {
            Ok(_) => "Ok".to_string(),
            Err(MethodCallFailed::InternalError(e)) => {
                format!("Err::Internal({e})")
            }
            Err(MethodCallFailed::ExceptionThrown(_)) => {
                "Err::Exception".to_string()
            }
        };
        let pending_nd = vm
            .shared
            .thread_registry
            .alive_non_daemon_thread_ids()
            .len();
        eprintln!(
            "[MAIN-RETURN] SpringApplication/main() returned in {:.2}s; \
             result={kind}; swallowed_vm_errors={swallowed}; \
             pending_non_daemon_threads={pending_nd}",
            main_elapsed.as_secs_f64()
        );
    }

    // NEW-10: before reporting the invocation result, write the
    // missing-natives audit log to the user-specified JSON path. We
    // do this unconditionally (regardless of Ok/Err) so a crashing
    // program still produces a census file. Any I/O error surfaces
    // as a warning — the primary invocation result takes precedence.
    if let Some(path) = &args.dump_missing_natives {
        match vm.shared.dump_missing_natives_json(path) {
            Ok(()) => {
                let count = vm.shared.get_missing_natives().len();
                eprintln!(
                    "[rustjvm] wrote {count} missing-native entries to {path}"
                );
            }
            Err(e) => {
                eprintln!(
                    "[rustjvm] warning: could not write missing-natives JSON to {path}: {e}"
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
                    "[rustjvm] wrote {total} missing-native entries across \
                     {} modules to {path}",
                    grouped.len()
                );
            }
            Err(e) => {
                eprintln!(
                    "[rustjvm] warning: could not write grouped missing-natives \
                     JSON to {path}: {e}"
                );
            }
        }
    }

    // B6: Silent-exit guard. If main() returned Ok but the VM has recorded
    // one or more swallowed errors during class init / invokedynamic / native
    // calls, surface a WARN to stderr so users (and CI) don't mistake a
    // silent exit for a successful run. Exit code stays 0 for compatibility
    // with programs that legitimately produce no stdout.
    if matches!(result, Ok(_)) {
        let swallowed = vm
            .shared
            .swallow_counter
            .load(std::sync::atomic::Ordering::Relaxed);
        if swallowed > 0 {
            eprintln!(
                "WARN: main() completed with {swallowed} swallowed VM error(s) \
                 (class-init / invokedynamic / native). Re-run with \
                 RUST_LOG=warn (already default) to see each site, or \
                 RUSTJVM_STRICT_SWALLOWS=1 to escalate the first swallow to a \
                 panic for diagnosis."
            );
        }
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
            .thread_registry
            .alive_non_daemon_thread_ids()
            .len();
        if pending > 0 {
            eprintln!(
                "[rustjvm] main() returned; VM held alive by {pending} \
                 non-daemon thread(s) (JVM-spec behaviour). Send SIGINT/\
                 SIGTERM, use System.exit(), or pass --stack-dump-on-timeout \
                 to bound execution."
            );
        }
        let joined = vm
            .shared
            .thread_registry
            .wait_for_non_daemon_threads(None);
        if joined > 0 {
            tracing::info!(
                "rustjvm: joined {joined} non-daemon thread(s) after main() returned"
            );
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
            // current rustjvm, `Throwable.fillInStackTrace` (see
            // `native-builtins/src/lang_misc.rs`) only stashes frames into the
            // per-thread `JvmThread::throwable_stacks` map keyed by identity
            // hash — it does NOT populate the heap-side `stackTrace` /
            // `backtrace` field. The Java code only writes that field lazily
            // when something calls `Throwable.getStackTrace()`. For unhandled
            // exceptions that escape `main()`, that has typically never
            // happened, so the renderer below will usually find a null array
            // and emit no `\tat ...` lines. Promoting the synthetic capture
            // to populate the heap field (or wiring this CLI to read from
            // `throwable_stacks` directly) is roadmap item T2.2.18 — see
            // `docs/roadmap-100.md` line 471.
            let mut cur = exc_ref;
            let mut lines: Vec<String> = Vec::new();
            let mut prefix = "Exception in thread \"main\"";
            for depth in 0..8 {
                let cid = vm.shared.heap.class_id_of(cur);
                let cname = vm.shared.class_manager.read()
                    .get_class(cid).map(|c| c.name.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                // Find fields by name so we work regardless of layout.
                // Also probe `target` (used by InvocationTargetException
                // in lieu of Throwable.cause — see its `getCause()` override)
                // so that `Caused by:` chains still walk through the wrapper.
                let (msg_idx, cause_idx, stack_idx, target_idx) = {
                    let cm = vm.shared.class_manager.read();
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
                        } else { break; }
                    }
                    (msg_i, cause_i, stack_i, target_i)
                };
                let message = if let Some(i) = msg_idx {
                    let v = vm.shared.heap.get_field(cur, i);
                    if let Value::Object(Some(s)) = v {
                        rustjvm_vm::vm::read_java_string(&vm.shared.heap, s).unwrap_or_default()
                    } else { String::new() }
                } else { String::new() };
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
                    let stack_val = vm.shared.heap.get_field(cur, si);
                    if let Value::Object(Some(arr)) = stack_val {
                        let len = vm.shared.heap.array_length(arr);
                        if len > 0 {
                            emitted_frames = true;
                            // Resolve StackTraceElement field indices once
                            // from the first non-null element's class.
                            let mut ste_idx: Option<(usize, usize, usize, usize)> = None;
                            for i in 0..len {
                                let elem = vm.shared.heap.get_array_element(arr, i)
                                    .ok()
                                    .and_then(|v| if let Value::Object(Some(o)) = v { Some(o) } else { None });
                                let Some(elem_ref) = elem else { continue };
                                if ste_idx.is_none() {
                                    let ecid = vm.shared.heap.class_id_of(elem_ref);
                                    let cm = vm.shared.class_manager.read();
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
                                                        "declaringClass" if dc.is_none() => dc = Some(abs),
                                                        "methodName" if mn.is_none() => mn = Some(abs),
                                                        "fileName" if fn_.is_none() => fn_ = Some(abs),
                                                        "lineNumber" if ln.is_none() => ln = Some(abs),
                                                        _ => {}
                                                    }
                                                    inst += 1;
                                                }
                                            }
                                            walk = cls.superclass;
                                        } else { break; }
                                    }
                                    if let (Some(a), Some(b), Some(c), Some(d)) = (dc, mn, fn_, ln) {
                                        ste_idx = Some((a, b, c, d));
                                    }
                                }
                                let Some((dc, mn, fn_, ln)) = ste_idx else { continue };
                                let read_str = |idx: usize| -> Option<String> {
                                    match vm.shared.heap.get_field(elem_ref, idx) {
                                        Value::Object(Some(s)) => {
                                            rustjvm_vm::vm::read_java_string(&vm.shared.heap, s)
                                        }
                                        _ => None,
                                    }
                                };
                                let class_name = read_str(dc).unwrap_or_else(|| "<unknown>".to_string());
                                let method_name = read_str(mn).unwrap_or_else(|| "<unknown>".to_string());
                                let file_name = read_str(fn_);
                                let line_no = match vm.shared.heap.get_field(elem_ref, ln) {
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
                // `getStackTrace()`), pull frames from the per-thread
                // `JvmThread::throwable_stacks` map keyed by identity hash
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
                        if let Value::Object(Some(c)) = vm.shared.heap.get_field(cur, i) {
                            if c != cur { next = Some(c); }
                        }
                    }
                    if next.is_none() {
                        if let Some(i) = target_idx {
                            if let Value::Object(Some(t)) = vm.shared.heap.get_field(cur, i) {
                                if t != cur { next = Some(t); }
                            }
                        }
                    }
                    next
                };
                if next_cause.is_none() && cname == "java/lang/reflect/InvocationTargetException" {
                    let ite_decl = vm
                        .shared
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
                                    "[rustjvm-cli] InvocationTargetException.{meth}() failed: {e:?}"
                                ));
                            }
                        }
                    }
                }
                if let Some(c) = next_cause {
                    cur = c;
                    prefix = "Caused by:";
                    continue;
                }
                let _ = depth;
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
                            "[rustjvm-cli] Throwable stack (fillInStackTrace) for InvocationTargetException:"
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
            bail!("{}", lines.join("\n"));
        }
    }
}

// ---------------------------------------------------------------------------
// letsgo postmortem SEGV trap — Windows SEH unhandled-exception filter.
//
// When `RUSTJVM_DBG_LETSGO=1` is set, the VM's `dispatch_trace` ring
// buffer records every bytecode-method entry and every native dispatch.
// A SIGSEGV / STATUS_ACCESS_VIOLATION on Windows skips the Rust panic
// path entirely, so we install a low-level SEH filter that dumps the
// ring to stderr before the OS terminates the process with rc=139.
//
// Mirrors the pattern in `rustjvm-vm/src/lib.rs::harness_exit_shim` but
// targets release-build crash diagnosis rather than test teardown.
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod letsgo_segv_trap {
    use std::sync::atomic::{AtomicBool, Ordering};

    #[repr(C)]
    struct ExceptionRecord {
        exception_code: u32,
        exception_flags: u32,
        exception_record: *mut ExceptionRecord,
        exception_address: *mut core::ffi::c_void,
        number_parameters: u32,
        exception_information: [usize; 15],
    }

    #[repr(C)]
    struct ExceptionPointers {
        exception_record: *mut ExceptionRecord,
        context_record: *mut core::ffi::c_void,
    }

    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
    const STATUS_ACCESS_VIOLATION: u32 = 0xC0000005;
    const STATUS_ILLEGAL_INSTRUCTION: u32 = 0xC000001D;
    const STATUS_PRIVILEGED_INSTRUCTION: u32 = 0xC0000096;
    const STATUS_STACK_OVERFLOW: u32 = 0xC00000FD;
    const STATUS_INTEGER_DIVIDE_BY_ZERO: u32 = 0xC0000094;

    static DUMPED: AtomicBool = AtomicBool::new(false);

    #[link(name = "kernel32")]
    extern "system" {
        fn SetUnhandledExceptionFilter(
            filter: Option<unsafe extern "system" fn(*mut ExceptionPointers) -> i32>,
        ) -> Option<unsafe extern "system" fn(*mut ExceptionPointers) -> i32>;
    }

    unsafe extern "system" fn handler(info: *mut ExceptionPointers) -> i32 {
        if DUMPED.swap(true, Ordering::SeqCst) {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        use std::io::Write;
        let mut stderr = std::io::stderr().lock();
        if !info.is_null() {
            let rec = (*info).exception_record;
            if !rec.is_null() {
                let _ = writeln!(
                    stderr,
                    "===== letsgo SEH trap fired: code=0x{:08X} addr={:p} =====",
                    (*rec).exception_code,
                    (*rec).exception_address,
                );
                let code = (*rec).exception_code;
                let name = match code {
                    STATUS_ACCESS_VIOLATION => "STATUS_ACCESS_VIOLATION",
                    STATUS_ILLEGAL_INSTRUCTION => "STATUS_ILLEGAL_INSTRUCTION",
                    STATUS_PRIVILEGED_INSTRUCTION => "STATUS_PRIVILEGED_INSTRUCTION",
                    STATUS_STACK_OVERFLOW => "STATUS_STACK_OVERFLOW",
                    STATUS_INTEGER_DIVIDE_BY_ZERO => "STATUS_INTEGER_DIVIDE_BY_ZERO",
                    _ => "<other>",
                };
                let _ = writeln!(stderr, "exception name: {name}");
                if code == STATUS_ACCESS_VIOLATION && (*rec).number_parameters >= 2 {
                    let op = (*rec).exception_information[0];
                    let va = (*rec).exception_information[1];
                    let op_str = match op {
                        0 => "read",
                        1 => "write",
                        8 => "DEP/NX",
                        _ => "?",
                    };
                    let _ = writeln!(
                        stderr,
                        "access-violation op={op} ({op_str}) faulting_va=0x{va:016X}",
                    );
                }
            }
        }
        let _ = stderr.flush();
        rustjvm_vm::dispatch_trace::dump_to_stderr("SEH");
        EXCEPTION_CONTINUE_SEARCH
    }

    pub fn install() {
        unsafe {
            SetUnhandledExceptionFilter(Some(handler));
        }
    }
}

#[cfg(not(windows))]
mod letsgo_segv_trap {
    pub fn install() {}
}

fn main() {
    // AUDIT 2026-05-16: --version / --help fast path. The previous flow
    // installed a 64 MB-stack worker thread, initialized
    // tracing-subscriber, ran the panic hook, and the three preparse
    // rewrites before clap could detect `--version`. For HotSpot-style
    // single-flag probes (which CI scripts hammer to detect the JVM),
    // that's wasteful. Peek argv before any of that work — and before
    // installing the dispatch ring / SEH trap below, since version probes
    // by definition do not need crash forensics.
    {
        let mut args = std::env::args().skip(1);
        if let Some(first) = args.next() {
            match first.as_str() {
                "--version" | "-V" | "-version" => {
                    println!(
                        "rustjvm {} (CratonVM)",
                        env!("CARGO_PKG_VERSION"),
                    );
                    println!("openjdk version \"25\"");
                    println!("OpenJDK Runtime Environment RustJVM (build 25+CratonVM)");
                    println!("RustJVM VM (build 25+CratonVM, mixed mode)");
                    return;
                }
                _ => {}
            }
        }
    }

    // letsgo postmortem — initialize the dispatch ring + SEH trap *before*
    // any VM startup so even crashes during init are captured.
    rustjvm_vm::dispatch_trace::init_from_env();
    letsgo_segv_trap::install();

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
        let msg = info.payload().downcast_ref::<&str>().map(|s| s.to_string())
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
        let in_bootstrap = rustjvm_native_api::init_level::get_init_level() < 4;
        let is_known_bootstrap_quiet = (msg.contains("unaligned pointer")
            || msg.contains("null pointer"))
            && in_bootstrap;

        if is_known_bootstrap_quiet {
            if let Some(loc) = info.location() {
                tracing::debug!(
                    target: "rustjvm::panic",
                    file = loc.file(),
                    line = loc.line(),
                    column = loc.column(),
                    thread = thread_name,
                    "bootstrap-path panic (caught upstream): {msg}",
                );
            } else {
                tracing::debug!(
                    target: "rustjvm::panic",
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
                loc.file(), loc.line(), loc.column(),
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
                target: "rustjvm::panic",
                file = loc.file(),
                line = loc.line(),
                column = loc.column(),
                thread = thread_name,
                "panic: {msg}",
            );
        } else {
            tracing::warn!(target: "rustjvm::panic", thread = thread_name, "panic: {msg}");
        }
    }));

    // The interpreter uses recursive Rust calls for Java method invocations.
    // Deep Java call stacks (e.g. Quarkus bootstrap) can exceed the default
    // 8 MB Rust stack.  Spawn the real entry point on a thread with 64 MB.
    let builder = std::thread::Builder::new()
        .name("main-vm".into())
        .stack_size(64 * 1024 * 1024);
    let handler = builder.spawn(|| {
        if let Err(e) = run() {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
    }).expect("failed to spawn main-vm thread");
    handler.join().unwrap_or_else(|e| {
        eprintln!("main-vm thread panicked: {:?}", e);
        // letsgo postmortem — surface the dispatch trail on a panic-join,
        // mirroring the SEH path. No-op when RUSTJVM_DBG_LETSGO is unset.
        rustjvm_vm::dispatch_trace::dump_to_stderr("panic-join");
        std::process::exit(1);
    });
    // Successful exit path: also dump the trail if the flag was set so
    // we can compare the last N dispatches against the expected normal
    // shutdown sequence (only printed when the flag is active).
    if rustjvm_vm::dispatch_trace::is_enabled() {
        rustjvm_vm::dispatch_trace::dump_to_stderr("normal-exit");
    }
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

    num_str.parse::<usize>().ok().map(|n| n * multiplier)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "rustjvm".to_string(),
            "-Djboss.home.dir=C:/craton/kc16".to_string(),
            "-Dmy.flag".to_string(),
            "com.example.Main".to_string(),
        ];
        let (filtered, props) = extract_system_properties(raw);
        assert_eq!(filtered, vec!["rustjvm", "com.example.Main"]);
        assert_eq!(props, vec![
            ("jboss.home.dir".to_string(), "C:/craton/kc16".to_string()),
            ("my.flag".to_string(), String::new()),
        ]);
    }

    #[test]
    fn extract_d_no_properties() {
        let raw = vec!["rustjvm".to_string(), "Main".to_string()];
        let (filtered, props) = extract_system_properties(raw);
        assert_eq!(filtered, vec!["rustjvm", "Main"]);
        assert!(props.is_empty());
    }

    #[test]
    fn extract_d_value_with_equals() {
        // -Dkey=val=ue  —  key = "val=ue"
        let raw = vec!["rustjvm".to_string(), "-Dpath=a=b".to_string()];
        let (_, props) = extract_system_properties(raw);
        assert_eq!(props, vec![("path".to_string(), "a=b".to_string())]);
    }

    #[test]
    fn normalize_classpath_and_jar_for_clap() {
        let raw = vec![
            "rustjvm".to_string(),
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
                "rustjvm",
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
            "rustjvm".to_string(),
            "-XX:+HeapDumpOnOutOfMemoryError".to_string(),
            "Main".to_string(),
        ];
        let (filtered, flags) = extract_hotspot_flags(raw);
        assert_eq!(filtered, vec!["rustjvm", "Main"]);
        assert_eq!(flags.heap_dump_on_oom, Some(true));
    }

    #[test]
    fn hotspot_flag_disables_heap_dump_on_oom() {
        let raw = vec![
            "rustjvm".to_string(),
            "-XX:-HeapDumpOnOutOfMemoryError".to_string(),
            "Main".to_string(),
        ];
        let (_, flags) = extract_hotspot_flags(raw);
        assert_eq!(flags.heap_dump_on_oom, Some(false));
    }

    #[test]
    fn hotspot_flag_extracts_heap_dump_path() {
        let raw = vec![
            "rustjvm".to_string(),
            "-XX:HeapDumpPath=/tmp/heap.hprof".to_string(),
            "Main".to_string(),
        ];
        let (filtered, flags) = extract_hotspot_flags(raw);
        assert_eq!(filtered, vec!["rustjvm", "Main"]);
        assert_eq!(flags.heap_dump_path.as_deref(), Some("/tmp/heap.hprof"));
    }

    #[test]
    fn hotspot_flag_preserves_all_agent_tokens() {
        let raw = vec![
            "rustjvm".to_string(),
            "-agentlib:jdwp=transport=dt_socket,server=y,address=5005".to_string(),
            "-agentpath:/opt/myagent.so=trace".to_string(),
            "-javaagent:/opt/bytebuddy.jar".to_string(),
            "Main".to_string(),
        ];
        let (filtered, flags) = extract_hotspot_flags(raw);
        assert_eq!(filtered, vec!["rustjvm", "Main"]);
        assert_eq!(flags.agent_options.len(), 3);
        assert!(flags.agent_options[0].starts_with("-agentlib:jdwp="));
        assert!(flags.agent_options[1].starts_with("-agentpath:/opt/myagent.so"));
        assert!(flags.agent_options[2].starts_with("-javaagent:/opt/bytebuddy.jar"));
    }

    #[test]
    fn hotspot_flag_passes_unknown_through() {
        let raw = vec![
            "rustjvm".to_string(),
            "--foo".to_string(),
            "Main".to_string(),
        ];
        let (filtered, flags) = extract_hotspot_flags(raw);
        assert_eq!(filtered, vec!["rustjvm", "--foo", "Main"]);
        assert!(flags.agent_options.is_empty());
        assert!(flags.heap_dump_on_oom.is_none());
        assert!(flags.heap_dump_path.is_none());
    }

    // -----------------------------------------------------------------------
    // expand_aggregate_jars tests
    // -----------------------------------------------------------------------

    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("rustjvm-cli-{label}-{pid}-{id}"));
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
        assert!(has("netty-common.jar"), "missing netty-common: {expanded:?}");
        assert!(has("netty-transport.jar"), "missing netty-transport: {expanded:?}");
        assert!(!has("other.jar"), "unexpected 'other.jar' in result: {expanded:?}");
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
        let missing = dir.join("does-not-exist.jar").to_string_lossy().into_owned();
        let expanded = expand_aggregate_jars(vec![missing.clone()]);
        assert_eq!(expanded, vec![missing]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
