#[allow(unused_imports)]
use super::super::*;

/// The VM's own main-class check (`CRATONVM_LAUNCHER_HELPER` off, or a
/// helper the VM could not run): the outcome of the main class's load, the
/// launcher lookup's initiating records and listing, and
/// `cratonvm_vm::vm::validate_main_method`. Every failure prints the
/// launcher's text and exits (or, with no JDK to ask, returns the VM's own
/// load error).
pub(crate) fn launcher_check_main_natively(
    args: &Args,
    vm: &mut Vm,
    class_name: &str,
    main_class_load: Result<cratonvm_vm::ClassId, cratonvm_vm::error::VmError>,
    jdk_only_watermark: &mut ViolationWatermark,
) -> Result<cratonvm_vm::vm::LauncherMain> {
    if let Err(e) = main_class_load {
        // Over a real JDK, HotSpot's text: `LauncherHelper.loadMainClass`'s
        // `java.launcher.cls.error1` / `error4`, from the throwable the system
        // loader's `Class.forName` raises for the name (interpreter round i2
        // wave 59, lane L5; probes `L5W59LauncherNoMain`'s second launch and
        // `L5W59LauncherMissingSuper`).
        let launcher_text = vm.launcher_load_failure_text(class_name);
        // Real-JDK: this is the likeliest strict-mode failure — the main class
        // (or something it needs) was refused rather than fabricated. The
        // census must survive it, or `difftest` cannot categorise the failure
        // it was launched to produce.
        finish_jdk_only(args, &vm.shared, jdk_only_watermark);
        if let Some(text) = launcher_text {
            exit_with_launcher_error(&text);
        }
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
    // HotSpot's launcher finds `main` by reflection, which resolves the main
    // class's public method signatures through its loader; the VM records what
    // that lookup initiates (`Instrumentation.getInitiatedClasses`; one relaxed
    // load while no record is armed; interpreter round i1 wave 47, lane L5).
    vm.note_launcher_main_lookup(class_name);
    // ...and that lookup's listing fails the launch when a listed signature
    // type cannot be loaded: HotSpot's `LauncherHelper.validateMainMethod`
    // turns any `Throwable` of `MethodFinder.findMainMethod` into
    // `java.launcher.cls.error5` and exit 1. Under
    // `CRATONVM_REFLECTION_LOADS_SIGNATURE_TYPES` only (the listings resolve
    // the types with it); interpreter round i2 wave 58, lane L5 (probe
    // `tools/probes/interp/L5/L5W57LauncherSignatureType.java`).
    // A main class in a named module gets `java.launcher.module.error3`'s
    // text instead (2026-10-07; the VM builds either).
    if let Some(text) = vm.launcher_main_listing_failure(class_name) {
        finish_jdk_only(args, &vm.shared, jdk_only_watermark);
        exit_with_launcher_error(&text);
    }
    // ...and `LauncherHelper.validateMainMethod`: JDK 25's `main` search
    // (`MethodFinder.findMainMethod`: `main(String[])`, then `main()`, static
    // or instance, never private) and its checks on an instance `main`'s
    // class, each failure a `java.launcher.cls.error*` text and exit 1. The
    // answer says how `main` is called (interpreter round i2 wave 59, lane
    // L5; `cratonvm_vm::vm::validate_main_method`, probes
    // `tools/probes/interp/L5/L5W59Launcher*.java`).
    match vm.launcher_check_main(class_name) {
        Ok(found) => Ok(found),
        Err(text) => {
            finish_jdk_only(args, &vm.shared, jdk_only_watermark);
            exit_with_launcher_error(&text);
        }
    }
}

/// The launcher's main-class failure: `text` (a `java.launcher.*` message of
/// `LauncherHelper`, which prints it to stderr) and exit 1, as
/// `LauncherHelper.abort` does before `main` (interpreter round i2 wave 59,
/// lane L5). Both streams are flushed first; the mode stamp the error arm of
/// `main` prints follows the text (triage reads it; HotSpot's text is
/// unchanged above it).
pub(crate) fn exit_with_launcher_error(text: &str) -> ! {
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    eprintln!("{text}");
    // nio21 wave 8 lane L4: HotSpot prints the text alone; the mode stamp is
    // CratonVM triage and moved behind `CRATONVM_DBG=launcher-outcome`.
    if launcher_outcome_lines_on() {
        eprintln!("[cratonvm] {}", active_jdk_mode_line());
    }
    let _ = std::io::stderr().flush();
    destroy_java_vm_before_exit();
    std::process::exit(1);
}

/// What HotSpot's `DestroyJavaVM` does for a launcher that leaves through it
/// with an error (`LEAVE()` in `java.c`: a main class that does not load,
/// a launcher error after the VM exists, an uncaught `bail!`), for an exit
/// that does not pass the `run()` exit arms (interpreter round i4 wave 84,
/// lane L1): `before_exit` posts JVMTI `VMDeath`, so an attached debugger
/// gets its `VM_DEATH` event set (`cratonvm_vm::report_debugger_vm_death`;
/// a no-op without one, and after the first report), and `perfMemory_exit`
/// unlinks the perf-data file. Nothing when no VM exists yet.
pub(crate) fn destroy_java_vm_before_exit() {
    if let Some(shared) = cratonvm_vm::native::jni::process_vm_strict() {
        cratonvm_vm::report_debugger_vm_death(&shared);
        cratonvm_vm::remove_perf_data_file_at_exit(&shared);
    }
}

/// Unlink this process's jvmstat perf-data file, if a VM wrote one, before an
/// abort (interpreter round i4 wave 84, lane L1): HotSpot's `os::abort` runs
/// `os::shutdown` -> `perfMemory_exit` but posts no `VMDeath`. The error
/// exits use [`destroy_java_vm_before_exit`]. Without this
/// `/tmp/hsperfdata_<user>/<pid>` outlived the process (`jps` showed
/// `<pid> -- process information unavailable` until the next VM's sweep).
/// Idempotent; nothing when no VM exists yet.
pub(crate) fn remove_perf_data_file_before_exit() {
    if let Some(shared) = cratonvm_vm::native::jni::process_vm_strict() {
        cratonvm_vm::remove_perf_data_file_at_exit(&shared);
    }
}

/// End the process as JPLIS's `abortJVM` does after a startup agent's start
/// failed (interpreter round i4 wave 84, lane L1;
/// `docs/feature-designs/real-jdk-mode.md`):
/// JNI `FatalError` prints its line on `tty` (stdout) and calls
/// `os::abort`, which runs `os::shutdown` (`perfMemory_exit` unlinks the
/// perf-data file) and then, on Linux and macOS, `::abort()` -- `SIGABRT`,
/// exit status 134 in a shell, a core file where the limits allow one -- and
/// on Windows `ExitProcess(1)` (no minidump: `FatalError` opens no dump
/// file). HotSpot also prints the thread's native frames under the
/// `FATAL ERROR` line (`JavaThread::print_jni_stack`); those addresses are
/// not reproduced. The VM's own crash handler is taken off `SIGABRT` first,
/// so this abort writes no `hs_err` report: HotSpot installs none for it.
pub(crate) fn abort_as_jplis_abort_jvm() -> ! {
    use std::io::Write as _;
    let _ = std::io::stderr().flush();
    {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(
            out,
            "{}",
            cratonvm_vm::runtime::instrument::JPLIS_PREMAIN_FATAL_ERROR
        );
        let _ = out.flush();
    }
    remove_perf_data_file_before_exit();
    #[cfg(unix)]
    {
        cratonvm_vm::abort_without_crash_report();
    }
    #[cfg(not(unix))]
    {
        std::process::exit(1);
    }
}

/// The option string of a `-agentlib:jdwp[=<options>]` argument: `Some(None)`
/// for none, `None` for any other argument (interpreter round i4 wave 85,
/// lane L1).
pub(crate) fn jdwp_agent_options_of(arg: &str) -> Option<Option<&str>> {
    let rest = arg.strip_prefix("-agentlib:jdwp")?;
    if rest.is_empty() {
        return Some(None);
    }
    rest.strip_prefix('=').map(Some)
}

/// What [`parse_jdwp_agent_options`] made of the options.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum JdwpAgentParse {
    Config(cratonvm_vm::config::JdwpAgentConfig),
    /// `-agentlib:jdwp=help`: the usage on stdout, exit status 0.
    Help,
}

/// The back end's usage text (`printUsage`, abbreviated: the option table
/// HotSpot prints, without its obsolete-option and example sections).
pub(crate) const JDWP_HELP: &str = "               Java Debugger JDWP Agent Library
               --------------------------------

  (See the \"VM Invocation Options\" section of the JPDA
   \"Connection and Invocation Details\" document for more information.)

jdwp usage: java -agentlib:jdwp=[help]|[<option>=<value>, ...]

Option Name and Value            Description                       Default
---------------------            -----------                       -------
suspend=y|n                      wait on startup?                  y
transport=<name>                 transport spec                    none
address=<listen/attach address>  transport spec                    \"\"
server=y|n                       listen for debugger?              n
launch=<command line>            run debugger on event             none
onthrow=<exception name>         debug on throw                    none
onuncaught=y|n                   debug on any uncaught?            n
timeout=<timeout value>          for listen/attach in milliseconds n
mutf8=y|n                        output modified utf-8             n
quiet=y|n                        control over terminal messages    n";

/// `-agentlib:jdwp`'s options as the JDK back end's `debugInit.c`
/// `parseOptions` reads them (interpreter round i4 wave 85, lane L1): `name=`
/// then a value up to the next `,` (`get_tok`); a boolean is exactly `y` or
/// `n` (`get_boolean`); an unknown name or a malformed value is
/// `ERROR: JDWP option syntax error: -agentlib:jdwp=<options>`; `address`
/// or `allow` before any `transport` is `address specified without
/// transport`; no `transport` at all is `no transport specified`;
/// `server=n` without an address is the `Non-server transport` line and
/// `invalid option`; `onthrow` / `onuncaught=y` without `launch` is the
/// `Specify launch=` line. All of these exit 1 (`forceExit(1)`). A transport
/// other than `dt_socket` fails to load (exit 2), and an `address` whose
/// port does not parse fails the socket transport's init (exit 2), as they
/// do on HotSpot a moment later, at `VMInit`. `launch` / `onthrow` /
/// `onuncaught` (debugger launched on an event) are refused: the built-in
/// server cannot start a debugger.
pub(crate) fn parse_jdwp_agent_options(
    options: Option<&str>,
) -> Result<JdwpAgentParse, cratonvm_vm::config::JdwpAgentFailure> {
    use cratonvm_vm::config::{JdwpAgentConfig, JdwpAgentFailure};
    const AGENTLIB: &str = "-agentlib:jdwp";
    let options = options.unwrap_or("");
    if options == "help" {
        return Ok(JdwpAgentParse::Help);
    }
    let syntax = || {
        JdwpAgentFailure::option(vec![format!(
            "ERROR: JDWP option syntax error: {AGENTLIB}={options}"
        )])
    };
    let bad = |message: &str| {
        JdwpAgentFailure::option(vec![format!(
            "ERROR: JDWP {message}: {AGENTLIB}={options}"
        )])
    };
    fn token(source: &str, separator: char) -> (&str, &str) {
        match source.find(separator) {
            Some(at) => (&source[..at], &source[at + 1..]),
            None => (source, ""),
        }
    }
    let boolean = |value: &str| match value {
        "y" => Some(true),
        "n" => Some(false),
        _ => None,
    };
    let mut transport: Option<&str> = None;
    let mut address: Option<&str> = None;
    let mut server = false;
    let mut suspend = true;
    let mut quiet = false;
    let mut timeout: Option<&str> = None;
    let mut launch: Option<&str> = None;
    let mut on_throw: Option<&str> = None;
    let mut on_uncaught = false;
    let mut include_virtual_threads = false;
    let mut allow: Option<&str> = None;
    let mut rest = options;
    while !rest.is_empty() {
        let (name, after_name) = token(rest, '=');
        let (value, after_value) = token(after_name, ',');
        rest = after_value;
        match name {
            "transport" => transport = Some(value),
            "address" | "allow" if transport.is_none() => {
                return Err(bad(&format!("{name} specified without transport")));
            }
            "address" => address = Some(value),
            // Interpreter round i7 wave 140, lane L1: kept (it was dropped).
            // An empty list is the back end's syntax error (MEASURED,
            // HotSpot 25.0.3: `allow=` -> `JDWP option syntax error`).
            "allow" if value.is_empty() => return Err(syntax()),
            "allow" => allow = Some(value),
            "timeout" => timeout = Some(value),
            // Interpreter round i7 wave 144, lane L1: kept (the back end's
            // `get_tok` refuses an empty value as a syntax error, as for
            // `allow=`; PREDICTED).
            "launch" | "onthrow" if value.is_empty() => return Err(syntax()),
            "launch" => launch = Some(value),
            "onthrow" => on_throw = Some(value),
            "logfile" | "logflags" => {}
            "server" | "suspend" | "quiet" | "onuncaught" | "mutf8"
            | "includevirtualthreads" => {
                let Some(flag) = boolean(value) else {
                    return Err(syntax());
                };
                match name {
                    "server" => server = flag,
                    "suspend" => suspend = flag,
                    "quiet" => quiet = flag,
                    "onuncaught" => on_uncaught = flag,
                    // Interpreter round i6 wave 130 (lane L1b): kept (it was
                    // read and dropped), for `AllThreads`.
                    "includevirtualthreads" => include_virtual_threads = flag,
                    _ => {}
                }
            }
            _ => return Err(syntax()),
        }
    }
    let Some(transport) = transport else {
        return Err(bad("no transport specified"));
    };
    if !server && address.is_none() {
        return Err(JdwpAgentFailure::option(vec![
            format!(
                "ERROR: JDWP Non-server transport {transport} must have a connection address specified through the 'address=' option"
            ),
            format!("ERROR: JDWP invalid option: {AGENTLIB}={options}"),
        ]));
    }
    if (on_throw.is_some() || on_uncaught) && launch.is_none() {
        return Err(bad(
            "Specify launch=<command line> when using onthrow or onuncaught suboption",
        ));
    }
    // Interpreter round i7 wave 144, lane L1 (`CRATONVM_JDWP_AGENT_LAUNCH`,
    // parity, default ON): served (`debug::launch_jdwp_debugger`,
    // `debug::start_deferred_jdwp_agent`). `=0` refuses them as before.
    if (launch.is_some() || on_throw.is_some() || on_uncaught)
        && false
    {
        return Err(bad(
            "launch, onthrow and onuncaught are not supported by the CratonVM JDWP server",
        ));
    }
    if transport != "dt_socket" {
        return Err(JdwpAgentFailure::transport_load(transport));
    }
    // `[host:]port`, the host up to the LAST `:` (an IPv6 literal keeps its
    // colons); no address listens on an ephemeral port.
    let (host, port) = match address.filter(|a| !a.is_empty()) {
        None => (None, "0"),
        Some(address) => match address.rfind(':') {
            Some(at) => (Some(&address[..at]), &address[at + 1..]),
            None => (None, address),
        },
    };
    let Ok(port) = port.parse::<u16>() else {
        return Err(JdwpAgentFailure::transport_init_with(
            103,
            "invalid port number specified",
        ));
    };
    Ok(JdwpAgentParse::Config(JdwpAgentConfig {
        server,
        suspend,
        host: host.map(str::to_string),
        port,
        quiet,
        timeout_ms: timeout.and_then(|t| t.parse::<u64>().ok()),
        include_virtual_threads,
        // Interpreter round i7 wave 140, lane L1: kept for the transport,
        // which parses and enforces it (`debug::parse_jdwp_allow`).
        allow: allow.map(str::to_string),
        // Wave 144 (L1): served by the server (`debug::start_jdwp_agent`).
        launch: launch.map(str::to_string),
        on_throw: on_throw.map(str::to_string),
        on_uncaught,
    }))
}

/// End the process as the JDK back end's `forceExit` does after `failure`
/// (interpreter round i4 wave 85, lane L1): its lines on stderr, its exit
/// status, nothing on stdout. The perf-data file goes with it.
pub(crate) fn exit_with_jdwp_agent_failure(failure: &cratonvm_vm::config::JdwpAgentFailure) -> ! {
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    {
        let mut err = std::io::stderr().lock();
        for line in &failure.stderr_lines {
            let _ = writeln!(err, "{line}");
        }
        let _ = err.flush();
    }
    remove_perf_data_file_before_exit();
    std::process::exit(failure.exit_code);
}

/// zs s2/ob (`zgc-zr3mt-zero-gc-thread-flags-warn-instead-of-refusing-FIXED-20261019.md`):
/// HotSpot 25's `ZArguments::initialize` refuses an explicit `-XX:+UseZGC`
/// with `-XX:ParallelGCThreads=0` (checked first) or `-XX:ConcGCThreads=0`:
/// `vm_exit_during_initialization`, i.e. `Error occurred during
/// initialization of VM` and `The flag -XX:+UseZGC can not be combined with
/// -XX:<name>=0` on stdout, exit 1 (local 25.0.3; `=00` is refused too, a
/// non-number is HotSpot's `Improperly specified VM option`: refused on ZGC
/// since zs s3/ob2, `exit_with_improperly_specified_vm_option`). Without
/// `-XX:+UseZGC` HotSpot's default G1 accepts the flags, so a default
/// (implicit) ZGC ignores a zero (a zero `ConcGCThreads` silently since zs
/// s3/ob2, `CRATONVM_ZGC_IMPLICIT_ZERO_GC_THREADS_SILENT`; HotSpot's G1
/// refuses a zero `ParallelGCThreads`, s3/ob2 residual page). The
/// `--XX:ParallelGCThreads` spelling is this launcher's, not HotSpot's, and is
/// not refused. `enabled` is `CRATONVM_ZGC_REFUSE_ZERO_GC_THREADS` (default
/// ON, `=0`: the warn-and-continue policy of zr z3/mt). `hotspot_syntax`
/// (`CRATONVM_ZGC_REFUSE_MALFORMED_GC_THREADS`, zs s3/ob2) reads the values
/// in HotSpot's syntax ([`hotspot_parse_uint_flag`]), so a zero spelled
/// `0x0` or `0k` is the zero HotSpot refuses (zs s4/rv3: it used to pass the
/// decimal-only test here and then be accepted or dropped). Pure.
pub(crate) fn zgc_zero_gc_thread_refusal(
    zgc_explicit: bool,
    parallel: Option<&str>,
    conc: Option<&str>,
    enabled: bool,
    hotspot_syntax: bool,
) -> Option<String> {
    if !zgc_explicit || !enabled {
        return None;
    }
    let zero = |v: Option<&str>| {
        v.is_some_and(|s| {
            s.parse::<u64>() == Ok(0) || (hotspot_syntax && hotspot_parse_uint_flag(s) == Some(0))
        })
    };
    let name = if zero(parallel) {
        "ParallelGCThreads"
    } else if zero(conc) {
        "ConcGCThreads"
    } else {
        return None;
    };
    Some(format!(
        "The flag -XX:+UseZGC can not be combined with -XX:{name}=0"
    ))
}

/// Is `algorithm` ZGC? `false` in a build without the `zgc` feature (where
/// the variant does not exist). zs s2/ob.
pub(crate) fn algorithm_is_zgc(algorithm: cratonvm_vm::config::GcAlgorithm) -> bool {
    #[cfg(feature = "zgc")]
    {
        algorithm == cratonvm_vm::config::GcAlgorithm::Zgc
    }
    #[cfg(not(feature = "zgc"))]
    {
        let _ = algorithm;
        false
    }
}

/// [`zgc_zero_gc_thread_refusal`] over the raw launcher section of `argv`
/// (everything before `--`), for the version queries that exit before
/// argument parsing: the last `-XX:+Use<Name>GC` / `-XX:-UseZGC` /
/// `-XX:-UseG1GC` decides the collector (`-XX:-UseZGC` and `-XX:-UseG1GC`
/// select Generational, as in `normalize_java_launcher_argv`), the last
/// `-XX:ParallelGCThreads=` / `-XX:ConcGCThreads=` the counts.
pub(crate) fn scan_zgc_zero_gc_thread_refusal(argv: &[String]) -> Option<String> {
    let mut zgc_explicit = false;
    let mut selected: Option<&str> = None;
    let mut parallel: Option<&str> = None;
    let mut conc: Option<&str> = None;
    for a in argv.iter().skip(1).take_while(|a| a.as_str() != "--") {
        if let Some(name) = a
            .strip_prefix("-XX:+Use")
            .and_then(|core| core.strip_suffix("GC"))
            .filter(|core| !core.is_empty())
        {
            zgc_explicit =
                cratonvm_vm::config::parse_gc_algorithm(name).is_some_and(algorithm_is_zgc);
            selected = Some(name);
        } else if (a == "-XX:-UseZGC" || a == "-XX:-UseG1GC")
            && minus_gc_flag_deselects(a, selected)
        {
            zgc_explicit = false;
            selected = Some("Generational");
        } else if let Some(v) = a.strip_prefix("-XX:ParallelGCThreads=") {
            parallel = Some(v);
        } else if let Some(v) = a.strip_prefix("-XX:ConcGCThreads=") {
            conc = Some(v);
        }
    }
    zgc_zero_gc_thread_refusal(
        zgc_explicit,
        parallel,
        conc,
        true,
        true,
    )
}

/// zs s9/ob6 (`zgc-zs3ob2-default-collector-thread-flag-residuals` item 1):
/// with NO collector flag, HotSpot 25's default collector (G1) refuses a zero
/// `-XX:ParallelGCThreads` (`G1Arguments`): `Error occurred during
/// initialization of VM` / `The flag -XX:+UseG1GC can not be combined with
/// -XX:ParallelGCThreads=0` on stdout, exit 1, before any `-version` banner
/// (local 25.0.3; a zero `ConcGCThreads` it accepts silently, s3/ob2). Both
/// modes target HotSpot's Java-visible answer to the same command line, so
/// this launcher's default (implicit) ZGC gives exactly that answer -- the
/// text names HotSpot's default collector, the one the command line asked
/// for by naming none. Under `CRATONVM_ZGC_IMPLICIT_ZERO_PARALLEL_GC_THREADS_REFUSE`
/// (default ON, `=0` kill: the `Warning: ignoring ...` line and the run).
/// `zgc_implicit`: the effective collector is ZGC and no `-XX:+Use<Name>GC`
/// selected it. `hotspot_syntax` as in [`zgc_zero_gc_thread_refusal`]. Pure.
pub(crate) fn zgc_implicit_zero_parallel_gc_threads_refusal(
    zgc_implicit: bool,
    parallel: Option<&str>,
    enabled: bool,
    hotspot_syntax: bool,
) -> Option<String> {
    if !zgc_implicit || !enabled {
        return None;
    }
    let zero = parallel.is_some_and(|s| {
        s.parse::<u64>() == Ok(0) || (hotspot_syntax && hotspot_parse_uint_flag(s) == Some(0))
    });
    zero.then(|| {
        "The flag -XX:+UseG1GC can not be combined with -XX:ParallelGCThreads=0".to_string()
    })
}



/// [`zgc_implicit_zero_parallel_gc_threads_refusal`] over the raw launcher
/// section of `argv`, for the version queries (as
/// [`scan_zgc_zero_gc_thread_refusal`]): implicit = no `-XX:+Use<Name>GC` /
/// `-XX:-UseZGC` / `-XX:-UseG1GC` at all and a ZGC build; the last
/// `-XX:ParallelGCThreads=` the count. zs s9/ob6.
pub(crate) fn scan_zgc_implicit_zero_parallel_gc_threads_refusal(argv: &[String]) -> Option<String> {
    let mut selector_seen = false;
    let mut parallel: Option<&str> = None;
    for a in argv.iter().skip(1).take_while(|a| a.as_str() != "--") {
        if (a.starts_with("-XX:+Use") && a.ends_with("GC") && a.len() > "-XX:+UseGC".len())
            || a == "-XX:-UseZGC"
            || a == "-XX:-UseG1GC"
        {
            selector_seen = true;
        } else if let Some(v) = a.strip_prefix("-XX:ParallelGCThreads=") {
            parallel = Some(v);
        }
    }
    zgc_implicit_zero_parallel_gc_threads_refusal(
        !selector_seen && scan_effective_zgc(argv),
        parallel,
        true,
        true,
    )
}

/// HotSpot's `vm_exit_during_initialization(detail)` before a VM exists: the
/// two lines on stdout, nothing on stderr, exit status 1. zs s2/ob.
pub(crate) fn exit_with_vm_initialization_error(detail: &str) -> ! {
    use std::io::Write as _;
    let report = cratonvm_vm::error::VmInitializationError {
        detail: detail.to_string(),
    }
    .report();
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(report.as_bytes());
    let _ = out.flush();
    std::process::exit(1);
}

/// zs s3/ob2: HotSpot 25's value syntax for the `uint` GC thread flags
/// (`-XX:ParallelGCThreads=`, `-XX:ConcGCThreads=`), as observed on the local
/// 25.0.3: decimal digits or `0x` / `0X` hex digits, optionally followed by
/// one `k` / `m` / `g` / `t` multiplier (either case), within `u32`
/// (`ConcGCThreads=0x10` and `=3k` and `=1K` boot; `=abc`, `=-1`, `=+3`,
/// `=1.5`, `=`, `=0x` and `=4294967296` are `Improperly specified VM
/// option`). `None` is a malformed value. Pure.
pub(crate) fn hotspot_parse_uint_flag(s: &str) -> Option<u32> {
    let (body, radix) = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => (hex, 16),
        None => (s, 10),
    };
    let (digits, multiplier) = match body.chars().last() {
        Some(c @ ('k' | 'K' | 'm' | 'M' | 'g' | 'G' | 't' | 'T')) => (
            &body[..body.len() - 1],
            match c.to_ascii_lowercase() {
                'k' => 1u64 << 10,
                'm' => 1 << 20,
                'g' => 1 << 30,
                _ => 1 << 40,
            },
        ),
        _ => (body, 1),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    let value = u64::from_str_radix(digits, radix)
        .ok()?
        .checked_mul(multiplier)?;
    u32::try_from(value).ok()
}

/// zs s3/ob2: the first malformed GC thread flag of the launcher section of
/// `argv` (everything before `--`, which `insert_program_args_separator`
/// places after the program selector), as HotSpot names it in its refusal:
/// `ParallelGCThreads=abc`. HotSpot parses the options in command-line order
/// and stops at the first bad one (local 25.0.3: `-XX:ConcGCThreads=y
/// -XX:ParallelGCThreads=x` names `ConcGCThreads=y`), even one a later
/// occurrence would override. Pure.
pub(crate) fn scan_malformed_gc_thread_flag(argv: &[String]) -> Option<String> {
    argv.iter()
        .skip(1)
        .take_while(|a| a.as_str() != "--")
        .find_map(|a| malformed_gc_thread_option(a))
}

/// [`scan_malformed_gc_thread_flag`]'s answer for ONE launcher argument: a
/// `-XX:ParallelGCThreads=` / `-XX:ConcGCThreads=` value outside HotSpot's
/// `uint` syntax, as HotSpot names it (`ParallelGCThreads=abc`). zt-dev
/// merge: split out for the ordered first-refusal scan's G1 arm
/// (`OrderedVmOptionRules::g1_gc_threads`). Pure.
pub(crate) fn malformed_gc_thread_option(a: &str) -> Option<String> {
    ["ParallelGCThreads", "ConcGCThreads"].iter().find_map(|name| {
        let v = a
            .strip_prefix("-XX:")?
            .strip_prefix(name)?
            .strip_prefix('=')?;
        hotspot_parse_uint_flag(v)
            .is_none()
            .then(|| format!("{name}={v}"))
    })
}

/// Does the launcher section of `argv` name no collector at all (no
/// `-XX:+Use<Name>GC`, no `-XX:-UseZGC` / `-XX:-UseG1GC`)? HotSpot then runs
/// its default G1, and the constraint lines it prints are G1's. Pure.
pub(crate) fn scan_names_no_collector(argv: &[String]) -> bool {
    !argv.iter().skip(1).take_while(|a| a.as_str() != "--").any(|a| {
        a == "-XX:-UseZGC"
            || a == "-XX:-UseG1GC"
            || a
                .strip_prefix("-XX:+Use")
                .and_then(|core| core.strip_suffix("GC"))
                .is_some_and(|core| !core.is_empty())
    })
}

/// Does HotSpot 25 answer this command line's collector constraints as G1:
/// an effective G1, or a command line naming no collector (HotSpot's
/// default)? Pure.
pub(crate) fn scan_hotspot_constraints_are_g1(argv: &[String]) -> bool {
    scan_effective_g1(argv) || scan_names_no_collector(argv)
}

/// `CRATONVM_LAUNCHER_HOTSPOT_FLAG_TABLE`: HotSpot 25.0.3's `AfterErgo`
/// constraint lines for the flags this launcher takes but no collector
/// checks itself (local rows, before a `-version` banner too), in HotSpot's
/// order. Empty = start.
///
/// * on G1 (`g1`, see [`scan_hotspot_constraints_are_g1`]):
///   `MaxGCPauseMillisConstraintFuncG1` (`MaxGCPauseMillis (<m>) must be
///   less than GCPauseIntervalMillis (<i>)`, for an explicit pair), then
///   `GCPauseIntervalMillisConstraintFuncG1` (`GCPauseIntervalMillis (<i>)
///   must be greater than or equal to 1`; else `GCPauseIntervalMillis cannot
///   be set without setting MaxGCPauseMillis`; else `GCPauseIntervalMillis
///   (<i>) must be greater than MaxGCPauseMillis (<m>)`). Other collectors
///   take both flags without a word.
/// (`SoftMaxHeapSize` above the maximum is
/// [`scan_soft_max_above_max_heap`]'s, zt t10/tg3.)
///
/// The last spelling of each flag wins; values are HotSpot's parse (the
/// early scan refused a bad one). Pure.
pub(crate) fn hotspot_after_ergo_constraint_errors(argv: &[String], g1: bool) -> Vec<String> {
    let last = |name: &str| {
        let prefix = format!("-XX:{name}=");
        argv.iter()
            .skip(1)
            .take_while(|a| a.as_str() != "--")
            .filter_map(|a| a.strip_prefix(prefix.as_str()))
            .last()
            .and_then(hotspot_parse_uintx)
    };
    let mut lines = Vec::new();
    if g1 {
        let max_pause = last("MaxGCPauseMillis");
        let interval = last("GCPauseIntervalMillis");
        if let (Some(m), Some(i)) = (max_pause, interval) {
            if m >= i {
                lines.push(format!(
                    "MaxGCPauseMillis ({m}) must be less than GCPauseIntervalMillis ({i})"
                ));
            }
        }
        if let Some(i) = interval {
            match max_pause {
                _ if i < 1 => lines.push(format!(
                    "GCPauseIntervalMillis ({i}) must be greater than or equal to 1"
                )),
                None => lines
                    .push("GCPauseIntervalMillis cannot be set without setting MaxGCPauseMillis".into()),
                Some(m) if i <= m => lines.push(format!(
                    "GCPauseIntervalMillis ({i}) must be greater than MaxGCPauseMillis ({m})"
                )),
                Some(_) => {}            }        }    }
    lines
}

/// Share the launcher's last-selector/deselection policy with the other scans.
pub(crate) fn scan_effective_g1(argv: &[String]) -> bool {
    !scan_effective_zgc(argv) && !scan_effective_generational(argv)
}

/// G1 requires a positive parallel pool size, including before -version.
/// Read only launcher arguments; the last valid occurrence wins.
pub(crate) fn scan_g1_zero_parallel_gc_threads_refusal(argv: &[String]) -> Option<String> {
    if !scan_effective_g1(argv) {
        return None;
    }
    let mut parallel = None;
    let mut args = argv.iter().skip(1).take_while(|a| a.as_str() != "--");
    while let Some(arg) = args.next() {
        if let Some(value) = arg
            .strip_prefix("-XX:ParallelGCThreads=")
            .or_else(|| arg.strip_prefix("--XX:ParallelGCThreads="))
            .or_else(|| arg.strip_prefix("--XX:ZgcParallelGCThreads="))
        {
            parallel = hotspot_parse_uint_flag(value);
        } else if arg == "--XX:ParallelGCThreads" || arg == "--XX:ZgcParallelGCThreads" {
            parallel = args.next().and_then(|value| hotspot_parse_uint_flag(value));
        }
    }
    (parallel == Some(0)).then(|| {
        "The flag -XX:+UseG1GC can not be combined with -XX:ParallelGCThreads=0".to_string()
    })
}

/// zs s3/ob2: does the launcher section of `argv` select ZGC, explicitly
/// (the last `-XX:+Use<Name>GC` naming ZGC) or by default (no selector, in a
/// build whose default collector is ZGC)? `-XX:-UseZGC` / `-XX:-UseG1GC` and
/// an unsupported selector select Generational, as the parsed path does.
///
/// gcj j2/x1 (`gcj-j2x1-wave-j1-default-on-review-residuals` item 11): the
/// selection is [`scan_selected_gc_name`]'s, which also reads the internal
/// `--XX:UseGc <Name>` spelling the parsed path takes.
pub(crate) fn scan_effective_zgc(argv: &[String]) -> bool {
    match scan_selected_gc_name(argv) {
        Some(name) => cratonvm_vm::config::parse_gc_algorithm(name).is_some_and(algorithm_is_zgc),
        None => cfg!(feature = "zgc"),
    }
}

/// zt t11/tg4 (`t11-x8-report.md` T2): is `arg` one of the maximum-heap
/// spellings [`early_max_heap`] reads (`-Xmx<size>`, `-Xmx <size>`,
/// `--Xmx=<size>`, `--Xmx <size>`, `-XX:MaxHeapSize=<size>`)? Pure.
pub(crate) fn is_max_heap_option(arg: &str) -> bool {
    arg.starts_with("-Xmx") || arg.starts_with("--Xmx") || arg.starts_with("-XX:MaxHeapSize=")
}

/// zt t11/tg4 (`zgc-zt10tg3-*` item 6): the `<Name>` of the collector the
/// launcher section of `argv` selects, by the parsed path's rule
/// (`normalize_java_launcher_argv`): the last `-XX:+Use<Name>GC`, and a later
/// `-XX:-UseG1GC` / `-XX:-UseZGC` that deselects it
/// ([`minus_gc_flag_deselects`]) makes it `Generational`. `None` when no
/// selector is given (the build's default collector). The version path's
/// [`scan_soft_max_above_max_heap`] took the last `-XX:+Use<Name>GC` alone,
/// so `-XX:+UseG1GC -XX:-UseG1GC` (Generational) still aligned as G1. Pure.
///
/// gcj j2/x1 (`gcj-j2x1-wave-j1-default-on-review-residuals` item 11): the
/// internal spelling `--XX:UseGc <Name>` (`--XX:UseGc=<Name>`), which
/// `normalize_java_launcher_argv` emits and clap takes as the same option, is
/// a selector too; every argv scan read only the `-XX:+Use<Name>GC` form, so
/// `--XX:UseGc Generational` skipped the early Generational refusals.
pub(crate) fn scan_selected_gc_name(argv: &[String]) -> Option<&str> {
    let mut selected: Option<&str> = None;
    let mut args = argv.iter().skip(1).take_while(|a| a.as_str() != "--");
    while let Some(a) = args.next() {
        if let Some(name) = a
            .strip_prefix("-XX:+Use")
            .and_then(|core| core.strip_suffix("GC"))
            .filter(|core| !core.is_empty())
        {
            selected = Some(name);
        } else if let Some(name) = a.strip_prefix("--XX:UseGc=") {
            selected = Some(name);
        } else if a == "--XX:UseGc" {
            if let Some(name) = args.next() {
                selected = Some(name.as_str());
            }
        } else if (a == "-XX:-UseZGC" || a == "-XX:-UseG1GC")
            && minus_gc_flag_deselects(a, selected)
        {
            selected = Some("Generational");
        }
    }
    selected
}

/// zt t3/ob (`zgc-zs3rv2-wave-s2-review-residuals-CLOSED-20261019.md` §8): does
/// `flag` (`-XX:-UseG1GC` / `-XX:-UseZGC`) deselect the collector, given the
/// `<Name>` of the last `-XX:+Use<Name>GC` before it (`selected`)? HotSpot
/// 25 turns the named collector's flag off and nothing else, so turning off
/// a collector that is not the selected one changes nothing
/// (`-XX:+UseZGC -XX:-UseG1GC` runs ZGC). With no collector selected yet,
/// or under `CRATONVM_LAUNCHER_OTHER_GC_OFF_NOOP=0` (default ON), the flag
/// selects Generational, as before. G1 / Generational command lines are
/// unchanged; only a line that selected one collector and turned another
/// off now keeps the one it selected.
pub(crate) fn minus_gc_flag_deselects(flag: &str, selected: Option<&str>) -> bool {
    minus_gc_flag_deselects_with(
        flag,
        selected,
        true,
    )
}

/// [`minus_gc_flag_deselects`] with the switch given. Pure.
pub(crate) fn minus_gc_flag_deselects_with(flag: &str, selected: Option<&str>, enabled: bool) -> bool {
    let Some(name) = selected else {
        return true;
    };
    if !enabled {
        return true;
    }
    let off = if flag == "-XX:-UseG1GC" { "G1" } else { "Z" };
    match (
        cratonvm_vm::config::parse_gc_algorithm(name),
        cratonvm_vm::config::parse_gc_algorithm(off),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => name.eq_ignore_ascii_case(off) || (off == "Z" && name.eq_ignore_ascii_case("zgc")),
    }
}





/// HotSpot's refusal of a malformed `-XX:` value (`Arguments::parse_argument`
/// failing): three lines on stderr, nothing on stdout, exit status 1
/// (local 25.0.3, any collector). zs s3/ob2.
pub(crate) fn exit_with_improperly_specified_vm_option(option: &str) -> ! {
    use std::io::Write as _;
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "Improperly specified VM option '{option}'");
    let _ = writeln!(err, "Error: Could not create the Java Virtual Machine.");
    let _ = writeln!(
        err,
        "Error: A fatal exception has occurred. Program will exit."
    );
    let _ = err.flush();
    std::process::exit(1);
}

/// zt t7/x4: HotSpot 25's value syntax for a `double` `-XX:` flag
/// (`Arguments::parse_argument` over `strtod`): no leading or trailing blank,
/// the whole text consumed, a finite result, and no underflow (`strtod`'s
/// `ERANGE`). Local 25.0.3 rows for `-XX:ZAllocationSpikeTolerance`: `0`,
/// `-1`, `+3`, `.5`, `1e-310` (a subnormal, on Windows) and the hexadecimal
/// forms `0x1p3` (8.0), `0x10` (16.0), `-0x1.8p1` (-3.0), `0X1P-2` (0.25) and
/// `0x.8` (0.5) are taken; ` 3`, `3 `, empty, `abc`, `inf`, `1e400`,
/// `1e-400` and `0x` are `Improperly specified VM option`. `None` is the
/// refusal. zt t7/x4b: the hexadecimal form ([`hotspot_parse_hex_double`])
/// was refused here. Pure.
pub(crate) fn hotspot_parse_double_flag(raw: &str) -> Option<f64> {
    if raw.chars().next().is_none_or(char::is_whitespace) {
        return None;
    }
    let unsigned = raw.strip_prefix(['+', '-']).unwrap_or(raw);
    if unsigned.starts_with("0x") || unsigned.starts_with("0X") {
        return hotspot_parse_hex_double(raw);
    }
    let v = raw.parse::<f64>().ok().filter(|v| v.is_finite())?;
    // `strtod` reports a nonzero number too small for a double as `ERANGE`,
    // which HotSpot refuses; Rust rounds it to zero.
    let mantissa = raw.split(['e', 'E']).next().unwrap_or(raw);
    if v == 0.0 && mantissa.bytes().any(|b| (b'1'..=b'9').contains(&b)) {
        return None;
    }
    Some(v)
}

/// zt t7/x4b: `strtod`'s hexadecimal form, which Rust's `f64` parser lacks:
/// an optional sign, `0x`/`0X`, hex digits with at most one `.` (at least one
/// digit), and an optional binary exponent `p`/`P` with an optional sign and
/// at least one decimal digit. `None` for anything else, for an infinite
/// result and for a nonzero mantissa that underflows to zero (`ERANGE`), as
/// [`hotspot_parse_double_flag`] refuses them. Pure.
pub(crate) fn hotspot_parse_hex_double(raw: &str) -> Option<f64> {
    let (negative, rest) = match raw.as_bytes().first() {
        Some(b'-') => (true, &raw[1..]),
        Some(b'+') => (false, &raw[1..]),
        _ => (false, raw),
    };
    let body = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X"))?;
    let (mantissa, exponent) = match body.find(['p', 'P']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    let mut value = 0f64;
    let mut nonzero = false;
    for b in int_part.bytes().chain(frac_part.bytes()) {
        let digit = char::from(b).to_digit(16)?;
        nonzero |= digit != 0;
        value = value * 16.0 + f64::from(digit);
    }
    let mut exp = i32::try_from(frac_part.len()).ok()?.checked_mul(-4)?;
    if let Some(e) = exponent {
        let digits = e.strip_prefix(['+', '-']).unwrap_or(e);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        exp = exp.checked_add(e.parse::<i32>().ok()?)?;
    }
    let v = if value == 0.0 { 0.0 } else { value * 2f64.powi(exp) };
    if !v.is_finite() || (v == 0.0 && nonzero) {
        return None;
    }
    Some(if negative { -v } else { v })
}



/// `CRATONVM_ZGC_SOFT_MAX_ZERO_HOTSPOT` (zt t7/x4b; default ON, `=0` kill):
/// on ZGC `-XX:SoftMaxHeapSize=0` is kept (a soft maximum of 0, HotSpot
/// 25.0.3's `Soft Max Capacity: 0M`), not warned about and dropped. The ZGC
/// heap reads the same key when it applies the value.
pub(crate) fn zgc_soft_max_zero_hotspot_enabled() -> bool {
    true
}

/// zt t7/x4b: a ZGC boolean `-XX:` flag this launcher reads (`ZProactive`,
/// `ZUncommit`) given WITHOUT its `+`/`-` (`-XX:ZProactive=false`,
/// `-XX:ZUncommit=true`, a bare `-XX:ZProactive`): the option text after
/// `-XX:`, which HotSpot's refusal line quotes. `None` for any other argument
/// (`-XX:+ZProactive`, `-XX:ZUncommitDelay=5`, ...). Pure.
pub(crate) fn zgc_boolean_without_sign(arg: &str) -> Option<&str> {
    let option = arg.strip_prefix("-XX:")?;
    let name = option.split('=').next().unwrap_or(option);
    matches!(name, "ZProactive" | "ZUncommit").then_some(option)
}



/// HotSpot's refusal of a boolean `-XX:` flag without `+`/`-` (local 25.0.3,
/// `-XX:ZProactive=false`, `=1`, bare `-XX:ZProactive`, `-XX:ZUncommit=true`):
/// `Missing +/- setting for VM option '<option>'` and the two `Error:` lines
/// on stderr, nothing on stdout, exit status 1.
pub(crate) fn exit_with_missing_plus_minus(option: &str) -> ! {
    use std::io::Write as _;
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "Missing +/- setting for VM option '{option}'");
    let _ = writeln!(err, "Error: Could not create the Java Virtual Machine.");
    let _ = writeln!(
        err,
        "Error: A fatal exception has occurred. Program will exit."
    );
    let _ = err.flush();
    std::process::exit(1);
}

/// `CRATONVM_LAUNCHER_ZGC_FLAG_HOTSPOT_PARSE` (zt t9/tg2; default ON, `=0`
/// kill: the t8 launcher). HotSpot 25 parses the `-XX:` options in
/// command-line order and refuses the FIRST bad one, before anything else
/// runs ([`scan_first_zgc_flag_refusal`]); `-XX:ZUncommitDelay` and, on ZGC,
/// `-XX:SoftMaxHeapSize` take HotSpot's integer syntax
/// ([`hotspot_parse_uintx_flag`]); `-XX:ZCollectionIntervalMajor` is read
/// and wins over its alias `-XX:ZCollectionInterval`.
pub(crate) fn launcher_zgc_flag_hotspot_parse_enabled() -> bool {
    true
}

/// `CRATONVM_LAUNCHER_ZGC_FLAGS_ANY_COLLECTOR` (zt t9/tg2; default ON, `=0`
/// kill: refused on ZGC only, ignored on the other collectors). HotSpot's
/// option parsing does not depend on the collector: local 25.0.3 refuses
/// `-XX:+UseG1GC -XX:ZProactive=false` (`Missing +/- setting`) and
/// `-XX:+UseG1GC -XX:ZCollectionInterval=abc` / `-XX:ZUncommitDelay=abc` /
/// `-XX:SoftMaxHeapSize=abc` (`Improperly specified VM option`), exit 1.
/// Acts only under [`launcher_zgc_flag_hotspot_parse_enabled`]. A G1 /
/// Generational command line HotSpot accepts is unchanged.
pub(crate) fn launcher_zgc_flags_any_collector_enabled() -> bool {
    true
}

/// zt t9/tg2: do the ZGC flags take HotSpot's value syntax under `algorithm`?
/// On ZGC under [`launcher_zgc_flag_hotspot_parse_enabled`], and on every
/// collector under [`launcher_zgc_flags_any_collector_enabled`] too: a value
/// HotSpot takes on G1 (`-XX:ZAllocationSpikeTolerance=0`,
/// `-XX:ZCollectionInterval=0x1p1`, `-XX:SoftMaxHeapSize=0x100000`) is then
/// taken silently, as HotSpot 25.0.3 takes it, where the old off-ZGC arms
/// printed a `Warning: ignoring ...` line. The values are read only by a ZGC
/// heap, so off ZGC nothing else changes.
pub(crate) fn launcher_zgc_flags_take_hotspot_syntax(algorithm: cratonvm_vm::config::GcAlgorithm) -> bool {
    launcher_zgc_flag_hotspot_parse_enabled()
        && (algorithm_is_zgc(algorithm) || launcher_zgc_flags_any_collector_enabled())
}

/// zt t9/tg2: HotSpot 25's value syntax for a `uintx` / `size_t` `-XX:` flag
/// (`ZUncommitDelay`, `SoftMaxHeapSize`): [`hotspot_parse_uint_flag`]'s
/// syntax (decimal or `0x` hex digits, one optional `k`/`m`/`g`/`t`) within
/// `u64`. Local 25.0.3: `ZUncommitDelay=4294967296`, `=18446744073709551615`,
/// `=0x10`, `=3k`, `=16t` and `SoftMaxHeapSize=0x100000` boot;
/// `ZUncommitDelay=18446744073709551616`, `=17179869184t`, `=-1`, `=+5`,
/// `=`, `=abc` and `SoftMaxHeapSize=1.5g`, `=1b`, `=-1` are `Improperly
/// specified VM option`. `None` is the refusal. Pure.
pub(crate) fn hotspot_parse_uintx_flag(s: &str) -> Option<u64> {
    let (body, radix) = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => (hex, 16),
        None => (s, 10),
    };
    let (digits, multiplier) = match body.chars().last() {
        Some(c @ ('k' | 'K' | 'm' | 'M' | 'g' | 'G' | 't' | 'T')) => (
            &body[..body.len() - 1],
            match c.to_ascii_lowercase() {
                'k' => 1u64 << 10,
                'm' => 1 << 20,
                'g' => 1 << 30,
                _ => 1 << 40,
            },
        ),
        _ => (body, 1),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    u64::from_str_radix(digits, radix)
        .ok()?
        .checked_mul(multiplier)
}

/// zt t9/tg2: one HotSpot refusal of a `-XX:` option, as the launcher reports
/// it before any VM exists (three lines on stderr, exit status 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ZgcFlagRefusal {
    /// `Improperly specified VM option '<option>'`
    /// ([`exit_with_improperly_specified_vm_option`]).
    Improper(String),
    /// `Missing +/- setting for VM option '<option>'`
    /// ([`exit_with_missing_plus_minus`]).
    MissingSign(String),
    /// zt t24/hunt7: HotSpot's own lines, then the two `Error:` lines
    /// ([`exit_with_vm_option_lines`]): a limit flag's `double` range line
    /// and its `Improperly specified VM option` line
    /// ([`launcher_zgc_limit_flag_range_enabled`]), or, off Linux, the
    /// `Unrecognized VM option` line of a Linux-only flag
    /// ([`launcher_thp_flag_linux_only_enabled`]).
    Lines(Vec<String>),
}

impl ZgcFlagRefusal {
    pub(crate) fn exit(&self) -> ! {
        match self {
            ZgcFlagRefusal::Improper(option) => exit_with_improperly_specified_vm_option(option),
            ZgcFlagRefusal::MissingSign(option) => exit_with_missing_plus_minus(option),
            ZgcFlagRefusal::Lines(lines) => exit_with_vm_option_lines(lines),
        }
    }
}

/// zt t9/tg2: which refusals [`scan_first_zgc_flag_refusal`] reports. Each
/// field is the switch of the refusal it orders, so a kill of one arm still
/// turns that arm off.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ZgcFlagRefusalRules {
    /// ZGC is the effective collector ([`scan_effective_zgc`]).
    pub(crate) zgc: bool,
    /// The ZGC flags are refused on every collector
    /// ([`launcher_zgc_flags_any_collector_enabled`]).
    pub(crate) any_collector: bool,
    /// `CRATONVM_ZGC_BOOLEAN_FLAG_SIGN_REQUIRED`: a sign-less boolean.
    pub(crate) sign_required: bool,
    /// `CRATONVM_ZGC_SPIKE_TOLERANCE_HOTSPOT_PARSE`: the `double` flags.
    pub(crate) double_syntax: bool,
    /// `CRATONVM_ZGC_REFUSE_MALFORMED_GC_THREADS`: the GC thread counts
    /// (refused on ZGC only, as before; the G1 / Generational launcher is
    /// another page's).
    pub(crate) gc_threads: bool,
    /// zt t11/tg4, `CRATONVM_LAUNCHER_ZGC_GC_VALUE_FLAG_REFUSALS`: the GC
    /// value flags of [`gc_value_flag_parse_refusal_of`] in the ordered scan
    /// (on ZGC only: the G1 / Generational launcher is another page's).
    pub(crate) gc_values: bool,
    /// zt t23/hunt4, `CRATONVM_LAUNCHER_ZGC_LIMIT_FLAG_SYNTAX`: the `double`
    /// flags `ZFragmentationLimit` and `ZYoungCompactionLimit`
    /// ([`launcher_zgc_limit_flag_syntax_enabled`]).
    pub(crate) limit_flags: bool,
    /// zt t24/hunt7, `CRATONVM_LAUNCHER_ZGC_LIMIT_FLAG_RANGE`: with
    /// `limit_flags`, HotSpot's `range(0, 100)` of the two limit flags
    /// ([`launcher_zgc_limit_flag_range_enabled`]).
    pub(crate) limit_range: bool,
    /// zt t24/hunt7, `CRATONVM_LAUNCHER_THP_FLAG_LINUX_ONLY`: off Linux, and
    /// without `-XX:+IgnoreUnrecognizedVMOptions`, `-XX:±UseTransparentHugePages`
    /// is an unrecognized option ([`linux_only_flag_unrecognized_line`]).
    pub(crate) thp_unrecognized: bool,
    /// zt t25/hunt7b, `CRATONVM_LAUNCHER_LARGE_PAGE_FLAG_SIGN`: the booleans
    /// `UseLargePages` and (Linux) `UseTransparentHugePages` need their sign
    /// ([`large_page_flag_sign_refusal_of`]).
    pub(crate) large_page_sign: bool,
}

impl ZgcFlagRefusalRules {
    /// The rules for the launcher section of `argv`, from the switches.
    pub(crate) fn from_switches(argv: &[String]) -> ZgcFlagRefusalRules {
        ZgcFlagRefusalRules {
            zgc: scan_effective_zgc(argv),
            any_collector: launcher_zgc_flags_any_collector_enabled(),
            sign_required: true,
            double_syntax: true,
            gc_threads: true,
            gc_values: launcher_zgc_gc_value_flag_refusals_enabled(),
            limit_flags: launcher_zgc_limit_flag_syntax_enabled(),
            limit_range: launcher_zgc_limit_flag_range_enabled(),
            thp_unrecognized: !cfg!(target_os = "linux")
                && launcher_thp_flag_linux_only_enabled()
                && !scan_ignore_unrecognized_vm_options(argv),
            large_page_sign: launcher_large_page_flag_sign_enabled(),
        }
    }
}

/// `CRATONVM_LAUNCHER_LARGE_PAGE_FLAG_SIGN` (zt t25/hunt7b, default ON, `=0`
/// kill). `UseLargePages` and, on Linux, `UseTransparentHugePages` are HotSpot
/// 25 `bool` flags, so a spelling without `+`/`-` is `Missing +/- setting for
/// VM option '<option>'` and a signed one with a value (`-XX:+UseLargePages=true`)
/// is `Improperly specified VM option '<name>=<value>'`, each with the two
/// `Error:` lines, exit 1 (host row `h7_W1e`, 25.0.4 on Linux:
/// `-XX:UseTransparentHugePages=true` -> `Missing +/- setting for VM option
/// 'UseTransparentHugePages=true'`). Both fell into the silent `-XX:`
/// catch-all and booted. Every collector. Off Linux `UseTransparentHugePages`
/// is unrecognized instead (`CRATONVM_LAUNCHER_THP_FLAG_LINUX_ONLY`). `=0`:
/// dropped silently, as before.
pub(crate) fn launcher_large_page_flag_sign_enabled() -> bool {
    true
}

/// zt t25/hunt7b: HotSpot's refusal of a sign-less or valued spelling of the
/// large-page booleans, `option` being the text after `-XX:` (see
/// [`launcher_large_page_flag_sign_enabled`]); `None` for `-XX:+Name` /
/// `-XX:-Name` and for every other option. `linux` says whether
/// `UseTransparentHugePages` exists. Pure.
pub(crate) fn large_page_flag_sign_refusal_of(option: &str, linux: bool) -> Option<ZgcFlagRefusal> {
    let is_flag =
        |name: &str| name == "UseLargePages" || (linux && name == "UseTransparentHugePages");
    match option.strip_prefix(['+', '-']) {
        Some(signed) => {
            let (name, _) = signed.split_once('=')?;
            is_flag(name).then(|| ZgcFlagRefusal::Improper(signed.to_string()))
        }
        None => {
            let name = option.split('=').next().unwrap_or(option);
            is_flag(name).then(|| ZgcFlagRefusal::MissingSign(option.to_string()))
        }
    }
}

/// `CRATONVM_LAUNCHER_ZGC_LIMIT_FLAG_RANGE` (zt t24/hunt7, default ON, `=0`
/// kill; acts with `CRATONVM_LAUNCHER_ZGC_LIMIT_FLAG_SYNTAX`). HotSpot 25
/// declares `ZFragmentationLimit` and `ZYoungCompactionLimit` `range(0, 100)`
/// (`gc/z/z_globals.hpp`), and its parse-time range check of a `double` flag
/// prints `double <Name>=<%f> is outside the allowed range [ <%f> ... <%f> ]`
/// (`FlagAccessImpl_double::range_error`) before the `Improperly specified
/// VM option` line, as the `uint` / `uintx` ranges of
/// [`gc_value_flag_parse_refusal_of`] do. Recalled, not measured: the
/// deciding row is `h7_L3` (`t24-hunt7-rows.txt`). `=0`: an out-of-range
/// value boots, as before.
pub(crate) fn launcher_zgc_limit_flag_range_enabled() -> bool {
    true
}

/// zt t24/hunt7: HotSpot's range line and refusal for a parsed limit-flag
/// value `v` of `option` (`<Name>=<raw>`), or `None` inside `range(0, 100)`.
/// `%f` is six decimals, which `{:.6}` matches (both print every integer
/// digit). Pure.
pub(crate) fn zgc_limit_flag_range_refusal(name: &str, option: &str, v: f64) -> Option<Vec<String>> {
    if (0.0..=100.0).contains(&v) {
        return None;
    }
    Some(vec![
        format!("double {name}={v:.6} is outside the allowed range [ 0.000000 ... 100.000000 ]"),
        format!("Improperly specified VM option '{option}'"),
    ])
}

/// `CRATONVM_LAUNCHER_THP_FLAG_LINUX_ONLY` (zt t24/hunt7, default ON, `=0`
/// kill). `UseTransparentHugePages` is declared in HotSpot 25's
/// `globals_linux.hpp` only, so on Windows and macOS any spelling of it is
/// `Unrecognized VM option '<argname>'` and the two `Error:` lines, exit 1,
/// unless `-XX:+IgnoreUnrecognizedVMOptions` is given
/// (`Arguments::process_argument`). This launcher dropped it silently on every
/// platform. On Linux the flag is honoured (`VmConfig::use_transparent_huge_pages`).
/// `=0`: dropped silently off Linux, as before.
pub(crate) fn launcher_thp_flag_linux_only_enabled() -> bool {
    true
}

/// zt t24/hunt7: HotSpot's `-XX:±IgnoreUnrecognizedVMOptions`, which it reads
/// before parsing the other options (anywhere in the option section, the
/// last one wins). Pure.
pub(crate) fn scan_ignore_unrecognized_vm_options(argv: &[String]) -> bool {
    argv.iter()
        .skip(1)
        .take_while(|a| a.as_str() != "--")
        .filter_map(|a| match a.as_str() {
            "-XX:+IgnoreUnrecognizedVMOptions" => Some(true),
            "-XX:-IgnoreUnrecognizedVMOptions" => Some(false),
            _ => None,
        })
        .last()
        .unwrap_or(false)
}

/// zt t24/hunt7: HotSpot 25's `Unrecognized VM option '<argname>'` line for a
/// Linux-only flag given off Linux, `option` being the text after `-XX:`.
/// `argname` is the option without its sign, `=<value>` kept
/// (`process_argument` prints it so). Only `UseTransparentHugePages`. Pure.
pub(crate) fn linux_only_flag_unrecognized_line(option: &str) -> Option<String> {
    let argname = option.strip_prefix(['+', '-']).unwrap_or(option);
    let name = argname.split('=').next().unwrap_or(argname);
    (name == "UseTransparentHugePages").then(|| format!("Unrecognized VM option '{argname}'"))
}

/// `CRATONVM_LAUNCHER_ZGC_LIMIT_FLAG_SYNTAX` (zt t23/hunt4, default ON, `=0`
/// kill). HotSpot 25's `double` flags `ZFragmentationLimit` and
/// `ZYoungCompactionLimit` take the `double` syntax
/// ([`hotspot_parse_double_flag`]); a value outside it, or the name without
/// `=<value>`, is `Improperly specified VM option '<option>'` in the ordered
/// first-refusal scan, as for `ZAllocationSpikeTolerance` (the same `double`
/// parse in `Arguments::parse_argument`). Both fell into the silent `-XX:`
/// catch-all, which booted on `-XX:ZFragmentationLimit=abc`. The values are
/// still ignored (`docs/GC.md`, ZGC paragraph G); HotSpot's `range(0, 100)`
/// check is [`launcher_zgc_limit_flag_range_enabled`]'s (zt t24/hunt7).
/// `=0`: dropped silently, as before.
pub(crate) fn launcher_zgc_limit_flag_syntax_enabled() -> bool {
    true
}

/// zt t9/tg2: HotSpot's refusal of ONE launcher argument under `rules`, or
/// `None` when HotSpot takes it (or it is not one of the flags this scan
/// orders). The flags: the sign-less booleans `ZProactive`, `ZUncommit`
/// ([`zgc_boolean_without_sign`]) and `ZCollectionIntervalOnly`; the `double`
/// flags `ZAllocationSpikeTolerance`, `ZCollectionInterval`,
/// `ZCollectionIntervalMajor`, `ZCollectionIntervalMinor`
/// ([`hotspot_parse_double_flag`]); `ZUncommitDelay` and `SoftMaxHeapSize`
/// ([`hotspot_parse_uintx_flag`]); on ZGC only, `ParallelGCThreads` and
/// `ConcGCThreads` ([`hotspot_parse_uint_flag`]). Also a value flag given
/// without `=<value>` and a signed boolean given with one
/// (`-XX:-ZProactive=true`). Pure.
pub(crate) fn zgc_flag_refusal_of(arg: &str, rules: ZgcFlagRefusalRules) -> Option<ZgcFlagRefusal> {
    let option = arg.strip_prefix("-XX:")?;
    // zt t24/hunt7 (`CRATONVM_LAUNCHER_THP_FLAG_LINUX_ONLY`): option parsing
    // does not depend on the collector, so on every collector.
    if rules.thp_unrecognized {
        if let Some(line) = linux_only_flag_unrecognized_line(option) {
            return Some(ZgcFlagRefusal::Lines(vec![line]));
        }
    }
    // zt t25/hunt7b (`CRATONVM_LAUNCHER_LARGE_PAGE_FLAG_SIGN`): every collector.
    if rules.large_page_sign {
        if let Some(refusal) = large_page_flag_sign_refusal_of(option, cfg!(target_os = "linux")) {
            return Some(refusal);
        }
    }
    let zgc_flags = rules.zgc || rules.any_collector;
    if zgc_flags && rules.sign_required {
        let name = option.split('=').next().unwrap_or(option);
        if zgc_boolean_without_sign(arg).is_some() || name == "ZCollectionIntervalOnly" {
            return Some(ZgcFlagRefusal::MissingSign(option.to_string()));
        }
        // A signed boolean with a value (`-XX:-ZProactive=true`): HotSpot
        // 25.0.3 names it without its sign, `Improperly specified VM option
        // 'ZProactive=true'`. It fell into the silent `-XX:` catch-all.
        if let Some(unsigned) = option.strip_prefix(['+', '-']) {
            let name = unsigned.split('=').next().unwrap_or(unsigned);
            let boolean = matches!(name, "ZProactive" | "ZUncommit" | "ZCollectionIntervalOnly");
            if boolean && unsigned.contains('=') {
                return Some(ZgcFlagRefusal::Improper(unsigned.to_string()));
            }
        }
    }
    let Some((name, value)) = option.split_once('=') else {
        // A value flag without its `=<value>` (`-XX:ZUncommitDelay`): local
        // 25.0.3 prints `Improperly specified VM option 'ZUncommitDelay'`.
        let bare = matches!(
            option,
            "ZAllocationSpikeTolerance"
                | "ZCollectionInterval"
                | "ZCollectionIntervalMajor"
                | "ZCollectionIntervalMinor"
                | "ZUncommitDelay"
                | "SoftMaxHeapSize"
        );
        // zt t23/hunt4: the two limit flags, under their own switch.
        let bare = bare
            || (rules.limit_flags
                && matches!(option, "ZFragmentationLimit" | "ZYoungCompactionLimit"));
        return (zgc_flags && bare).then(|| ZgcFlagRefusal::Improper(option.to_string()));
    };
    let refused = match name {
        "ZAllocationSpikeTolerance"
        | "ZCollectionInterval"
        | "ZCollectionIntervalMajor"
        | "ZCollectionIntervalMinor" => {
            zgc_flags && rules.double_syntax && hotspot_parse_double_flag(value).is_none()
        }
        "ZUncommitDelay" | "SoftMaxHeapSize" => {
            zgc_flags && hotspot_parse_uintx_flag(value).is_none()
        }
        // zt t23/hunt4 (`CRATONVM_LAUNCHER_ZGC_LIMIT_FLAG_SYNTAX`).
        "ZFragmentationLimit" | "ZYoungCompactionLimit" if zgc_flags && rules.limit_flags => {
            match hotspot_parse_double_flag(value) {
                None => true,
                // zt t24/hunt7 (`CRATONVM_LAUNCHER_ZGC_LIMIT_FLAG_RANGE`).
                Some(v) if rules.limit_range => {
                    if let Some(lines) = zgc_limit_flag_range_refusal(name, option, v) {
                        return Some(ZgcFlagRefusal::Lines(lines));
                    }
                    false
                }
                Some(_) => false,
            }
        }
        "ParallelGCThreads" | "ConcGCThreads" => {
            rules.zgc && rules.gc_threads && hotspot_parse_uint_flag(value).is_none()
        }
        _ => false,
    };
    refused.then(|| ZgcFlagRefusal::Improper(option.to_string()))
}

/// zt t9/tg2 (`zgc-zt8x5-wave-t7-review-residuals-CLOSED-20261019.md` items 6, 7):
/// the FIRST refusal in command-line order over the launcher section of
/// `argv` (everything before `--`), as HotSpot 25 reports it: it parses the
/// options left to right and stops at the first bad one, even one a later
/// occurrence would override (local 25.0.3: `-XX:ZCollectionInterval=abc
/// -XX:ZProactive=false` names `ZCollectionInterval=abc`, the reverse order
/// `Missing +/- setting for VM option 'ZProactive=false'`;
/// `-XX:ZCollectionInterval=abc -XX:ZCollectionInterval=1` is refused).
/// `run` checked the sign-less booleans first, then each `double` flag's
/// LAST value, so it named whichever arm ran first. Pure.
pub(crate) fn scan_first_zgc_flag_refusal(
    argv: &[String],
    rules: ZgcFlagRefusalRules,
) -> Option<ZgcFlagRefusal> {
    argv.iter()
        .skip(1)
        .take_while(|a| a.as_str() != "--")
        .find_map(|a| zgc_flag_refusal_of(a, rules))
}

/// `CRATONVM_LAUNCHER_ORDERED_HEAP_FLAG_REFUSALS` (zt t10/tg3,
/// `zgc-zt9tg2-timer-and-launcher-residuals-SUPERSEDED-20261019.md` item 5; default ON,
/// `=0` kill). The parse-time refusals of the tenuring flags
/// (`MaxTenuringThreshold`, `InitialTenuringThreshold`,
/// `TargetSurvivorRatio`) and of `SurvivorRatio` / the TLAB value flags join
/// the ordered first-refusal scan ([`scan_first_ordered_vm_option_refusal`]),
/// so the first bad option in command-line order is the one named, as
/// HotSpot 25.0.3 names it: `-XX:MaxTenuringThreshold=x
/// -XX:ZProactive=false` -> `Improperly specified VM option
/// 'MaxTenuringThreshold=x'` (the scan named `ZProactive=false`, which runs
/// first); `-XX:SurvivorRatio=0 -XX:ZProactive=false` -> the range line and
/// `Improperly specified VM option 'SurvivorRatio=0'`. The lines are the
/// arms' own ([`tenuring_config_from_flags`],
/// [`space_tlab_flags_from_hotspot_flags`]), and those arms stay as the
/// kill switch's behaviour. These refusals also end a `-version` launch now,
/// as HotSpot's do (the arms run in the parsed path, after the banner).
/// Option parsing does not depend on the collector, so this acts on every
/// collector. Acts only under [`launcher_zgc_flag_hotspot_parse_enabled`].
pub(crate) fn launcher_ordered_heap_flag_refusals_enabled() -> bool {
    true
}

/// zt t10/tg3: HotSpot's parse-time refusal lines of ONE tenuring,
/// `SurvivorRatio` or TLAB value argument (`-XX:<Name>=<raw>`), as the
/// launcher's own arms print them, or `None` when HotSpot takes it (or it is
/// none of those flags). `space_tlab` is `CRATONVM_LAUNCHER_SPACE_TLAB_FLAGS`
/// (its `=0` turns that family off here as in `run`). Only the parse-time
/// checks: the `AfterMemoryInit` constraints (`space_tlab_constraint_errors`)
/// read the heap geometry and stay in `run`. Pure over the switches passed.
pub(crate) fn heap_flag_parse_refusal_of(arg: &str, space_tlab: bool) -> Option<Vec<String>> {
    let mut flags = HotspotFlags::default();
    let tenuring = [
        "-XX:MaxTenuringThreshold=",
        "-XX:InitialTenuringThreshold=",
        "-XX:TargetSurvivorRatio=",
    ];
    if let Some((which, raw)) = tenuring
        .iter()
        .enumerate()
        .find_map(|(i, prefix)| arg.strip_prefix(prefix).map(|raw| (i as u8, raw)))
    {
        flags.tenuring_values_in_order.push((which, raw.to_string()));
        return tenuring_config_from_flags(&flags).err();
    }
    if !space_tlab {
        return None;
    }
    let (name, raw) = space_tlab_value_flag(arg)?;
    flags.space_tlab_values_in_order.push((name, raw.to_string()));
    space_tlab_flags_from_hotspot_flags(&flags).err()
}

/// zt t10/tg3: HotSpot's refusal lines, then the two `Error:` lines, on
/// stderr; exit status 1 (as `run`'s tenuring and TLAB arms end the launch).
pub(crate) fn exit_with_vm_option_lines(lines: &[String]) -> ! {
    use std::io::Write as _;
    let mut err = std::io::stderr().lock();
    for line in lines {
        let _ = writeln!(err, "{line}");
    }
    let _ = writeln!(err, "Error: Could not create the Java Virtual Machine.");
    let _ = writeln!(
        err,
        "Error: A fatal exception has occurred. Program will exit."
    );
    let _ = err.flush();
    std::process::exit(1);
}

/// zt t10/tg3: the first refusal of the ordered launcher scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OrderedLauncherRefusal {
    /// A ZGC flag ([`zgc_flag_refusal_of`]).
    Zgc(ZgcFlagRefusal),
    /// A tenuring / `SurvivorRatio` / TLAB value
    /// ([`heap_flag_parse_refusal_of`]): HotSpot's lines. zt t18/tg8: also
    /// an `-Xmx` / `-XX:MaxHeapSize=` / `-Xms` / `-Xss` value
    /// ([`memory_size_option_refusal_of`]).
    HeapFlag(Vec<String>),
    /// zt t18/tg8: a `--patch-module` value without `=`
    /// ([`PatchModuleRefusal::MissingEquals`]).
    PatchModuleMissingEquals,
    /// zt t20/tg9: the second `--patch-module` value for `java.base`
    /// ([`PatchModuleRefusal::JavaBaseTwice`]).
    PatchModuleJavaBaseTwice,
}

impl OrderedLauncherRefusal {
    pub(crate) fn exit(&self) -> ! {
        match self {
            OrderedLauncherRefusal::Zgc(refusal) => refusal.exit(),
            OrderedLauncherRefusal::HeapFlag(lines) => exit_with_vm_option_lines(lines),
            OrderedLauncherRefusal::PatchModuleMissingEquals => {
                exit_with_patch_module_refusal(&PatchModuleRefusal::MissingEquals)
            }
            OrderedLauncherRefusal::PatchModuleJavaBaseTwice => {
                exit_with_patch_module_refusal(&PatchModuleRefusal::JavaBaseTwice)
            }
        }
    }
}

/// zt t18/tg8: the families [`scan_first_ordered_vm_option_refusal`] adds
/// to the t10 / t11 ordered scan, each under its own switch.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct OrderedVmOptionRules {
    /// `CRATONVM_LAUNCHER_MEMORY_SIZE_OPTIONS`
    /// ([`memory_size_option_refusal_of`]).
    pub(crate) memory_sizes: bool,
    /// `CRATONVM_LAUNCHER_PATCH_MODULE_REFUSALS` under
    /// `CRATONVM_LAUNCHER_VM_OPTION_PARSE_ORDER`: a `--patch-module=<v>` or
    /// `--patch-module <v>` whose value has no `=`.
    pub(crate) patch_module_missing_equals: bool,
    /// zt t20/tg9: with `patch_module_missing_equals`, also the second
    /// `java.base` value (`CRATONVM_LAUNCHER_PATCH_MODULE_JAVA_BASE_TWICE`).
    pub(crate) patch_module_java_base_twice: bool,
    /// zt t21/tg10: `-Xmn<size>` (`CRATONVM_LAUNCHER_XMN_REFUSAL` under
    /// `memory_sizes`; [`young_size_option_refusal_of`]).
    pub(crate) young_size: bool,
    /// zt-dev merge (gcj j1/ob, j2/ob2): under
    /// `CRATONVM_GEN_HOTSPOT_VALUE_FLAGS` (on every collector since dev
    /// cabdb9f4ae; zt t24/m5), the typed `-XX:` value table
    /// ([`gen_value_flag_refusal_of`]) in the same command-line order, so a
    /// Generational value refusal and a ZGC-flag / heap-flag / memory-size
    /// refusal are not checked in two passes (HotSpot stops at the first bad
    /// option whatever its group).
    pub(crate) gen_values: bool,
    /// With `gen_values`: `CRATONVM_GEN_ONE_ORDERED_VALUE_PARSE`'s
    /// syntax-only rows.
    pub(crate) gen_extra: bool,
    /// With `gen_values`: `CRATONVM_GEN_HOTSPOT_VALUE_RANGES`' captured
    /// ranges and `bool` refusals.
    pub(crate) gen_ranges: bool,
    /// zt-dev merge (dev 7dae32b9f9): on an effective G1 command line a
    /// malformed `-XX:ParallelGCThreads=` / `-XX:ConcGCThreads=` value
    /// ([`malformed_gc_thread_option`]) in the same order (HotSpot's option
    /// parse does not depend on the collector; the ZGC arm is
    /// `ZgcFlagRefusalRules::gc_threads`). No switch, as on dev.
    pub(crate) g1_gc_threads: bool,
    /// With `gen_values`: `CRATONVM_LAUNCHER_HOTSPOT_FLAG_TABLE`, every
    /// option HotSpot 25's flag table knows
    /// ([`hotspot_flag_table_arg_errors`], which answers the typed rows of
    /// `gen_values` first).
    pub(crate) flag_table: bool,
    /// With `flag_table`: `CRATONVM_LAUNCHER_HOTSPOT_FLAG_LOCKS`.
    pub(crate) flag_locks: bool,
}

impl OrderedVmOptionRules {
    /// zt-dev merge: the gcj value-table rules (every collector since dev
    /// cabdb9f4ae) and the G1 GC thread-count arm for the launcher section
    /// of `argv`, from their switches.
    pub(crate) fn with_dev_collector_rules(self, argv: &[String]) -> OrderedVmOptionRules {
        OrderedVmOptionRules {
            g1_gc_threads: scan_effective_g1(argv),
            gen_values: true,
            gen_extra: true,
            gen_ranges: true,
            flag_table: launcher_hotspot_flag_table_enabled(),
            flag_locks: launcher_hotspot_flag_locks_enabled(),
            ..self
        }
    }
}

/// zt t18/tg8: `scan_first_ordered_launcher_refusal` (the spelling the
/// t10 / t11 tests keep) widened by [`OrderedVmOptionRules`]: the memory
/// size options and `--patch-module`'s `Missing '='` take their place in
/// command-line order. With every `more` rule off it is exactly the t11
/// scan. Pure.
pub(crate) fn scan_first_ordered_vm_option_refusal(
    argv: &[String],
    rules: ZgcFlagRefusalRules,
    heap_flags: bool,
    space_tlab: bool,
    more: OrderedVmOptionRules,
) -> Option<OrderedLauncherRefusal> {
    let gc_values = rules.zgc && rules.gc_values;
    if !heap_flags
        && !gc_values
        && !more.memory_sizes
        && !more.patch_module_missing_equals
        && !more.gen_values
        && !more.g1_gc_threads
        && !more.flag_table
    {
        return scan_first_zgc_flag_refusal(argv, rules).map(OrderedLauncherRefusal::Zgc);
    }
    let section: Vec<&String> = argv
        .iter()
        .skip(1)
        .take_while(|a| a.as_str() != "--")
        .collect();
    // zt t20/tg9: a `java.base` value already met (the VM parse's own state).
    let mut java_base_seen = false;
    // The unlocks read so far (`more.flag_table`).
    let mut unlocks = HotspotFlagUnlocks::for_argv(argv);
    section.iter().enumerate().find_map(|(k, a)| {
        let a = a.as_str();
        if more.patch_module_missing_equals {
            // libjli hands the VM `--patch-module=<v>` for both spellings
            // (`AddLongFormOption`), at the option's own place.
            let value = match a.strip_prefix("--patch-module=") {
                Some(v) => Some(v),
                None if a == "--patch-module" => section.get(k + 1).map(|v| v.as_str()),
                None => None,
            };
            if value.is_some_and(|v| !v.contains('=')) {
                return Some(OrderedLauncherRefusal::PatchModuleMissingEquals);
            }
            if more.patch_module_java_base_twice
                && value.and_then(|v| v.split_once('=')).is_some_and(|(m, _)| m == "java.base")
            {
                if java_base_seen {
                    return Some(OrderedLauncherRefusal::PatchModuleJavaBaseTwice);
                }
                java_base_seen = true;
            }
        }
        if let Some(refusal) = zgc_flag_refusal_of(a, rules) {
            return Some(OrderedLauncherRefusal::Zgc(refusal));
        }
        // zt-dev merge: G1's malformed GC thread counts (dev 7dae32b9f9).
        if more.g1_gc_threads {
            if let Some(option) = malformed_gc_thread_option(a) {
                return Some(OrderedLauncherRefusal::Zgc(ZgcFlagRefusal::Improper(option)));
            }
        }
        // `CRATONVM_LAUNCHER_HOTSPOT_FLAG_LOCKS`: a locked flag's lock lines
        // come before any value answer (HotSpot checks the lock first).
        if more.flag_table && hotspot_flag_is_locked(a, &unlocks, more.flag_locks) {
            let mut probe = unlocks;
            return hotspot_flag_table_arg_errors(
                a,
                &mut probe,
                space_tlab,
                more.gen_extra,
                more.gen_ranges,
                more.flag_locks,
            )
            .map(OrderedLauncherRefusal::HeapFlag);
        }
        // zt-dev merge: the gcj Generational typed value table, ahead of the
        // tenuring / TLAB arm below (its lines are the j1 / j2a rows').
        if more.gen_values {
            if let Some(lines) =
                gen_value_flag_refusal_of(a, space_tlab, more.gen_extra, more.gen_ranges)
            {
                return Some(OrderedLauncherRefusal::HeapFlag(lines));
            }
        }
        if gc_values {
            if let Some(lines) = gc_value_flag_parse_refusal_of(a) {
                return Some(OrderedLauncherRefusal::HeapFlag(lines));
            }
        }
        if more.memory_sizes {
            if let Some(lines) = memory_size_option_refusal_of(a) {
                return Some(OrderedLauncherRefusal::HeapFlag(lines));
            }
        }
        if more.young_size {
            if let Some(lines) = young_size_option_refusal_of(a) {
                return Some(OrderedLauncherRefusal::HeapFlag(lines));
            }
        }
        if heap_flags {
            if let Some(lines) = heap_flag_parse_refusal_of(a, space_tlab) {
                return Some(OrderedLauncherRefusal::HeapFlag(lines));
            }
        }
        // `CRATONVM_LAUNCHER_HOTSPOT_FLAG_TABLE`: every other option HotSpot
        // 25's table knows, after the arms above (their texts are HotSpot's
        // special cases); it also reads the unlocks.
        if more.flag_table {
            return hotspot_flag_table_arg_errors(
                a,
                &mut unlocks,
                space_tlab,
                more.gen_extra,
                more.gen_ranges,
                more.flag_locks,
            )
            .map(OrderedLauncherRefusal::HeapFlag);
        }
        None
    })
}

/// Is `a` a flag of HotSpot 25's table that is still locked given `unlocks`
/// (and `locks`, `CRATONVM_LAUNCHER_HOTSPOT_FLAG_LOCKS`)? The two unlock
/// flags never are. Pure.
pub(crate) fn hotspot_flag_is_locked(a: &str, unlocks: &HotspotFlagUnlocks, locks: bool) -> bool {
    use hotspot25_flags::L;
    let Some(rest) = a.strip_prefix("-XX:") else {
        return false;
    };
    let (name, _) = xx_option_spelling(rest);
    if !locks || matches!(name, "UnlockDiagnosticVMOptions" | "UnlockExperimentalVMOptions") {
        return false;
    }
    match hotspot25_os_flags::lookup(name).map(|f| f.lock) {
        Some(L::Diagnostic) => !unlocks.diagnostic,
        Some(L::Experimental) => !unlocks.experimental,
        _ => false,
    }
}

/// `CRATONVM_LAUNCHER_ZGC_GC_VALUE_FLAG_REFUSALS` (zt t11/tg4,
/// `zgc-zt10tg3-*` item 5; default ON, `=0` kill). On ZGC (explicit or
/// default), the parse-time refusals of `-XX:MaxGCPauseMillis`,
/// `-XX:InitiatingHeapOccupancyPercent`, `-XX:G1HeapRegionSize`,
/// `-XX:MinHeapFreeRatio` and `-XX:MaxHeapFreeRatio` join the ordered
/// first-refusal scan ([`scan_first_ordered_vm_option_refusal`]), with
/// HotSpot's lines, and the G1 arms of `run` stop warning about a value
/// HotSpot 25 takes. Local 25.0.3 (`-XX:+UseZGC`): `-XX:MaxGCPauseMillis=x
/// -XX:ZProactive=false` names `MaxGCPauseMillis=x`;
/// `-XX:InitiatingHeapOccupancyPercent=1k` prints `uint
/// InitiatingHeapOccupancyPercent=1024 is outside the allowed range [ 0 ...
/// 100 ]` and the `Improperly specified` line; `-XX:MaxGCPauseMillis=3k`,
/// `-XX:InitiatingHeapOccupancyPercent=0x10` and `-XX:G1HeapRegionSize=1m`
/// boot silently. This launcher warned and ignored the malformed ones (the
/// G1 knobs' lenient arms), refused the free ratios only after every other
/// arm, and warned about `=3k` / `=0x10` / `=0`. The Min / Max free-ratio
/// cross-check stays in `run` (HotSpot checks it after the parse: local
/// `-XX:MinHeapFreeRatio=80 -XX:ZProactive=false` names `ZProactive`).
/// Only under [`launcher_zgc_flag_hotspot_parse_enabled`]; off ZGC
/// nothing changes. `=0`: those arms as before.
pub(crate) fn launcher_zgc_gc_value_flag_refusals_enabled() -> bool {
    true
}

/// zt t11/tg4: HotSpot 25.0.3's parse-time refusal lines of ONE GC value
/// argument (`-XX:<Name>=<raw>`, or the bare `-XX:<Name>`), or `None` when
/// HotSpot takes it (or it is not one of these flags): a value outside the
/// flag type's syntax ([`hotspot_parse_uint_flag`] for the `uint`,
/// [`hotspot_parse_uintx_flag`] for the `uintx` / `size_t` ones) or a bare
/// name is `Improperly specified VM option '<option>'`; a value outside the
/// flag's range prints `<type> <Name>=<value> is outside the allowed range [
/// <lo> ... <hi> ]` first (local rows in
/// [`launcher_zgc_gc_value_flag_refusals_enabled`]). Pure.
pub(crate) fn gc_value_flag_parse_refusal_of(arg: &str) -> Option<Vec<String>> {
    // (name, HotSpot type, range), as local 25.0.3 prints them in its
    // range lines.
    // zt t28/mf: `HeapDumpGzipLevel` (`int`, `range(0, 9)`; HotSpot 25.0.4
    // `-XX:HeapDumpGzipLevel=12`: `int HeapDumpGzipLevel=12 is outside the
    // allowed range [ 0 ... 9 ]`), parsed as the `uint` ones (a negative
    // value is `Improperly specified` here).
    const FLAGS: [(&str, &str, u64, u64); 6] = [
        ("MaxGCPauseMillis", "uintx", 1, u64::MAX - 1),
        ("InitiatingHeapOccupancyPercent", "uint", 0, 100),
        ("G1HeapRegionSize", "size_t", 0, HOTSPOT_G1_HEAP_REGION_SIZE_MAX as u64),
        ("MinHeapFreeRatio", "uintx", 0, 100),
        ("MaxHeapFreeRatio", "uintx", 0, 100),
        ("HeapDumpGzipLevel", "int", 0, 9),
    ];
    let option = arg.strip_prefix("-XX:")?;
    let (name, value) = match option.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (option, None),
    };
    let &(_, kind, lo, hi) = FLAGS.iter().find(|flag| flag.0 == name)?;
    let improper = format!("Improperly specified VM option '{option}'");
    let Some(value) = value else {
        return Some(vec![improper]);
    };
    let parsed = if kind == "uint" || kind == "int" {
        hotspot_parse_uint_flag(value).map(u64::from)
    } else {
        hotspot_parse_uintx_flag(value)
    };
    let Some(v) = parsed else {
        return Some(vec![improper]);
    };
    if v < lo || v > hi {
        return Some(vec![
            format!("{kind} {name}={v} is outside the allowed range [ {lo} ... {hi} ]"),
            improper,
        ]);
    }
    None
}

/// zt t11/tg4: does HotSpot 25 take `raw` for the GC value flag `name`
/// ([`gc_value_flag_parse_refusal_of`] finds nothing to refuse)? For the G1
/// arms of `run`, which on ZGC stop warning about such a value under
/// [`launcher_zgc_gc_value_flag_refusals_enabled`]. Pure.
pub(crate) fn hotspot_takes_gc_value_flag(name: &str, raw: &str) -> bool {
    gc_value_flag_parse_refusal_of(&format!("-XX:{name}={raw}")).is_none()
}

/// `CRATONVM_LAUNCHER_SOFT_MAX_ABOVE_MAX_REFUSED` (zt t10/tg3,
/// `zgc-zt9tg2-*` item 6; default ON, `=0` kill: the value is clamped to
/// the maximum heap and the VM boots, as before). HotSpot 25.0.3 refuses
/// a `-XX:SoftMaxHeapSize` above the (aligned) maximum heap on every
/// collector (`SoftMaxHeapSizeConstraintFunc`, after ergonomics): local
/// `-XX:+UseZGC -Xmx64m -XX:SoftMaxHeapSize=128m` (also `-XX:+UseG1GC`,
/// `-XX:+UseSerialGC`, `-XX:SoftMaxHeapSize=64g` against the ergonomic
/// maximum) prints `SoftMaxHeapSize must be less than or equal to the
/// maximum heap size` and the two `Error:` lines on stderr, exit 1;
/// `-Xmx9m -XX:SoftMaxHeapSize=10m` boots (the maximum is aligned to 10M).
/// zt t28/sm: does HotSpot's ZGC ergonomic soft maximum apply -- the switch
/// (`CRATONVM_ZGC_SOFT_MAX_ERGONOMIC`, default ON) is on and no option of
/// `jvm_args` sizes the maximum heap by RAM (`-XX:MaxRAMPercentage`,
/// `-XX:MaxRAMFraction`) or names `MaxHeapSize` / `SoftMaxHeapSize`? The
/// caller checks `-Xmx` (its parsed field). Pure but for the switch.
pub(crate) fn zgc_soft_max_ergonomic_applies(jvm_args: &[String]) -> bool {
    !jvm_args.iter().any(|arg| {
            ["-XX:MaxRAMPercentage=", "-XX:MaxRAMFraction=", "-XX:MaxHeapSize=", "-XX:SoftMaxHeapSize="]
                .iter()
                .any(|prefix| arg.starts_with(prefix))
                || arg.starts_with("-Xmx")
        })
}

/// zt t28/sm: `MaxHeapSize * 90 / 100`, in HotSpot's order (multiply first,
/// `size_t` arithmetic). Pure.
pub(crate) fn hotspot_ergonomic_soft_max(max_heap: usize) -> usize {
    usize::try_from(max_heap as u128 * 90 / 100).unwrap_or(max_heap)
}



/// zt t10/tg3: HotSpot's `SoftMaxHeapSizeConstraintFunc`: `soft` above
/// the maximum heap, `max_heap` aligned up to `alignment` (the collector's
/// `HeapAlignment`; pass 1 for an already aligned maximum). Pure.
pub(crate) fn soft_max_heap_size_refused(soft: u64, max_heap: usize, alignment: usize) -> bool {
    let max = hotspot_heap_align_up_to(max_heap, alignment);
    soft > u64::try_from(max).unwrap_or(u64::MAX)
}

/// zt t11/tg4 (`t11-x8-report.md` C12 / T1): the launcher's main-thread
/// exit arms' director stop (main returned, the `-version` boot): stop the
/// ZGC director tick, then, under `CRATONVM_ZGC_TICK_STOP_WAITS` (default
/// ON, `=0` kill), wait for a tick pause that was already past its last stop
/// check, so its `GC(n)` lines print before the `gc,exit` summary, as
/// HotSpot's director stop waits for its running cycle. The wait runs in the
/// main thread's blocked region (the pause must not wait for this thread)
/// and is bounded (`wait_for_zgc_tick_pause`). A no-op off ZGC and without a
/// tick. The `System.exit` arm stops without waiting.
pub(crate) fn stop_zgc_director_tick_and_wait(vm: &mut Vm) {
    cratonvm_vm::runtime::interpreter::stop_zgc_director_tick(&vm.shared);
    // zt t31/x10: G1's periodic service stops with it.
    cratonvm_vm::runtime::interpreter::stop_g1_periodic_gc_service(&vm.shared);
    if cratonvm_vm::runtime::interpreter::zgc_tick_stop_waits_enabled()
        && cratonvm_vm::runtime::interpreter::zgc_tick_pause_in_flight(&vm.shared)
    {
        vm.begin_main_thread_blocking_region("vm-main:zgc-director-stop");
        let _ = cratonvm_vm::runtime::interpreter::wait_for_zgc_tick_pause(&vm.shared);
        vm.end_main_thread_blocking_region();
    }
}

/// zt t12/x9: whether the launcher publishes the perf-data file. `env_on` is
/// `CRATONVM_PERF_DATA_FILE` (default ON; `VmConfig::perf_data_file`), which
/// the HotSpot flags cannot turn back on: `-XX:-UsePerfData` and
/// `-XX:+PerfDisableSharedMem` turn the file off, as on HotSpot, and their
/// opposites only leave the switch's answer. Before, either flag present
/// replaced the switch, so `CRATONVM_PERF_DATA_FILE=0 -XX:+UsePerfData`
/// published a file. Pure.
pub(crate) fn launcher_perf_data_file(
    env_on: bool,
    use_perf_data: Option<bool>,
    perf_disable_shared_mem: Option<bool>,
) -> bool {
    env_on && use_perf_data.unwrap_or(true) && !perf_disable_shared_mem.unwrap_or(false)
}

/// zt t10/tg3: HotSpot's constraint failure for `-XX:SoftMaxHeapSize`.
pub(crate) fn exit_with_soft_max_above_max_heap() -> ! {
    exit_with_vm_option_lines(&[
        "SoftMaxHeapSize must be less than or equal to the maximum heap size".to_string(),
    ])
}

/// zt t10/tg3: the version path's [`soft_max_heap_size_refused`], from the
/// launcher section of `argv` (a `-version` launch without `-Xlog` never
/// reaches `run`'s check). The last `-XX:SoftMaxHeapSize` in HotSpot's
/// syntax; the last `-Xmx` / `-XX:MaxHeapSize` ([`early_max_heap`]) or, with
/// none, the launcher's ergonomic maximum (the container limit unless
/// `-XX:-UseContainerSupport`), aligned as `run` aligns it: 2 MiB, or the G1
/// alignment when the last selector names G1 (`-XX:G1HeapRegionSize`
/// honoured). `false` when nothing is given, the value does not parse, or no
/// maximum is known.
pub(crate) fn scan_soft_max_above_max_heap(argv: &[String]) -> bool {
    let launcher: Vec<&String> = argv
        .iter()
        .skip(1)
        .take_while(|a| a.as_str() != "--")
        .collect();
    let Some(soft) = launcher
        .iter()
        .rev()
        .find_map(|a| a.strip_prefix("-XX:SoftMaxHeapSize="))
        .and_then(hotspot_parse_uintx_flag)
    else {
        return false;
    };
    let max = match early_max_heap(argv) {
        Some((size, _)) => size,
        // zt t11/tg4 (`t11-x8-report.md` C13 / T2): a maximum-heap option
        // this scan cannot read (`-Xmx1t`) is not "no maximum": refuse
        // nothing rather than check against the ergonomic one.
        None if launcher.iter().any(|a| is_max_heap_option(a)) => return false,
        None => {
            let container = if launcher.iter().any(|a| a.as_str() == "-XX:-UseContainerSupport") {
                None
            } else {
                cratonvm_vm::runtime::container::detect_container().memory_limit
            };
            let Some(ergo) = ergonomic_default_max_heap(container) else {
                return false;
            };
            ergo
        }
    };
    let g1 = scan_selected_gc_name(argv)
        .and_then(cratonvm_vm::config::parse_gc_algorithm)
        .is_some_and(|algo| algo == cratonvm_vm::config::GcAlgorithm::G1);
    let alignment = if g1 {
        let region = launcher
            .iter()
            .rev()
            .find_map(|a| a.strip_prefix("-XX:G1HeapRegionSize="))
            .and_then(|s| g1_heap_region_size_from_flag(s).ok().flatten());
        hotspot_heap_alignment(cratonvm_vm::config::GcAlgorithm::G1, max, region)
            .unwrap_or(HOTSPOT_SERIAL_HEAP_ALIGNMENT)
    } else {
        HOTSPOT_SERIAL_HEAP_ALIGNMENT
    };
    soft_max_heap_size_refused(soft, max, alignment)
}

/// gcj j1/ob (`launcher-gc-thread-flag-and-xx-value-refusals-on-g1-and-generational-FIXED-20261022`
/// item 3): HotSpot 25's declared type of a `-XX:<Name>=<value>` flag, which
/// decides how `Arguments::parse_argument` reads the value and the type name
/// a range line prints (`uintx SurvivorRatio=0 is outside the allowed range
/// [ 1 ... 18446744073709551613 ]`). `bool` flags take `-XX:+<Name>` /
/// `-XX:-<Name>` and are not in the table: what HotSpot 25 answers to a
/// `-XX:<BoolFlag>=<value>` spelling has no local row yet (residual page
/// `gcj-j1ob-launcher-typed-value-residuals-FIXED-20261022.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HotspotFlagType {
    Int,
    Intx,
    Uint,
    Uintx,
    Uint64,
    SizeT,
    Double,
}

impl HotspotFlagType {
    /// The type name HotSpot's range checker prints.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Int => "int",
            Self::Intx => "intx",
            Self::Uint => "uint",
            Self::Uintx => "uintx",
            Self::Uint64 => "uint64_t",
            Self::SizeT => "size_t",
            Self::Double => "double",
        }
    }

    /// `raw` read as HotSpot 25 reads a value of this type: the integer
    /// (`parse_integer<T>`: [`hotspot_parse_uintx`]'s syntax -- decimal or
    /// `0x` hex, one optional `k`/`m`/`g`/`t` multiplier, no blank -- within
    /// the type, a leading `-` for the signed types only), or for a `double`
    /// [`hotspot_parse_double_flag`]'s rule (answered as `0`: no double flag
    /// of the table carries a range). `None` is HotSpot's `Improperly
    /// specified VM option`. Pure.
    pub(crate) fn parse(self, raw: &str) -> Option<i128> {
        match self {
            Self::Double => hotspot_parse_double_flag(raw).map(|_| 0),
            Self::Uint => hotspot_parse_uintx(raw)
                .filter(|&v| v <= u64::from(u32::MAX))
                .map(i128::from),
            Self::Uintx | Self::Uint64 | Self::SizeT => hotspot_parse_uintx(raw).map(i128::from),
            Self::Int | Self::Intx => {
                let (negative, body) = match raw.strip_prefix('-') {
                    Some(rest) => (true, rest),
                    None => (false, raw),
                };
                let magnitude = i128::from(hotspot_parse_uintx(body)?);
                let v = if negative { -magnitude } else { magnitude };
                let (lo, hi) = if self == Self::Int {
                    (i128::from(i32::MIN), i128::from(i32::MAX))
                } else {
                    (i128::from(i64::MIN), i128::from(i64::MAX))
                };
                (lo..=hi).contains(&v).then_some(v)
            }
        }
    }
}

/// gcj j1/ob (item 3): one typed table of the `-XX:<Name>=<value>` options
/// this launcher takes a value for, each with HotSpot 25's declared type,
/// the `range(lo, hi)` where an earlier lane verified it on the local 25.0.3
/// (the tenuring flags, gci i23/tn + i47w/ob47; the heap free ratios, gcg
/// g3/x; `G1HeapRegionSize`, gci i11/gm; `DiagnoseSyncOnValueBasedClasses`,
/// i5 wave 102), and whether HotSpot parses it in `parse_each_vm_init_arg`
/// itself (`MaxTenuringThreshold`: a range failure prints the range line
/// alone, its text is read as a `julong`). `SurvivorRatio` and the TLAB
/// value flags are [`SPACE_TLAB_VALUE_FLAGS`] (looked up there).
///
/// Ranges not captured in the table are syntax-checked only. The typed
/// syntax/range scan is collector-independent; collector-specific option
/// normalization remains scoped to Generational.
pub(crate) const GEN_HOTSPOT_VALUE_FLAGS: &[(&str, HotspotFlagType, Option<(i128, i128)>, bool)] = &[
    ("MaxTenuringThreshold", HotspotFlagType::Uint, Some((0, 16)), true),
    ("InitialTenuringThreshold", HotspotFlagType::Uint, Some((0, 16)), false),
    ("TargetSurvivorRatio", HotspotFlagType::Uint, Some((0, 100)), false),
    ("MinHeapFreeRatio", HotspotFlagType::Uintx, Some((0, 100)), false),
    ("MaxHeapFreeRatio", HotspotFlagType::Uintx, Some((0, 100)), false),
    (
        "G1HeapRegionSize",
        HotspotFlagType::SizeT,
        Some((0, HOTSPOT_G1_HEAP_REGION_SIZE_MAX as i128)),
        false,
    ),
    ("DiagnoseSyncOnValueBasedClasses", HotspotFlagType::Int, Some((0, 2)), false),
    ("ParallelGCThreads", HotspotFlagType::Uint, None, false),
    ("ConcGCThreads", HotspotFlagType::Uint, None, false),
    ("MaxGCPauseMillis", HotspotFlagType::Uintx, None, false),
    // gcj j2/x1 (item 9): `uint` in HotSpot 25 (`-XX:+PrintFlagsFinal` on
    // 25.0.4.1), not `uintx`: a value of 2^32 or more is improperly specified.
    (
        "InitiatingHeapOccupancyPercent",
        HotspotFlagType::Uint,
        None,
        false,
    ),
    ("G1HeapWastePercent", HotspotFlagType::Uint, None, false),
    ("SoftMaxHeapSize", HotspotFlagType::SizeT, None, false),
    ("MaxDirectMemorySize", HotspotFlagType::Uint64, None, false),
    ("MaxJavaStackTraceDepth", HotspotFlagType::Intx, None, false),
    ("SoftRefLRUPolicyMSPerMB", HotspotFlagType::Intx, None, false),
    ("MaxMetaspaceSize", HotspotFlagType::SizeT, None, false),
    ("ZAllocationSpikeTolerance", HotspotFlagType::Double, None, false),
    ("ZUncommitDelay", HotspotFlagType::Uintx, None, false),
];

/// gcj j1/ob: `(type, range, own parser)` of `name` in
/// [`GEN_HOTSPOT_VALUE_FLAGS`], else (when `space_tlab`, i.e.
/// `CRATONVM_LAUNCHER_SPACE_TLAB_FLAGS` is on) in [`SPACE_TLAB_VALUE_FLAGS`].
pub(crate) fn hotspot_value_flag_entry(
    name: &str,
    space_tlab: bool,
) -> Option<(HotspotFlagType, Option<(i128, i128)>, bool)> {
    let typed = GEN_HOTSPOT_VALUE_FLAGS.iter().find(|(n, ..)| *n == name);
    if let Some(&(_, ty, range, own_parser)) = typed {
        return Some((ty, range, own_parser));
    }
    if !space_tlab {
        return None;
    }
    SPACE_TLAB_VALUE_FLAGS
        .iter()
        .find(|(n, ..)| *n == name)
        .map(|&(_, ty, lo, hi)| {
            let ty = if ty == "size_t" {
                HotspotFlagType::SizeT
            } else {
                HotspotFlagType::Uintx
            };
            (ty, Some((i128::from(lo), i128::from(hi))), false)
        })
}

/// gcj j1/ob: HotSpot 25's lines for one `-XX:<name>=<raw>` of type `ty`, or
/// `None` when HotSpot takes it: the `Improperly specified VM option
/// '<name>=<raw>'` line for a value its type does not parse; for a number
/// outside `range`, the range line followed by that line (the range line
/// alone for an `own_parser` flag, whose text is read as a `julong`). Pure.
pub(crate) fn hotspot_value_flag_errors(
    name: &str,
    raw: &str,
    ty: HotspotFlagType,
    range: Option<(i128, i128)>,
    own_parser: bool,
) -> Option<Vec<String>> {
    let improper = format!("Improperly specified VM option '{name}={raw}'");
    // gcj j2/x1 (item 10): the own parser reads a `julong`, and the flag's
    // `set` then refuses a value its `uint` cannot hold -- `Improperly
    // specified`, no range line (`MaxTenuringThreshold=4294967296` /
    // `=0x100000001`; `=4294967295` still prints the range line; 25.0.4.1).
    let parsed = if own_parser {
        hotspot_parse_uintx(raw)
            .map(i128::from)
            .filter(|&v| ty != HotspotFlagType::Uint || v <= i128::from(u32::MAX))
    } else {
        ty.parse(raw)
    };
    let Some(v) = parsed else {
        return Some(vec![improper]);
    };
    let (lo, hi) = range?;
    if (lo..=hi).contains(&v) {
        return None;
    }
    let line = format!(
        "{} {name}={v} is outside the allowed range [ {lo} ... {hi} ]",
        ty.name()
    );
    Some(if own_parser { vec![line] } else { vec![line, improper] })
}

/// gcj j1/ob (items 2 and 3 of
/// `launcher-gc-thread-flag-and-xx-value-refusals-on-g1-and-generational-FIXED-20261022`,
/// the Generational half; `gci-i48-launcher-version-skips-vm-option-checks`
/// and item 4 of `gci-i42ta-launcher-drops-survivor-ratio-and-tlab-flags`,
/// Generational): the first `-XX:<Name>=<value>` of the launcher section of
/// `argv` (everything before `--`) that HotSpot 25 refuses while it parses
/// the options, with HotSpot's lines -- in COMMAND-LINE order across every
/// flag of the typed table, as HotSpot stops at the first bad option
/// whatever its group (the parsed path checks the tenuring group before the
/// TLAB group). Run before the version banner, as HotSpot parses the
/// options before `-version` prints. `space_tlab`:
/// `CRATONVM_LAUNCHER_SPACE_TLAB_FLAGS` (its `=0` drops those flags
/// unchecked, as before). Pure.
///
/// gcj j2/x1 (item 8): a `-XX:+<Name>` / `-XX:-<Name>` of one of these value
/// flags is HotSpot's `Unexpected +/- setting in VM option '<Name>'` (local
/// 25.0.4.1, `-XX:+MaxTenuringThreshold`), as the flag table's scan
/// ([`hotspot_flag_table_refusal`]) answers it; this table-free path let it
/// fall through.
pub(crate) fn gen_hotspot_value_flag_refusal(argv: &[String], space_tlab: bool) -> Option<Vec<String>> {
    argv.iter()
        .skip(1)
        .take_while(|a| a.as_str() != "--")
        .find_map(|a| {
            let rest = a.strip_prefix("-XX:")?;
            if let Some(name) = rest.strip_prefix(['+', '-']) {
                hotspot_value_flag_entry(name, space_tlab)?;
                return Some(vec![format!(
                    "Unexpected +/- setting in VM option '{name}'"
                )]);
            }
            let (name, raw) = rest.split_once('=')?;
            let (ty, range, own_parser) = hotspot_value_flag_entry(name, space_tlab)?;
            hotspot_value_flag_errors(name, raw, ty, range, own_parser)
        })
}

/// gcj j1/ob: does the launcher section of `argv` select the Generational
/// collector -- `-XX:+UseGenerationalGC`, `-XX:+UseSerialGC` or any other
/// collector this VM does not have (the parsed path falls back to
/// Generational for those), a `-XX:-UseZGC` / `-XX:-UseG1GC` that deselects
/// ([`minus_gc_flag_deselects`]), or no selector in a build without ZGC?
/// The last selector wins, as in `normalize_java_launcher_argv`. Pure.
///
/// gcj j2/x1 (item 11): the selection is [`scan_selected_gc_name`]'s, so the
/// internal `--XX:UseGc <Name>` spelling counts as well.
pub(crate) fn scan_effective_generational(argv: &[String]) -> bool {
    match scan_selected_gc_name(argv) {
        Some(name) => matches!(
            cratonvm_vm::config::parse_gc_algorithm(name),
            Some(cratonvm_vm::config::GcAlgorithm::Generational) | None
        ),
        None => !cfg!(feature = "zgc"),
    }
}





/// gcj j1/ob: does `-XX:+Use<sel>GC` select Generational without the
/// unsupported-collector warning? Only `Serial`, HotSpot's collector
/// Generational is the twin of, and only when `enabled`. Pure.
///
/// gcj j2/x1: exactly `Serial`, as HotSpot spells the flag. A case-folded
/// match silenced the warning for `-XX:+UseserialGC`, an option HotSpot does
/// not know; it keeps the warning (and the Generational fallback) now.
pub(crate) fn serial_selector_is_silent(sel: &str, enabled: bool) -> bool {
    enabled && sel == "Serial"
}

/// gcj j2/ob2 (proposal `gcj-j1ob-proposal-one-ordered-vm-option-parse-for-every-collector`,
/// the Generational slice; residual page `gcj-j1ob-launcher-typed-value-residuals-FIXED-20261022`
/// item 5): `-XX:` value flags the typed table did not know, which the
/// launcher either dropped in its silent catch-all (`MetaspaceSize`) or read
/// with its own lenient syntax (`parse_size`, `str::parse`) and a `Warning:
/// ignoring ...` line. Each row is HotSpot 25's declared type as read from
/// `gc_globals.hpp` / `globals.hpp`; only the value SYNTAX is checked (an
/// `Improperly specified VM option` line, whose shape the j1 rows verified),
/// never a range. Applied under `CRATONVM_GEN_ONE_ORDERED_VALUE_PARSE`
/// (default ON, `=0` kill) because the types are read from source, not from rows.
pub(crate) const GEN_ORDERED_PARSE_EXTRA_FLAGS: &[(&str, HotspotFlagType)] = &[
    ("NewSize", HotspotFlagType::SizeT),
    ("MaxNewSize", HotspotFlagType::SizeT),
    ("NewRatio", HotspotFlagType::Uintx),
    ("InitialHeapSize", HotspotFlagType::SizeT),
    ("MinHeapSize", HotspotFlagType::SizeT),
    ("MetaspaceSize", HotspotFlagType::SizeT),
];

/// gcj j2/ob2: the flags whose parsed-path consumer re-reads the value with
/// its own syntax (`parse_size` takes no `0x` and no `t`; `str::parse` takes
/// no multiplier), so a value HotSpot's parse accepted (`0x1000000`, `1t`,
/// `1k`) was dropped there with a `Warning: ignoring ...` line. Under the
/// ordered parse the launcher hands those consumers the value HotSpot read,
/// in decimal ([`gen_ordered_parse_argv`]).
pub(crate) const GEN_ORDERED_PARSE_CANONICAL_FLAGS: &[&str] = &[
    "MaxDirectMemorySize",
    "MaxMetaspaceSize",
    "MaxJavaStackTraceDepth",
    "SoftRefLRUPolicyMSPerMB",
    "NewSize",
    "MaxNewSize",
    "NewRatio",
    "InitialHeapSize",
    "MinHeapSize",
    "ZUncommitDelay",
    // zt t28/sm (row `x21_S2_ser`): not inert -- HotSpot 25's `SoftMaxHeapSize`
    // is manageable on every collector and `getVMOption` / `PrintFlagsFinal`
    // answer the command line's value on Serial (`33554432` for `=32m`); it
    // was dropped here, so the row read `MaxHeapSize`.
    "SoftMaxHeapSize",
];

/// gcj j2/ob2: the G1 / ZGC knobs that configure nothing on the
/// Generational collector (`G1ConfigOverrides`, the ZGC director and
/// uncommitter read them), with the values HotSpot's Serial collector takes
/// without a word: the `range(lo, hi)` read from `gc_globals.hpp` (`None` =
/// any value the type parses). The launcher warned for some of them on
/// Generational (`-XX:InitiatingHeapOccupancyPercent=0`,
/// `-XX:MaxGCPauseMillis=0x10`, `-XX:SoftMaxHeapSize=0x1000000`) and clap refused
/// `-XX:ZUncommitDelay=0x10` outright. Under the ordered parse such a token
/// is dropped (its value was checked by the typed scan); a value outside the
/// source range keeps the old arm, because HotSpot's range line for it has
/// no local row.
pub(crate) const GEN_ORDERED_PARSE_INERT_FLAGS: &[(&str, Option<(i128, i128)>)] = &[
    ("InitiatingHeapOccupancyPercent", Some((0, 100))),
    ("G1HeapWastePercent", Some((0, 100))),
    ("MaxGCPauseMillis", Some((1, u64::MAX as i128 - 1))),
    ("G1HeapRegionSize", None),
    ("ZUncommitDelay", None),
    ("ZAllocationSpikeTolerance", None),
];

/// gcj j2/ob2: `(type, range, own parser)` of `name` for the ordered parse:
/// [`hotspot_value_flag_entry`], then (when `extra`) the syntax-only rows of
/// [`GEN_ORDERED_PARSE_EXTRA_FLAGS`].
pub(crate) fn gen_ordered_value_flag_entry(
    name: &str,
    space_tlab: bool,
    extra: bool,
) -> Option<(HotspotFlagType, Option<(i128, i128)>, bool)> {
    hotspot_value_flag_entry(name, space_tlab).or_else(|| {
        if !extra {
            return None;
        }
        GEN_ORDERED_PARSE_EXTRA_FLAGS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|&(_, ty)| (ty, None, false))
    })
}

/// gcj j2/ob2: [`gen_hotspot_value_flag_refusal`] over the typed table and,
/// when `extra` (`CRATONVM_GEN_ONE_ORDERED_VALUE_PARSE`, default ON), the syntax-only
/// rows of [`GEN_ORDERED_PARSE_EXTRA_FLAGS`], still in command-line order.
/// Pure.
///
/// gcj j2/ob2, rows on j2a: when `ranges` (`CRATONVM_GEN_HOTSPOT_VALUE_RANGES`)
/// the scan also answers as the captured HotSpot 25.0.3 rows do: the rows of
/// [`GEN_HOTSPOT_VALUE_RANGES`] (their types and ranges), a
/// `-XX:<BoolFlag>=<v>` of [`GEN_HOTSPOT_BOOL_FLAGS`] (`Missing +/- setting
/// for VM option '<BoolFlag>=<v>'`), and a `MaxTenuringThreshold` above
/// `u32` (`Improperly specified` alone, no range line).
pub(crate) fn gen_value_flag_refusal_with(
    argv: &[String],
    space_tlab: bool,
    extra: bool,
    ranges: bool,
) -> Option<Vec<String>> {
    argv.iter()
        .skip(1)
        .take_while(|a| a.as_str() != "--")
        .find_map(|a| gen_value_flag_refusal_of(a, space_tlab, extra, ranges))
}

/// gcj j2/ob2: [`gen_value_flag_refusal_with`]'s answer for ONE launcher
/// argument (`None`: HotSpot takes it, or it is not a typed value flag).
/// With `extra` and `ranges` both off it is [`gen_hotspot_value_flag_refusal`]'s
/// per-argument answer. zt-dev merge: split out so the ordered first-refusal
/// scan ([`scan_first_ordered_vm_option_refusal`]) can check it in
/// command-line order with the other families. Pure.
pub(crate) fn gen_value_flag_refusal_of(
    arg: &str,
    space_tlab: bool,
    extra: bool,
    ranges: bool,
) -> Option<Vec<String>> {
    let rest = arg.strip_prefix("-XX:")?;
    // gcj j2/x1 (`gcj-j2x1-wave-j1-default-on-review-residuals` item 8): the
    // `+`/`-` spelling of a value flag is HotSpot's `Unexpected +/- setting`
    // here too, so the answer does not depend on the flag-table switch.
    if let Some(name) = rest.strip_prefix(['+', '-']) {
        gen_ordered_value_flag_entry(name, space_tlab, extra)?;
        return Some(vec![format!(
            "Unexpected +/- setting in VM option '{name}'"
        )]);
    }
    let (name, raw) = rest.split_once('=')?;
    if ranges && GEN_HOTSPOT_BOOL_FLAGS.contains(&name) {
        return Some(vec![format!(
            "Missing +/- setting for VM option '{name}={raw}'"
        )]);
    }
    let captured = GEN_HOTSPOT_VALUE_RANGES.iter().find(|(n, ..)| *n == name);
    let (ty, range, own_parser) = match captured {
        Some(&(_, ty, lo, hi)) if ranges => (ty, Some((lo, hi)), false),
        _ => gen_ordered_value_flag_entry(name, space_tlab, extra)?,
    };
    let above_u32 = hotspot_parse_uintx(raw).is_some_and(|v| v > u64::from(u32::MAX));
    if ranges && own_parser && above_u32 {
        return Some(vec![format!("Improperly specified VM option '{name}={raw}'")]);
    }
    hotspot_value_flag_errors(name, raw, ty, range, own_parser)
}

/// `CRATONVM_LAUNCHER_HOTSPOT_FLAG_TABLE` (retirement of
/// `launcher-gc-thread-flag-and-xx-value-refusals-on-g1-and-generational-FIXED-20261022`
/// item 3; default ON, `=0` kill: only the hand-written rows of
/// [`gen_value_flag_refusal_of`] again). Every `-XX:` option naming a flag of
/// HotSpot 25's own table ([`hotspot25_flags`]) is answered as HotSpot's
/// `Arguments::parse_argument` answers it, on every collector
/// ([`hotspot_flag_table_refusal`]). Names HotSpot does not have (this VM's
/// own `-XX:` options among them) are unchanged.
pub(crate) fn launcher_hotspot_flag_table_enabled() -> bool {
    true
}



/// HotSpot 25's young-size ergonomics for the collector HotSpot would run
/// for `hotspot_argv` (`argv[0]` then the VM options, `jvm_args`):
///
/// * G1 -- an effective G1 or no collector named (HotSpot's default,
///   [`scan_hotspot_constraints_are_g1`]) -- `G1YoungGenSizer`'s texts
///   ([`cratonvm_vm::g1_young_sizer_from_flags`]), unless this VM's heap is
///   G1 (its collector prints them itself);
/// * Serial (`-XX:+UseSerialGC`, `-XX:+UseGenerationalGC`) -- `GenArguments`'
///   texts ([`cratonvm_vm::hotspot_serial_young_ergonomics`]) over the
///   resolved heap sizes, and, when the initial eden is one HotSpot's own
///   bootstrap cannot fit in ([`cratonvm_vm::hotspot_serial_boot_failing_eden`],
///   `-Xshare:off` read from `jvm_args`), the two lines of
///   [`cratonvm_vm::hotspot_gc_before_init_lines`];
/// * ZGC -- nothing (HotSpot ZGC takes the flags silently).
///
/// Pure over `config`.
pub(crate) fn hotspot_young_ergo_warnings(
    config: &cratonvm_vm::config::VmConfig,
    hotspot_argv: &[String],
    jvm_args: &[String],
) -> (Vec<String>, Option<[String; 2]>) {
    use cratonvm_vm::config::GcAlgorithm;
    let young = &config.young_gen_sizing;
    if scan_hotspot_constraints_are_g1(hotspot_argv) {
        if config.gc_algorithm == GcAlgorithm::G1 {
            return (Vec::new(), None);
        }
        // The texts do not depend on the region size.
        return (
            cratonvm_vm::g1_young_sizer_from_flags(young, 1 << 20).1,
            None,
        );
    }
    if !scan_effective_generational(hotspot_argv) {
        return (Vec::new(), None);
    }
    let initial = config.initial_heap_size.min(config.max_heap_size);
    let min =
        cratonvm_vm::runtime::unified_logging::hotspot_min_heap_size(config.min_heap_size, initial);
    let flags = cratonvm_vm::SerialYoungFlags::new(
        config.max_heap_size,
        initial,
        min,
        young,
        config.survivor_ratio.map(|r| r as usize),
    );
    let ergo = cratonvm_vm::hotspot_serial_young_ergonomics(&flags);
    let sharing_off = jvm_args
        .iter()
        .rev()
        .find(|a| a.starts_with("-Xshare:"))
        .is_some_and(|a| a == "-Xshare:off");
    let failing = cratonvm_vm::hotspot_serial_boot_failing_eden(sharing_off);
    let boot_failure = (failing > 0 && ergo.initial_eden <= failing)
        .then(|| cratonvm_vm::hotspot_gc_before_init_lines(ergo.initial_young));
    (ergo.warnings, boot_failure)
}

// `CRATONVM_LAUNCHER_HOTSPOT_FLAG_LOCKS` (with
// `CRATONVM_LAUNCHER_HOTSPOT_FLAG_TABLE`): a diagnostic / experimental flag
// before its `-XX:+Unlock...VMOptions` is refused with HotSpot 25's lock
// lines ([`hotspot_flag_table_refusal`]). Default ON, `=0` kill: such a
// flag is taken without its unlock again, as this launcher took
// `-XX:DiagnoseSyncOnValueBasedClasses` and `-XX:+ShowHiddenFrames`
// (interpreter round i5 wave 102).
// WARNING gc-ki-user-flip-20261022: default flipped ON for HotSpot parity
// (it reverses wave 102's lenient diagnostic flags); revert here if apps
// triage finds launch scripts passing diagnostic flags without the unlock.
pub(crate) fn launcher_hotspot_flag_locks_enabled() -> bool {
    true
}

/// The spelling of one `-XX:` option: `+Name` / `-Name`, `Name=<value>`, or a
/// bare `Name`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum XxSpelling<'a> {
    Sign(bool),
    Value(&'a str),
    Bare,
}

/// `-XX:<rest>` split into the flag name and its spelling. Pure.
pub(crate) fn xx_option_spelling(rest: &str) -> (&str, XxSpelling<'_>) {
    if let Some(name) = rest.strip_prefix('+') {
        return (name, XxSpelling::Sign(true));
    }
    if let Some(name) = rest.strip_prefix('-') {
        return (name, XxSpelling::Sign(false));
    }
    match rest.split_once('=') {
        Some((name, raw)) => (name, XxSpelling::Value(raw)),
        None => (rest, XxSpelling::Bare),
    }
}

/// HotSpot 25.0.3's lines for `-XX:<name>=<raw>` of a numeric flag `f`
/// (unlocked): `Improperly specified VM option '<name>=<raw>'` for a value
/// its type does not parse ([`HotspotFlagType::parse`];
/// [`hotspot_parse_double_flag`] for a `double`); for a number outside the
/// flag's declared range, the range line (`uint G1ReservePercent=1024 is
/// outside the allowed range [ 0 ... 50 ]`, a `double` with six decimals:
/// `double MaxRAMPercentage=-1.000000 is outside the allowed range [ 0.000000
/// ... 100.000000 ]`) and then that line. `None`: taken. Pure.
pub(crate) fn hotspot_table_value_errors(f: &hotspot25_flags::F, raw: &str) -> Option<Vec<String>> {
    use hotspot25_flags::{R, T};
    let name = f.name;
    let improper = format!("Improperly specified VM option '{name}={raw}'");
    let ty = match f.ty {
        T::Bool | T::Ccstr => return None,
        T::Double => {
            let Some(v) = hotspot_parse_double_flag(raw) else {
                return Some(vec![improper]);
            };
            let R::Double(lo, hi) = f.range else {
                return None;
            };
            if (lo..=hi).contains(&v) {
                return None;
            }
            return Some(vec![
                format!(
                    "double {name}={v:.6} is outside the allowed range [ {lo:.6} ... {hi:.6} ]"
                ),
                improper,
            ]);
        }
        T::Int => HotspotFlagType::Int,
        T::Intx => HotspotFlagType::Intx,
        T::Uint => HotspotFlagType::Uint,
        T::Uintx => HotspotFlagType::Uintx,
        T::Uint64 => HotspotFlagType::Uint64,
        T::Size => HotspotFlagType::SizeT,
    };
    let range = match f.range {
        R::Int(lo, hi) => Some((lo, hi)),
        _ => None,
    };
    hotspot_value_flag_errors(name, raw, ty, range, false)}

/// The launcher's `-XX:` scan with HotSpot 25's flag table
/// ([`launcher_hotspot_flag_table_enabled`]): the first option of the
/// launcher section of `argv`, in command-line order, that HotSpot 25.0.3
/// refuses while it parses the options, with HotSpot's lines (local rows,
/// any collector):
///
/// * a diagnostic / experimental flag before its `-XX:+Unlock...VMOptions`
///   (an unlock counts only once it has been read; `-XX:-Unlock...` locks
///   again): `Error: VM option '<Name>' is <kind> and must be enabled via
///   -XX:+Unlock<Kind>VMOptions.`, `Error: The unlock option must precede
///   '<Name>'.`, then the spelling's line below -- for a locked flag a
///   `bool` `+`/`-` and any `<Name>=<value>` of a non-`bool` are
///   `Improperly specified VM option` whatever the value;
/// * a `bool` spelled `<Name>=<value>` or bare: `Missing +/- setting for VM
///   option '<Name>=<value>'` (`'<Name>'`);
/// * a non-`bool` spelled `+`/`-`: `Unexpected +/- setting in VM option
///   '<Name>'`; bare: `Improperly specified VM option '<Name>'`;
/// * a numeric value: [`hotspot_table_value_errors`], after the hand-written
///   rows of [`gen_value_flag_refusal_of`] (their verified special cases,
///   such as `MaxTenuringThreshold`'s own parser, keep their answers).
///
/// Options HotSpot does not know are left to the rest of the launcher.
/// `locks`: [`launcher_hotspot_flag_locks_enabled`] (without it a locked
/// flag is answered as an unlocked one). Pure.
pub(crate) fn hotspot_flag_table_refusal(
    argv: &[String],
    space_tlab: bool,
    extra: bool,
    ranges: bool,
    locks: bool,
) -> Option<Vec<String>> {
    let mut unlocks = HotspotFlagUnlocks::for_argv(argv);
    argv.iter()
        .skip(1)
        .take_while(|a| a.as_str() != "--")
        .find_map(|a| hotspot_flag_table_arg_errors(a, &mut unlocks, space_tlab, extra, ranges, locks))
}

/// The unlock state of a `-XX:` scan so far: which of
/// `-XX:±UnlockDiagnosticVMOptions` / `-XX:±UnlockExperimentalVMOptions`
/// was read last as `+`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct HotspotFlagUnlocks {
    pub(crate) diagnostic: bool,
    pub(crate) experimental: bool,
    /// gcj j2/x1: `-XX:+IgnoreUnrecognizedVMOptions` anywhere in the option
    /// section ([`scan_ignore_unrecognized_vm_options`]; HotSpot reads it
    /// before the other options), set by the scan's caller.
    pub(crate) ignore_unrecognized: bool,
    /// [`launcher_unknown_xx_refusal_enabled`]: a name HotSpot 25 does not
    /// know is refused ([`unknown_xx_option_lines`]).
    pub(crate) unknown_names: bool,
}

impl HotspotFlagUnlocks {
    /// The scan state before the first option of `argv`.
    pub(crate) fn for_argv(argv: &[String]) -> Self {
        Self {
            ignore_unrecognized: scan_ignore_unrecognized_vm_options(argv),
            unknown_names: launcher_unknown_xx_refusal_enabled(),
            ..Self::default()
        }
    }
}

// `CRATONVM_LAUNCHER_UNKNOWN_XX_REFUSAL` (retirement of
// `launcher-hotspot-flag-table-residuals-FIXED-20261025` item 1; default ON, `=0`
// kill: an unknown `-XX:` name is dropped silently again, the `java`
// drop-in leniency). With `CRATONVM_LAUNCHER_HOTSPOT_FLAG_TABLE`, a `-XX:`
// name HotSpot 25 does not have on this OS is refused with its lines
// ([`unknown_xx_option_lines`]), as AGENTS.md's "both modes target HotSpot"
// asks; this launcher's own `-XX:` options ([`LAUNCHER_OWN_XX_OPTIONS`]) stay.
// WARNING gc-ki-unknown-xx-20261025: default ON for HotSpot parity; revert
// here if apps triage finds launch scripts passing names HotSpot 25 refuses.
pub(crate) fn launcher_unknown_xx_refusal_enabled() -> bool {
    true
}

/// The `-XX:` names this launcher reads that HotSpot 25 does not have (on
/// any OS). HotSpot refuses each (`Unrecognized VM option`); this launcher
/// implements them, so [`unknown_xx_option_lines`] leaves them alone:
///
/// * `AuditMissingNatives` -- `--XX:AuditMissingNatives`, this VM's report;
/// * `UseGenerationalGC` -- this VM's own collector selector;
/// * `MaxRAMFraction` -- HotSpot's removed heap fraction, still read by the
///   launcher's heap sizing;
/// * `PrintTenuringDistribution` -- HotSpot's pre-unified-logging switch,
///   still printed by the Generational collector.
///
/// A new `-XX:` option of this launcher that HotSpot does not have is added
/// here, or the launcher refuses it.
pub(crate) const LAUNCHER_OWN_XX_OPTIONS: &[&str] = &[
    "AuditMissingNatives",
    "MaxRAMFraction",
    "PrintTenuringDistribution",
    "UseGenerationalGC",
];

/// Is HotSpot's x64 flag table the one of this target? The generated tables
/// were captured on x86-64 Linux and Windows; on another CPU (AArch64's
/// `UseSVE`, ...) or OS (macOS's `globals_bsd.hpp`) an unknown name cannot
/// be told from a flag of that build, so the refusal stays off there.
pub(crate) const UNKNOWN_XX_REFUSAL_TARGET: bool = cfg!(all(
    target_arch = "x86_64",
    any(target_os = "linux", target_os = "windows")
));

/// HotSpot 25's lines for `-XX:<rest>` whose `name` is not in its listed
/// flag table on this OS ([`hotspot25_os_flags::lookup`]), when
/// `unlocks.unknown_names` (and `-XX:+IgnoreUnrecognizedVMOptions` is
/// absent: then HotSpot takes every such name, `develop` ones included):
///
/// * a `develop` flag: `Error: VM option '<Name>' is develop and is
///   available only in debug version of VM.`, then the spelling's line --
///   for a `bool`, `+`/`-` is `Improperly specified VM option '<Name>'`,
///   `=<v>` / bare `Missing +/- setting for VM option '<Name>=<v>'`
///   (`'<Name>'`); for a non-`bool`, `+`/`-` is `Unexpected +/- setting in VM
///   option '<Name>'`, `=<v>` / bare `Improperly specified VM option
///   '<Name>=<v>'` (`'<Name>'`) whatever the value;
/// * an alias (`CreateMinidumpOnCrash`) spelled `=<v>` or bare, after its
///   deprecation warning ([`obsolete_flag_warning_of`]), and any other name:
///   `Unrecognized VM option '<argname>'` (`argname` drops the sign and
///   keeps `=<v>`), then [`hotspot_did_you_mean_line`].
///
/// `None` (taken) for an obsolete flag (its warning is printed by
/// [`obsolete_flag_warning_lines`]), a `+`/`-` alias, HotSpot's `-XX:Flags=` /
/// `-XX:VMOptionsFile=` options, and [`LAUNCHER_OWN_XX_OPTIONS`]. Local
/// HotSpot 25.0.3 (Windows) and 25.0.4.1 (Linux). Pure.
pub(crate) fn unknown_xx_option_lines(
    rest: &str,
    name: &str,
    spelling: XxSpelling<'_>,
    unlocks: &HotspotFlagUnlocks,
) -> Option<Vec<String>> {
    if !UNKNOWN_XX_REFUSAL_TARGET
        || !unlocks.unknown_names
        || unlocks.ignore_unrecognized
        || LAUNCHER_OWN_XX_OPTIONS.contains(&name)
        || (matches!(name, "Flags" | "VMOptionsFile") && matches!(spelling, XxSpelling::Value(_)))
        || cratonvm_vm::config::hotspot_obsolete_flag_warning(&format!("-XX:{name}")).is_some()
    {
        return None;
    }
    if let Some(bool_flag) = hotspot25_os_flags::develop(name) {
        let last = match (bool_flag, spelling) {
            (true, XxSpelling::Sign(_)) => format!("Improperly specified VM option '{name}'"),
            (true, XxSpelling::Value(_) | XxSpelling::Bare) => {
                format!("Missing +/- setting for VM option '{rest}'")
            }
            (false, XxSpelling::Sign(_)) => format!("Unexpected +/- setting in VM option '{name}'"),
            (false, XxSpelling::Value(_) | XxSpelling::Bare) => {
                format!("Improperly specified VM option '{rest}'")
            }
        };
        return Some(vec![
            format!(
                "Error: VM option '{name}' is develop and is available only in debug version of VM."
            ),
            last,
        ]);
    }
    let alias = hotspot25_develop_flags::ALIASES
        .iter()
        .any(|(a, _, _)| *a == name);
    if alias && matches!(spelling, XxSpelling::Sign(_)) {
        return None;
    }
    let argname = match spelling {
        XxSpelling::Sign(_) => name,
        _ => rest,
    };
    let mut lines = vec![format!("Unrecognized VM option '{argname}'")];
    lines.extend(hotspot_did_you_mean_line(name));
    Some(lines)
}

/// gcj j2/x1 (`gcj-j2x1-wave-j1-default-on-review-residuals` item 11b, row
/// `x1_ss_lower`): HotSpot 25's answer to a collector selector it does not
/// know, `-XX:+Use<X>GC` / `-XX:-Use<X>GC` with `Use<X>GC` not in its flag
/// table: `Unrecognized VM option 'Use<X>GC'`, then -- when its fuzzy match
/// finds a flag -- `Did you mean '(+/-)<Flag>'?` (`'<Flag>=<value>'` for a
/// non-`bool`), exit 1, unless `-XX:+IgnoreUnrecognizedVMOptions` (local
/// 25.0.4.1: `UseserialGC` suggests `UseSerialGC`; `UseFooGC`, `Useg1GC`,
/// `UseZgcGC` suggest nothing). The launcher warned and fell back to
/// Generational. `UseGenerationalGC` is this VM's own selector and stays
/// accepted; Parallel / Shenandoah / Epsilon are HotSpot flags, so they keep
/// the warning and the fallback. `None`: not such a selector. Pure.
pub(crate) fn unknown_gc_selector_lines(name: &str, ignore_unrecognized: bool) -> Option<Vec<String>> {
    let core = name.strip_prefix("Use")?.strip_suffix("GC")?;
    if core.is_empty()
        || ignore_unrecognized
        || core == "Generational"
        || hotspot25_os_flags::lookup(name).is_some()
    {
        return None;
    }
    let mut lines = vec![format!("Unrecognized VM option '{name}'")];
    lines.extend(hotspot_did_you_mean_line(name));
    Some(lines)
}

/// HotSpot 25's `Did you mean '(+/-)<Flag>'?` (`'<Flag>=<value>'` for a
/// non-`bool`) after an `Unrecognized VM option` line, when its fuzzy match
/// ([`hotspot_fuzzy_match`]) finds a flag for `name`. Pure.
pub(crate) fn hotspot_did_you_mean_line(name: &str) -> Option<String> {
    let (flag, bool_flag) = hotspot_fuzzy_match(name)?;
    Some(format!(
        "Did you mean '{}{flag}{}'?",
        if bool_flag { "(+/-)" } else { "" },
        if bool_flag { "" } else { "=<value>" }
    ))
}

/// gcj j2/x1: HotSpot 25's `JVMFlag::fuzzy_match(name, len, true)` over its
/// flag table: the flag with the highest `StringUtils::similarity` (first one
/// on a tie), if that score reaches `0.7`, as (name, `bool`). The table here
/// is the generated `-XX:+PrintFlagsFinal` listing (both unlocks on) of this
/// OS, then the `develop` flags a product build does not list but still
/// scores (`-XX:+CountBytecodess` suggests `CountBytecodes`, local 25.0.4.1);
/// HotSpot breaks ties in declaration order, not by name. Pure.
pub(crate) fn hotspot_fuzzy_match(name: &str) -> Option<(&'static str, bool)> {
    let listed = hotspot25_os_flags::flags().map(|f| (f.name, f.ty == hotspot25_flags::T::Bool));
    let mut best: Option<((&'static str, bool), f32)> = None;
    for flag in listed.chain(hotspot25_os_flags::develop_flags()) {
        let score = hotspot_string_similarity(flag.0.as_bytes(), name.as_bytes()) as f32;
        if best.is_none_or(|(_, max)| score > max) {
            best = Some((flag, score));
        }
    }
    best.filter(|&(_, max)| max >= 0.7f32).map(|(f, _)| f)
}

/// HotSpot's `StringUtils::similarity`: twice the number of `a`'s bigrams
/// found in `b` (each counted once), over the summed lengths. Pure.
pub(crate) fn hotspot_string_similarity(a: &[u8], b: &[u8]) -> f64 {
    let total = a.len() + b.len();
    if total == 0 {
        return 0.0;
    }
    let hit = a
        .windows(2)
        .filter(|x| b.windows(2).any(|y| *x == y))
        .count();
    2.0 * hit as f64 / total as f64
}

/// [`hotspot_flag_table_refusal`]'s answer for ONE launcher argument, given
/// the unlocks read before it (updated when `a` is one), so the ordered
/// first-refusal scan ([`scan_first_ordered_vm_option_refusal`]) can check
/// it in command-line order with the other families. `None`: taken, or not
/// a `-XX:` option. Pure apart from `unlocks`.
pub(crate) fn hotspot_flag_table_arg_errors(
    a: &str,
    unlocks: &mut HotspotFlagUnlocks,
    space_tlab: bool,
    extra: bool,
    ranges: bool,
    locks: bool,
) -> Option<Vec<String>> {
    use hotspot25_flags::{L, T};
    let rest = a.strip_prefix("-XX:")?;
    let (name, spelling) = xx_option_spelling(rest);
    // `StartFlightRecording` has HotSpot-specific bare, `:` and `=` forms
    // parsed by `extract_hotspot_flags` and `parse_jfr_start_recording_opts`.
    // Its ccstr table entry would reject the valid bare form before that
    // dedicated parser gets the option.
    if is_start_flight_recording(a) {
        return None;
    }
    let Some(f) = hotspot25_os_flags::lookup(name) else {
        // gcj j2/x1 (item 11b): a selector of a collector HotSpot does not know.
        if matches!(spelling, XxSpelling::Sign(_)) {
            if let Some(lines) = unknown_gc_selector_lines(name, unlocks.ignore_unrecognized) {
                return Some(lines);
            }
        }
        return gen_value_flag_refusal_of(a, space_tlab, extra, ranges)
            .or_else(|| unknown_xx_option_lines(rest, name, spelling, unlocks));
    };
    // `parse_each_vm_init_arg` reads `-XX:MaxHeapSize=` itself (`Invalid
    // maximum heap size: -XX:MaxHeapSize=x`, local 25.0.3); the launcher's
    // memory-size arm answers it.
    if name == "MaxHeapSize" {
        return gen_value_flag_refusal_of(a, space_tlab, extra, ranges);
    }
    // The two unlocks carry their own kind in HotSpot's table but are never
    // locked themselves.
    let unlock_flag = matches!(name, "UnlockDiagnosticVMOptions" | "UnlockExperimentalVMOptions");
    let locked = locks
        && !unlock_flag
        && match f.lock {
            L::None => false,
            L::Diagnostic => !unlocks.diagnostic,
            L::Experimental => !unlocks.experimental,
        };
    let mut lines = Vec::new();
    if locked {
        let (kind, unlock) = match f.lock {
            L::Diagnostic => ("diagnostic", "Diagnostic"),
            _ => ("experimental", "Experimental"),
        };
        lines.push(format!(
            "Error: VM option '{name}' is {kind} and must be enabled via -XX:+Unlock{unlock}VMOptions."
        ));
        lines.push(format!("Error: The unlock option must precede '{name}'."));
    }
    let last = match (f.ty, spelling) {
        (T::Bool, XxSpelling::Sign(on)) => {
            if locked {
                Some(format!("Improperly specified VM option '{name}'"))
            } else {
                match name {
                    "UnlockDiagnosticVMOptions" => unlocks.diagnostic = on,
                    "UnlockExperimentalVMOptions" => unlocks.experimental = on,
                    _ => {}
                }
                None
            }
        }
        (T::Bool, XxSpelling::Value(raw)) => {
            Some(format!("Missing +/- setting for VM option '{name}={raw}'"))
        }
        (T::Bool, XxSpelling::Bare) => Some(format!("Missing +/- setting for VM option '{name}'")),
        (_, XxSpelling::Sign(_)) => Some(format!("Unexpected +/- setting in VM option '{name}'")),
        (_, XxSpelling::Bare) => Some(format!("Improperly specified VM option '{name}'")),
        (_, XxSpelling::Value(raw)) if locked => {
            Some(format!("Improperly specified VM option '{name}={raw}'"))
        }
        (_, XxSpelling::Value(raw)) => {
            return gen_value_flag_refusal_of(a, space_tlab, extra, ranges)
                .or_else(|| hotspot_table_value_errors(f, raw));
        }
    };
    lines.push(last?);
    Some(lines)
}

/// gcj j2/ob2 (rows on j2a, `ob2_cap_*`): types and ranges of `-XX:` value
/// flags as HotSpot 25.0.3 printed them (`<type> <Name>=<v> is outside the
/// allowed range [ <lo> ... <hi> ]`, then `Improperly specified VM option`).
/// They correct three types of [`GEN_HOTSPOT_VALUE_FLAGS`]
/// (`InitiatingHeapOccupancyPercent` and `G1HeapWastePercent` are `uint`,
/// `MaxJavaStackTraceDepth` is `int`). Used under
/// `CRATONVM_GEN_HOTSPOT_VALUE_RANGES`.
pub(crate) const GEN_HOTSPOT_VALUE_RANGES: &[(&str, HotspotFlagType, i128, i128)] = &[
    ("SoftRefLRUPolicyMSPerMB", HotspotFlagType::Intx, 0, i64::MAX as i128),
    ("MaxJavaStackTraceDepth", HotspotFlagType::Int, 0, 1_073_741_823),
    ("MaxGCPauseMillis", HotspotFlagType::Uintx, 1, u64::MAX as i128 - 1),
    ("InitiatingHeapOccupancyPercent", HotspotFlagType::Uint, 0, 100),
    ("G1HeapWastePercent", HotspotFlagType::Uint, 0, 100),
    ("MaxDirectMemorySize", HotspotFlagType::Uint64, 0, i64::MAX as i128),
];

/// gcj j2/ob2 (row `ob2_cap_bool_*`): `bool` flags this launcher knows by
/// their `-XX:+<Name>` / `-XX:-<Name>` spelling. HotSpot 25.0.3 answers
/// `-XX:UseTLAB=true` (`=1`, `=x`) with `Missing +/- setting for VM option
/// 'UseTLAB=true'` and the two `Error:` lines, exit 1 (`Arguments::parse_argument`
/// for any `bool` flag).
pub(crate) const GEN_HOTSPOT_BOOL_FLAGS: &[&str] = &[
    "UseTLAB",
    "ResizeTLAB",
    "PrintTenuringDistribution",
    "DisableExplicitGC",
    "ExplicitGCInvokesConcurrent",
    "HeapDumpOnOutOfMemoryError",
    "ExitOnOutOfMemoryError",
    "CrashOnOutOfMemoryError",
    "UseGCOverheadLimit",
    "UsePerfData",
    "UseSerialGC",
    "UseG1GC",
    "UseParallelGC",
    "UseZGC",
    "UseDynamicNumberOfGCThreads",
    "PrintFlagsFinal",
];





/// gcj j2/ob2: the launcher section of `argv` as the ordered parse hands it
/// on (the caller checked the switch and the collector): each
/// `-XX:<Name>=<raw>` of [`GEN_ORDERED_PARSE_CANONICAL_FLAGS`] whose value
/// HotSpot's parse takes and is not negative becomes `-XX:<Name>=<decimal>`;
/// each of [`GEN_ORDERED_PARSE_INERT_FLAGS`] whose value parses within its
/// range is removed. Everything else, and everything after `--`, is kept as
/// written. A negative value keeps its spelling (its consumer answers it as
/// before). Pure.
pub(crate) fn gen_ordered_parse_argv(argv: Vec<String>) -> Vec<String> {
    ordered_parse_argv(argv, true, false)
}

/// [`gen_ordered_parse_argv`]'s rewrite; the inert drop only when
/// `generational`, and with `table` (`CRATONVM_LAUNCHER_HOTSPOT_FLAG_TABLE`)
/// every numeric flag of HotSpot 25's table in decimal
/// ([`hotspot_table_canonical_value`]). Pure.
pub(crate) fn ordered_parse_argv(argv: Vec<String>, generational: bool, table: bool) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut past_separator = false;
    for (i, a) in argv.into_iter().enumerate() {
        if i == 0 || past_separator {
            out.push(a);
            continue;
        }
        if a == "--" {
            past_separator = true;
            out.push(a);
            continue;
        }
        let Some((name, raw)) = a.strip_prefix("-XX:").and_then(|f| f.split_once('=')) else {
            out.push(a);
            continue;
        };
        let inert = generational.then(|| GEN_ORDERED_PARSE_INERT_FLAGS.iter()
            .find(|(n, _)| *n == name)).flatten();
        if let Some(&(_, range)) = inert {
            let parses = gen_ordered_value_flag_entry(name, false, true).is_some_and(|(ty, ..)| {
                ty.parse(raw).is_some_and(|v| {
                    // A `double` answers 0 (no range rows); a range applies
                    // to the integer flags only.
                    ty == HotspotFlagType::Double
                        || range.is_none_or(|(lo, hi)| (lo..=hi).contains(&v))
                })
            });
            if !parses {
                out.push(a);
            }
            continue;
        }
        if GEN_ORDERED_PARSE_CANONICAL_FLAGS.contains(&name) {
            let parsed = gen_ordered_value_flag_entry(name, false, true)
                .and_then(|(ty, ..)| ty.parse(raw))
                .filter(|v| *v >= 0);
            if let Some(v) = parsed {
                out.push(format!("-XX:{name}={v}"));
                continue;
            }
        }
        if table {
            if let Some(canonical) = hotspot_table_canonical_value(name, raw) {
                out.push(format!("-XX:{name}={canonical}"));
                continue;
            }
        }
        out.push(a);
    }
    out
}

/// `CRATONVM_LAUNCHER_HOTSPOT_FLAG_TABLE`: the value of `-XX:<name>=<raw>`,
/// a numeric flag of HotSpot 25's table, as HotSpot read it, in a spelling
/// every consumer's own parser reads -- a non-negative integer in decimal
/// (`0x10`, `1k`, `16m` become `16`, `1024`, `16777216`), a `double` in
/// Rust's shortest decimal (`0x10` becomes `16`). `None`: keep `raw` (not a
/// table flag, not numeric, negative, or not parsed; the early scan answered
/// what HotSpot refuses). Pure.
pub(crate) fn hotspot_table_canonical_value(name: &str, raw: &str) -> Option<String> {
    use hotspot25_flags::T;
    let f = hotspot25_os_flags::lookup(name)?;
    let ty = match f.ty {
        T::Bool | T::Ccstr => return None,
        T::Double => return hotspot_parse_double_flag(raw).map(|v| v.to_string()),
        T::Int => HotspotFlagType::Int,
        T::Intx => HotspotFlagType::Intx,
        T::Uint => HotspotFlagType::Uint,
        T::Uintx => HotspotFlagType::Uintx,
        T::Uint64 => HotspotFlagType::Uint64,
        T::Size => HotspotFlagType::SizeT,
    };
    ty.parse(raw).filter(|v| *v >= 0).map(|v| v.to_string())}

/// gcj j2/ob2 (residual page `gcj-j1ob-launcher-typed-value-residuals-FIXED-20261022`
/// item 4): the `AfterMemoryInit` constraint lines the parsed path prints
/// (`heap_free_ratios_from_flags`' pair, then [`space_tlab_constraint_errors`]),
/// for a `-version` / `-showversion` launch on Generational, which exits
/// before the parsed path. `SurvivorRatio`'s constraint reads `MaxHeapSize`:
/// it is checked only when the command line gives one (`-Xmx`,
/// `-XX:MaxHeapSize=`; the ergonomic maximum is decided on the parsed path).
/// The TLAB constraints read Serial's fixed TLAB maximum. Empty = start.
/// `space_tlab`: `CRATONVM_LAUNCHER_SPACE_TLAB_FLAGS`. Pure.
pub(crate) fn gen_version_constraint_errors(argv: &[String], space_tlab: bool) -> Vec<String> {
    version_constraint_errors_for(argv, space_tlab, None)
}

/// [`gen_version_constraint_errors`], or with `g1_max_heap` HotSpot's
/// default G1 answer for a command line that names no collector
/// (`CRATONVM_LAUNCHER_HOTSPOT_FLAG_TABLE`): the same lines from G1's
/// geometry for that maximum heap (the explicit one, else the ergonomic
/// default; `-XX:G1HeapRegionSize` when given), then the G1 PLAB constraints,
/// as the parsed path prints them. Pure.
pub(crate) fn version_constraint_errors_for(
    argv: &[String],
    space_tlab: bool,
    g1_max_heap: Option<usize>,
) -> Vec<String> {
    let launcher = || argv.iter().skip(1).take_while(|a| a.as_str() != "--");
    let last = |prefix: &str| {
        launcher()
            .filter_map(|a| a.strip_prefix(prefix))
            .last()
            .map(str::to_string)
    };
    let min_free = last("-XX:MinHeapFreeRatio=");
    let max_free = last("-XX:MaxHeapFreeRatio=");
    // Generational reads the decimal / hex pair (zt t12/tg5 keeps HotSpot's
    // `uintx` reading to an effective ZGC command line).
    if let Err(lines) =
        heap_free_ratios_from_flags_in(min_free.as_deref(), max_free.as_deref(), false)
    {
        return lines;
    }
    if !space_tlab {
        return Vec::new();
    }
    let flags = HotspotFlags {
        space_tlab_values_in_order: launcher()
            .filter_map(|a| space_tlab_value_flag(a))
            .map(|(name, raw)| (name, raw.to_string()))
            .collect(),
        ..Default::default()
    };
    let (survivor_ratio, tlab) = match space_tlab_flags_from_hotspot_flags(&flags) {
        Ok(checked) => checked,
        Err(lines) => return lines,
    };
    if let Some(max_heap) = g1_max_heap {
        let region = last("-XX:G1HeapRegionSize=")
            .and_then(|raw| hotspot_parse_uintx(&raw))
            .filter(|&r| r > 0)
            .and_then(|r| usize::try_from(r).ok());
        let geometry =
            hotspot_space_tlab_geometry(cratonvm_vm::config::GcAlgorithm::G1, max_heap, region);
        let mut lines = space_tlab_constraint_errors(survivor_ratio, &tlab, geometry);
        if let Some((_, _, tlab_max)) = geometry {
            lines.extend(g1_plab_constraint_errors(tlab.min_tlab_size, tlab_max));
        }
        return lines;
    }
    let explicit_max = early_max_heap(argv).map(|(size, _)| size);
    let geometry = hotspot_space_tlab_geometry(
        cratonvm_vm::config::GcAlgorithm::Generational,
        explicit_max.unwrap_or(0),
        None,
    );
    let survivor_ratio = survivor_ratio.filter(|_| explicit_max.is_some());
    space_tlab_constraint_errors(survivor_ratio, &tlab, geometry)
}

/// `CRATONVM_GEN_PRINT_FLAGS_FINAL` (gcj j2/ob2; default ON, `=0` kill;
/// proposal `gcj-j1ob-proposal-print-flags-final-from-the-typed-table`):
/// `-XX:+PrintFlagsFinal` prints `[Global flags]` and
/// one line per flag this VM models ([`gen_print_flags_final_lines`]) on
/// stdout before the VM starts, and a `-version` launch boots the VM to print
/// it (HotSpot prints the table after its ergonomics, before the banner).
/// The explicit HotSpot request is honored by default; `=0` is a diagnostic
/// kill switch. The column layout is read from `jvmFlag.cpp`.
pub(crate) fn gen_print_flags_final_enabled() -> bool {
    true
}

/// gcj j2/ob2: does an option list (HotSpot's `jvm_args`, or the launcher
/// section of an argv) ask for `-XX:+PrintFlagsFinal`? The last of
/// `-XX:+PrintFlagsFinal` / `-XX:-PrintFlagsFinal` wins. Pure.
pub(crate) fn print_flags_final_requested<'a>(options: impl Iterator<Item = &'a String>) -> bool {
    options
        .filter_map(|a| match a.as_str() {
            "-XX:+PrintFlagsFinal" => Some(true),
            "-XX:-PrintFlagsFinal" => Some(false),
            _ => None,
        })
        .last()
        .unwrap_or(false)
}

/// gcj j2/ob2: should a version query launch boot the VM to print the
/// `-XX:+PrintFlagsFinal` table (switch on, the library
/// resolves)? Pure apart from the switch read.
pub(crate) fn gen_print_flags_final_boots_version(
    query: VersionQuery,
    argv: &[String],
    library_resolves: bool,
) -> bool {
    library_resolves
        && matches!(query, VersionQuery::Version | VersionQuery::Show)
        && gen_print_flags_final_enabled()
        && print_flags_final_requested(argv.iter().skip(1).take_while(|a| a.as_str() != "--"))
}

/// gcj j2/ob2: the `-XX:+PrintFlagsFinal` table for a Generational `config`
/// (HotSpot's `-XX:+UseSerialGC`): `[Global flags]`, then the flags this VM
/// models, sorted by name as `JVMFlag::printFlags` sorts them, each with its
/// effective value and origin -- `{command line}` when `config.jvm_args`
/// (the options as the user wrote them) sets it, `{ergonomic}` for the heap
/// sizes HotSpot's ergonomics derive, `{default}` otherwise. Types are the
/// typed table's (verified by the j1 range rows where it has a range);
/// defaults are HotSpot 25's (`globals.hpp` / `gc_globals.hpp`, read).
/// Flags this VM does not model are absent rather than invented (proposal
/// scope (a)). Pure.
pub(crate) fn gen_print_flags_final_lines(config: &cratonvm_vm::VmConfig) -> Vec<String> {
    std::iter::once("[Global flags]".to_string()).chain(
        cratonvm_vm::config::hotspot_flags::resolved_flags(config)
            .iter().map(|row| row.print_final())
    ).collect()
}

/// `CRATONVM_JFR_STARTUP_BANNER` (gcj j2/ob2; default ON, `=0` kill;
/// `docs/feature-designs/real-jdk-mode.md`):
/// a `-XX:StartFlightRecording` start-up recording prints HotSpot's
/// `[jfr,startup]` banner on stdout ([`jfr_startup_banner_texts`]). Every
/// collector: the explicit recording request is honored by default.
pub(crate) fn jfr_startup_banner_enabled() -> bool {
    true
}

/// gcj j2/ob2: the `[jfr,startup]` banner texts HotSpot 25.0.3 prints for a
/// start-up recording with options `raw`, for the option shapes with captured
/// rows (`gci-runs/i48p/s_jc9/jc9_leg_g1_egc.hs`; `gcj-runs/j2a/vj2-ob2/ob2_cap_jfr_*.hs`):
///
/// * none: `Started recording 1. No limit specified, using maxsize=250MB as
///   default.`, an empty line, `Use jcmd <pid> JFR.dump name=1
///   filename=FILEPATH to copy recording data to file.`;
/// * `filename=` (with `name=<n>`): the same first two lines, then `Use jcmd
///   <pid> JFR.dump name=<n or 1> to copy recording data to file.`;
/// * `filename=` with `maxsize=` or `maxage=`: `Started recording 1.`, an
///   empty line, the same `Use jcmd` line;
/// * `filename=` with `duration=`: `Started recording 1. The result will be
///   written to:`, an empty line, the absolute file name.
///
/// `None` for every other shape (no row) and when the `-Xlog` spec names
/// `jfr` or `disable` (a selection that can silence or re-route the tag set:
/// not modelled). Pure apart from making a relative file name absolute.
pub(crate) fn jfr_startup_banner_texts(raw: &str, _xlog_spec: Option<&str>, pid: u32) -> Option<Vec<String>> {
    let mut options = std::collections::HashMap::new();
    for option in raw.split(',').map(str::trim).filter(|o| !o.is_empty()) {
        let (key, value) = option.split_once('=')?;
        options.insert(key, value);
    }
    let get = |key: &str| options.get(key).copied();
    let name = get("name").unwrap_or("1");
    let file = get("filename");
    if get("duration").is_some() {
        let file = file?;
        let path = std::path::absolute(file).ok()?.display().to_string();
        return Some(vec![
            "Started recording 1. The result will be written to:".to_string(),
            String::new(), path,
        ]);
    }
    let limited = get("maxsize").is_some() || get("maxage").is_some();
    let first = if limited {
        "Started recording 1."
    } else {
        "Started recording 1. No limit specified, using maxsize=250MB as default."
    };
    let filename_hint = if file.is_some() { "" } else { " filename=FILEPATH" };
    Some(vec![first.to_string(), String::new(),
        format!("Use jcmd {pid} JFR.dump name={name}{filename_hint} to copy recording data to file.")])
}

/// A second `-agentlib:jdwp` (interpreter round i4 wave 85, lane L1): the
/// back end's `Agent_OnLoad` refuses to load twice (`JNI_ERR`), and HotSpot
/// ends the boot with `agent library failed Agent_OnLoad: jdwp`.
pub(crate) fn exit_with_jdwp_agent_loaded_twice() -> ! {
    use std::io::Write as _;
    eprintln!(
        "ERROR: Cannot load this JVM TI agent twice, check your java command line for duplicate jdwp options."
    );
    let _ = std::io::stderr().flush();
    let report = cratonvm_vm::error::VmInitializationError {
        detail: "agent library failed Agent_OnLoad: jdwp".to_string(),
    }
    .report();
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(report.as_bytes());
    let _ = out.flush();
    std::process::exit(1);
}

/// JPLIS's stderr line for a `-javaagent:` JAR its `Agent_OnLoad` cannot use
/// (`InvocationAdapter.c`: `readAttributes` failed, or no `Premain-Class`),
/// the JAR named as the command line spelled it; `None` for an error
/// `parse_javaagent_spec` does not return.
pub(crate) fn javaagent_jar_failure_line(
    error: &cratonvm_vm::runtime::agent_loader::AgentLoadError,
) -> Option<String> {
    use cratonvm_vm::runtime::agent_loader::AgentLoadError;
    match error {
        AgentLoadError::JarNotFound { path, .. } => Some(format!(
            "Error opening zip file or JAR manifest missing : {}",
            path.display()
        )),
        AgentLoadError::MissingPremainClass { jar } => Some(format!(
            "Failed to find Premain-Class manifest attribute in {}",
            jar.display()
        )),
        _ => None,
    }
}

/// A startup `-javaagent:` whose JAR cannot be used (interpreter round i4
/// wave 84, lane L1). JPLIS's `Agent_OnLoad` prints its line on stderr and
/// returns `JNI_ERR`; HotSpot's `invoke_Agent_OnLoad` then calls
/// `vm_exit_during_initialization("agent library failed Agent_OnLoad",
/// "instrument")`: the two lines on stdout, exit status 1, the application
/// never started. This launcher printed a warning and ran `main` without the
/// agent. Probe `tools/probes/interp/L1/L1W84JavaagentBadJar.java`.
pub(crate) fn exit_with_javaagent_jar_failure(
    error: &cratonvm_vm::runtime::agent_loader::AgentLoadError,
) -> ! {
    use std::io::Write as _;
    match javaagent_jar_failure_line(error) {
        Some(line) => eprintln!("{line}"),
        None => tracing::warn!("-javaagent: {error}"),
    }
    let _ = std::io::stderr().flush();
    let report = cratonvm_vm::error::VmInitializationError {
        detail: "agent library failed Agent_OnLoad: instrument".to_string(),
    }
    .report();
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(report.as_bytes());
    let _ = out.flush();
    std::process::exit(1);
}

/// `new java.io.File(path).getPath()`, the name `LauncherHelper`'s jar texts
/// print (`JarFile.getName()`): duplicate separators collapsed and a trailing
/// one dropped, and on Windows `/` read as `\` (a UNC path keeps its leading
/// pair). Interpreter round i2 wave 60, lane L5.
pub(crate) fn launcher_jar_file_name(path: &str) -> String {
    let windows = cfg!(windows);
    let sep = if windows { '\\' } else { '/' };
    let mut out = String::with_capacity(path.len());
    let mut prev_sep = false;
    for (i, c) in path.chars().enumerate() {
        if c == '/' || (windows && c == '\\') {
            if prev_sep && !(windows && i == 1) {
                continue;
            }
            out.push(sep);
            prev_sep = true;
        } else {
            out.push(c);
            prev_sep = false;
        }
    }
    let keeps_trailing = out.len() == 1 || (windows && out.len() == 3 && out.ends_with(":\\"));
    if out.ends_with(sep) && !keeps_trailing {
        out.pop();
    }
    out
}

/// Shutdown half of the Real-JDK surface: drain the violation logs one last
/// time, then write the census files.
///
/// Called from the clean shutdown path **and** from each failing path that
/// still has a VM, so a strict-mode failure is categorisable.
pub(crate) fn finish_jdk_only(
    args: &Args,
    shared: &cratonvm_vm::SharedVm,
    watermark: &mut ViolationWatermark,
) {
    if args.trace_jdk_only || args.explain_jdk_only {
        trace_jdk_only_violations(shared, watermark, "shutdown", args.explain_jdk_only);
    }
    write_jdk_only_dumps(args, shared);
    // JIT round 19 wave 2, lane funnel2: `CRATONVM_DBG_NATIVE_SITE_DOOR_CENSUS`
    // and `CRATONVM_DBG_FUNNEL_INTRINSIC_CENSUS` (silent unless armed).
    shared.report_native_funnel_censuses_at_exit();
    // Interpreter round i4 wave 94, lane L6: `CRATONVM_DBG_VTHREAD_CENSUS`
    // (silent unless armed).
    shared.report_vthread_census_at_exit();
}

pub(crate) fn resolve_watchdog_timeout(
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

/// Phase 5 WP5: may this run use a boot image? Every launcher feature that
/// runs code, prints, or starts a thread between P0 (`Vm::try_new`) and P1
/// (after `boot_publish_jdk_booted`) makes it ineligible: a restored run skips
/// that span, and a capture run may not start threads in it.
pub(crate) fn boot_image_eligibility(
    args: &Args,
    config: &VmConfig,
    version_boot: bool,
    print_flags_final: bool,
    java_agents: bool,
    max_metaspace: bool,
) -> std::result::Result<(), &'static str> {
    let watchdog_env = std::env::var("CRATONVM_DISABLE_DEFAULT_WATCHDOG").ok().as_deref() != Some("1")
        && std::env::var("CRATONVM_DEFAULT_WATCHDOG_SEC").is_ok();
    let checks: [(bool, &'static str); 12] = [
        (config.java_home.is_none(), "no real-JDK boot (java.home unset)"),
        (version_boot, "-version / -showversion boot"),
        (print_flags_final, "-XX:+PrintFlagsFinal"),
        (java_agents, "-javaagent"),
        (!config.jvmti_agent_options.is_empty(), "-agentlib/-agentpath"),
        (config.jdwp_agent.is_some(), "-agentlib:jdwp"),
        (!config.jfr_startup_options.is_empty(), "-XX:StartFlightRecording"),
        (args.trace_jdk_only || args.explain_jdk_only, "--trace/--explain-real-jdk"),
        (
            args.stack_dump_on_timeout.is_some_and(|s| s > 0) || watchdog_env,
            "stack-dump watchdog",
        ),
        (args.stack_sample_ms.is_some_and(|ms| ms > 0), "--stack-sample-ms"),
        (
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok().as_deref() == Some("1"),
            "native ring recording",
        ),
        (max_metaspace, "-XX:MaxMetaspaceSize"),
    ];
    match checks.iter().find(|(hit, _)| *hit) {
        Some((_, why)) => Err(why),
        None => Ok(()),
    }
}

pub(crate) fn launcher_nojit_requested(argv: &[String]) -> bool {
    argv.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--nojit")
        || (launcher_xint_enabled() && launcher_xint_requested(argv))
}

/// `CRATONVM_LAUNCHER_XINT` (HotSpot parity, default ON, `=0` kill;
/// interpreter round i7 wave 138, lane L1b, page
/// `i137-L1b-xint-is-dropped-and-java-vm-info-never-says-interpreted`):
/// `-Xint` selects the `--nojit` configuration (interpreter only), and
/// `java.vm.info` then says `interpreted mode`
/// (`cratonvm_vm::vm::vm_info_execution_mode_list`). Read once, in the early
/// pass, straight from the environment: the flag snapshot is latched from
/// this pass's overlay, so it cannot serve this read yet (the
/// `CRATONVM_COMPAT=` token spelling is therefore not honoured here, only
/// the variable).
pub(crate) fn launcher_xint_enabled() -> bool {
    None::<std::ffi::OsString>.is_none_or(|v| {
        let v = v.to_string_lossy().trim().to_ascii_lowercase();
        !(v == "0" || v == "false" || v == "off" || v == "no")
    })
}

/// Does the launcher's option section (up to `--`) ask for `-Xint`, the
/// last of `-Xint` / `-Xmixed` / `-Xcomp` winning as in HotSpot? `-Xcomp`
/// has no CratonVM equivalent and runs mixed (the JIT's own thresholds).
pub(crate) fn launcher_xint_requested(argv: &[String]) -> bool {
    let options: Vec<String> = argv
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .cloned()
        .collect();
    cratonvm_vm::vm::hotspot_execution_mode_option(&options) == Some("-Xint")
}

/// Which assertion scope an argv token switches, and to what.
///
/// `Some((system, enable))` for the four **unscoped** spellings; `None` for
/// everything else, including the scoped forms (`-ea:pkg...`, `-da:Class`) —
/// see [`launcher_assertions_requested`].
pub(crate) fn assertion_flag_scope(arg: &str) -> Option<(bool, bool)> {
    match arg {
        "-ea" | "-enableassertions" => Some((false, true)),
        "-da" | "-disableassertions" => Some((false, false)),
        "-esa" | "-enablesystemassertions" => Some((true, true)),
        "-dsa" | "-disablesystemassertions" => Some((true, false)),
        _ => None,
    }
}

/// The JVM-wide assertion status requested on the command line, or `None` when
/// no unscoped assertion flag was passed.
///
/// `Class.desiredAssertionStatus()` decides whether a class's `<clinit>` stores
/// `$assertionsDisabled = false`, i.e. whether real `assert` bytecode throws.
/// CratonVM has always implemented it — `assertion_status_default()` in
/// `native-builtins` — but the only way to reach the switch was
/// `CRATONVM_ENABLE_ASSERTIONS`, which no Maven Surefire or Gradle fork will
/// ever set. Surefire forks the test JVM with `-ea` by default, so every
/// `assert`-based validation test in the corpus silently did nothing. The
/// recorded case is netty's HTTP/2 flow-controller classes, where HotSpot goes
/// 34/34 with `-ea` and CratonVM stayed at 28/34 — see the retired
/// `ea-flag-ignored-so-assert-never-fires-20260812` write-up.
///
/// **Why it is scanned here and not inside `normalize_java_launcher_argv`.**
/// That function is pure and has ~60 unit tests; `CRATONVM_ENABLE_ASSERTIONS` is
/// a declared flag served from the immutable snapshot `install_flags` latches at
/// the top of `main`. A `set_var` from inside the normaliser would be invisible
/// to the VM (the snapshot is already taken by the time `run()` normalises) and
/// would make those tests order-dependent. Injecting a launcher override is the
/// supported route and the one `--nojit` and `--dump-phase-report` already use.
///
/// **Only the user scope.** Interpreter round i5 wave 107, lane L5
/// (`interpreter-i106-L5-assertion-switches-are-one-global-FIXED-20261016.md`): the switch is
/// now HotSpot's USER default (`-ea` / `-da`, last wins); the system default
/// (`-esa` / `-dsa`) and the scoped forms travel in
/// `VmConfig::assertion_directives` ([`launcher_assertion_directives`]), so
/// `-ea -dsa` is "user on, system off" and a lone `-esa` no longer turns user
/// assertions on. Before wave 107 the two scopes were OR-ed onto this one bit.
pub(crate) fn launcher_assertions_requested(argv: &[String]) -> Option<bool> {
    let mut user: Option<bool> = None;
    for arg in argv.iter().take_while(|arg| arg.as_str() != "--") {
        if let Some((false, enable)) = assertion_flag_scope(arg) {
            user = Some(enable);
        }
    }
    user
}

/// Every assertion option of the launcher portion of `argv` (up to `--`), in
/// order, as HotSpot's `JavaAssertions` keeps them: `None` when there is none,
/// so the VM's answers stay with `CRATONVM_ENABLE_ASSERTIONS`. Carried in
/// `VmConfig::assertion_directives` (per VM, never a process global);
/// `Class.desiredAssertionStatus0` and `ClassLoader.retrieveDirectives` answer
/// from it. Interpreter round i5 wave 107, lane L5.
pub(crate) fn launcher_assertion_directives(
    argv: &[String],
) -> Option<cratonvm_native_api::assertion_directives::AssertionDirectives> {
    let mut directives = cratonvm_native_api::assertion_directives::AssertionDirectives::default();
    let mut any = false;
    for arg in argv.iter().skip(1).take_while(|arg| arg.as_str() != "--") {
        any |= directives.apply_option(arg);
    }
    any.then_some(directives)
}

/// zt t18/hunt2: the launcher portion's `--finalization=<value>` (JEP 421),
/// HotSpot 25's `Arguments::parse_each_vm_init_arg` arm: `enabled` and
/// `disabled` set the VM's finalization bit, the last one written wins (the
/// options are applied in order), and any other value is refused with
/// `Invalid finalization value '<value>', must be 'disabled' or 'enabled'.`
/// (`Err` carries the value) before the VM is created. `Ok(None)`: the
/// option is absent and the default (enabled) stands. Stops at `--`, so a
/// program argument of that spelling is never read.
pub(crate) fn launcher_finalization_option(argv: &[String]) -> std::result::Result<Option<bool>, String> {
    let mut enabled = None;
    for arg in argv.iter().skip(1).take_while(|arg| arg.as_str() != "--") {
        let Some(value) = arg.strip_prefix("--finalization=") else {
            continue;
        };
        enabled = Some(match value {
            "enabled" => true,
            "disabled" => false,
            other => return Err(other.to_string()),
        });
    }
    Ok(enabled)
}

/// A launcher-portion token the assertion strip arm of
/// `normalize_java_launcher_argv` removes but HotSpot does not accept
/// (`-enableassertionsX`, `-enablesystemassertions:p`, `-disablesystemassertionsX`
/// and the like: the system switches take no tail, the user ones only `:`).
/// Interpreter round i5 wave 107, lane L5.
pub(crate) fn refused_assertion_spelling(argv: &[String]) -> Option<&str> {
    argv.iter()
        .skip(1)
        .take_while(|arg| arg.as_str() != "--")
        .map(String::as_str)
        .find(|arg| {
            [
                "-enableassertions",
                "-disableassertions",
                "-enablesystemassertions",
                "-disablesystemassertions",
            ]
            .iter()
            .any(|p| arg.starts_with(p))
                && !cratonvm_native_api::assertion_directives::AssertionDirectives::default()
                    .apply_option(arg)
        })
}

/// The `--dump-phase-report <FILE>` path, scanned out of the launcher portion
/// of the expanded argv.
///
/// Scanned here rather than read from the parsed [`Args`] because the three
/// `CRATONVM_PHASE_ACCOUNTING*` names are **declared** flags:
/// `flags::runtime_var_os` serves a declared name from the immutable snapshot
/// that `install_flags` latches, and `phase::level()` latches its own answer on
/// first read — both of which happen before `run()` parses anything. A
/// `set_var` after the parse would be invisible. Injecting the value as a
/// launcher override into `VmFlags::from_env_with_overrides` is the supported
/// route, and the one `--nojit` above already uses.
///
/// Accepts both `--dump-phase-report FILE` and `--dump-phase-report=FILE`, and
/// stops at the `--` separator so a Java program argument of the same spelling
/// is never consumed.
pub(crate) fn launcher_phase_report_path(argv: &[String]) -> Option<String> {
    let end = argv
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(argv.len());
    let launcher = &argv[..end];
    for (i, arg) in launcher.iter().enumerate() {
        if let Some(path) = arg.strip_prefix("--dump-phase-report=") {
            return Some(path.to_string());
        }
        if arg == "--dump-phase-report" {
            return launcher.get(i + 1).cloned();        }    }
    None
}

/// The targetless level `run()` adds when `RUST_LOG` names no global level.
///
/// nio21 wave 8 lane L4: ERROR, no longer WARN. A plain run printed `tracing`
/// WARN events (`[NativeBridge] ... unregistered native methods`,
/// `NoSuchMethodError method=...`) on stderr, colourised and timestamped,
/// where HotSpot prints nothing, so no ordinary program's stderr matched.
/// `RUST_LOG=warn` restores the old default exactly (a targetless level
/// suppresses this floor; see [`rust_log_names_a_global_level`]).
pub(crate) const DEFAULT_LOG_FLOOR: tracing::Level = tracing::Level::ERROR;

/// Does `RUST_LOG` name a level for EVERY target, rather than only for some?
///
/// `EnvFilter`'s grammar makes a directive global exactly when it carries no
/// `target=` part: `info` and `trace` are global, `my_crate=debug` and
/// `[span]=trace` are not. This is what decides whether `run()` may add its own
/// targetless WARN floor — see the block that calls it, which explains why
/// adding that floor unconditionally silently downgraded `RUST_LOG=info`.
///
/// `None` (unset) and an empty or whitespace-only value are all "no global
/// level", so the floor applies. `EnvFilter` ignores an empty directive too, so
/// `RUST_LOG=,,` is the same as unset here and there.
pub(crate) fn rust_log_names_a_global_level(rust_log: Option<&str>) -> bool {
    rust_log.is_some_and(|v| {
        v.split(',')
            .map(str::trim)
            .any(|d| !d.is_empty() && !d.contains('='))
    })
}

/// The binary name HotSpot's `Throwable.toString()` prints for an internal
/// class name (`java/lang/IllegalStateException` ->
/// `java.lang.IllegalStateException`).
pub(crate) fn binary_class_name(internal: &str) -> String {
    internal.replace('/', ".")
}

/// The `\tat ...` lines of the uncaught-exception report for a trace taken
/// from the VM's throwable registry.
///
/// The registry stores a trace OUTERMOST-first (`main` at index 0), and
/// HotSpot's `printStackTrace` prints the throw site first, so the lines are
/// emitted in reverse. Each line is `StackTraceElement.toString()`'s shape for
/// an app-loader frame: the binary class name, `Native Method` for line -2,
/// `File.java:N` for a known line, `File.java` for an unknown one, and
/// `Unknown Source` when the class has no `SourceFile`
/// (gce-e2f-uncaught-exception-report-lists-frames-outermost-first-20260929).
pub(crate) fn uncaught_report_frame_lines(frames: &[cratonvm_vm::StackTraceFrame]) -> Vec<String> {
    frames
        .iter()
        .rev()
        .map(|frame| {
            let location = match (frame.file.as_deref(), frame.line) {
                (_, -2) => "Native Method".to_string(),
                (Some(f), n) if !f.is_empty() && n >= 0 => format!("{f}:{n}"),
                (Some(f), _) if !f.is_empty() => f.to_string(),
                _ => "Unknown Source".to_string(),
            };
            format!(
                "\tat {}.{}({location})",
                binary_class_name(&frame.class),
                frame.method
            )
        })
        .collect()
}

/// How many of `trace`'s lines `Throwable.printEnclosedStackTrace` prints
/// before `\t... N more`: all of them for the outermost throwable
/// (`enclosing` is `None`), otherwise up to the trailing run of frames equal to
/// the enclosing trace's trailing run.
pub(crate) fn frames_not_in_common(trace: &[String], enclosing: Option<&[String]>) -> usize {
    let Some(enclosing) = enclosing else {
        return trace.len();
    };
    let common = trace
        .iter()
        .rev()
        .zip(enclosing.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    trace.len() - common
}

/// Whether the uncaught-exception report is produced by the program's own
/// `Throwable.printStackTrace(PrintWriter)` (the default) rather than by the
/// launcher's Rust renderer. `CRATONVM_UNCAUGHT_RUST_RENDER=1` selects the Rust
/// renderer (gcf f2/r, `gcf-f1r-uncaught-report-lacks-module-prefix-and-suppressed`).
pub(crate) fn uncaught_report_runs_print_stack_trace() -> bool {
    false
}



/// The outcome of a `main` that died on an uncaught exception whose report has
/// ALREADY been written (by an `UncaughtExceptionHandler`, by the JDK's
/// `ThreadGroup.uncaughtException`, or by the launcher's own render). The
/// `main-vm` error arm prints nothing for it and the process exits 1, as
/// HotSpot's launcher does (nio21 wave 8 lane L4). The text names who reported
/// it, for `CRATONVM_DBG=launcher-outcome` only.
#[derive(Debug)]
pub(crate) struct UncaughtReported(pub(crate) String);

impl std::fmt::Display for UncaughtReported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "uncaught exception in main, reported above ({})", self.0)
    }
}

impl std::error::Error for UncaughtReported {}

pub(crate) fn java_bootstrap_error(vm: &Vm, error: MethodCallFailed) -> anyhow::Error {
    let detail = match error {
        MethodCallFailed::ExceptionThrown(obj) => vm.describe_exception(obj),
        other => other.to_string(),
    };
    if vm.shared.debug.jfr_startup_claimed.load(std::sync::atomic::Ordering::Acquire) {
        use cratonvm_vm::runtime::serviceability::VmDiagnosticState as _;
        let message = detail.split_once(": ").map_or(detail.as_str(), |(_, message)| message);
        cratonvm_vm::runtime::unified_logging::log_jfr_startup_error(message, vm.shared.uptime_secs());
        println!("Error occurred during initialization of VM\nFailure when starting JFR on_create_vm_3");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        return UncaughtReported("JFR startup failed".into()).into();
    }
    anyhow::anyhow!("Java bootstrap failed: {detail}")
}


/// Whether an exception escaping `main` with no handler installed goes through
/// the JDK's own `Thread.dispatchUncaughtException` ->
/// `ThreadGroup.uncaughtException` (default ON; nio21 wave 8 lane L4), the
/// path every other thread's death already takes, rather than the launcher's
/// `StringWriter` render. HotSpot does the former, so the report follows a
/// `System.setErr`, names a renamed main thread, and on a throwable whose
/// `toString()` throws prints the bare header plus `JavaThread::exit`'s
/// `Exception: ... thrown from the UncaughtExceptionHandler` line.
/// `CRATONVM_UNCAUGHT_MAIN_VIA_THREAD_GROUP=0` restores the render.
pub(crate) fn uncaught_main_via_thread_group() -> bool {
    true
}

/// zs s2/nd2 (`zgc-zs2rv-wave-s1-review-residuals-CLOSED-20261019.md` §2): report a
/// throwable raised BEFORE `main` is entered -- the main class's `<clinit>`,
/// an instance `main`'s receiver allocation or constructor -- the way
/// HotSpot's launcher does. `java.c` checks those steps with
/// `CHECK_EXCEPTION_NULL_LEAVE`, whose `ExceptionDescribe` writes
/// `Exception in thread "<name>" ` from the VM (no Java allocation), then
/// calls the throwable's `printStackTrace()` and DISCARDS anything that call
/// throws; no uncaught-exception handler runs. So on the full heap of an
/// `OutOfMemoryError` while making the receiver HotSpot 25 prints only the
/// prefix, no newline (measured, zs s1/nd: `Zs1NdLauncherOome`); with room it
/// prints the prefix and the trace, the same text `ThreadGroup` would.
///
/// `CRATONVM_LAUNCHER_PREMAIN_EXCEPTION_DESCRIBE`: unset or any value but an
/// off-word = on for every collector (`Vm::describe_launcher_exception`; it
/// was the ZGC heap only before the merge with dev's launcher reporting),
/// `0` / `false` / `off` / `no` = off (the uncaught dispatch, as before).
pub(crate) fn premain_exception_describe() -> bool {
    match None::<std::ffi::OsString> {
        None => true,
        // zs s4/nd4 (`zgc-zs3rv2-wave-s2-review-residuals-CLOSED-20261019.md` §2):
        // the shared off-word rule, so `false` / `off` / `no` read as off
        // too (they read as on before).
        Some(v) => !cratonvm_types::flags::value_is_off(&v.to_string_lossy()),
    }
}

/// `CRATONVM_DBG=launcher-outcome` (`CRATONVM_DBG_LAUNCHER_OUTCOME=1`): the
/// launcher's own narration of how the run ended -- `[cratonvm] main-vm run()
/// returned Ok`, the `returned Err:` line for an uncaught exception already
/// reported, and the `[cratonvm] jdk mode: ...` stamp after a HotSpot launcher
/// text. HotSpot prints none of them, so a plain run does not either (nio21
/// wave 8 lane L4). Errors that are CratonVM's own (a refused `--java-home`,
/// an internal error) keep their prefix and mode stamp unconditionally.
pub(crate) fn launcher_outcome_lines_on() -> bool {
    false
}

/// `CRATONVM_DBG=exit-census` (`CRATONVM_DBG_EXIT_CENSUS=1`): the exit-time
/// census lines that used to print on any run whose counter was non-zero
/// (`set_field_by_name stores dropped`, the JVMS 6.5 uninstantiable-receiver
/// census, the descriptor-coercion census, `shutdown hooks: ran=`, the
/// `filechannel fast I/O` row, the statics-scan rows, the `swallowed VM
/// error(s)` warning). They fire on ordinary programs and HotSpot prints none
/// of them (nio21 wave 8 lane L4); every one is still available under this
/// token, and each keeps its own switch where it had one. The crates outside
/// `vm-cli` read the same key inline.
pub(crate) fn exit_census_on() -> bool {
    // r23 w1 L11: the one exit-report policy; `CRATONVM_EXIT_REPORT_FILE`
    // turns it on too (and receives the lines that go through it).
    cratonvm_types::exit_report::exit_report_on()
}

/// The report's lines from the text `Throwable.printStackTrace(PrintWriter)`
/// wrote: `Exception in thread "main" ` before the first line, as
/// `ThreadGroup.uncaughtException` prints it, and the line separator (`\r\n`
/// on Windows) dropped, since the launcher joins the lines itself. `None` when
/// the text has no header line to prefix.
pub(crate) fn uncaught_report_lines_from_printed_trace(text: &str, thread_name: &str) -> Option<Vec<String>> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let first = lines.first_mut()?;
    if first.is_empty() {
        return None;
    }
    *first = format!("Exception in thread \"{thread_name}\" {first}");
    Some(lines)
}

/// The name `ThreadGroup.uncaughtException` prints for the thread `ctx` runs
/// on: `Thread.getName()`, i.e. the `name` field, which a program may have
/// changed (`Thread.currentThread().setName(..)`). `"main"` when there is no
/// thread object or no readable name (nio21 wave 9 lane L4; it was always
/// `"main"`).
pub(crate) fn uncaught_thread_name(ctx: &mut dyn cratonvm_native_api::NativeContext) -> String {
    ctx.current_thread_object_if_any()
        .and_then(|t| match ctx.get_field_by_name(t, "name") {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        })
        .unwrap_or_else(|| "main".to_string())}

/// The line separator `printStackTrace(System.err)` would write: the
/// `line.separator` property (`\r\n` on Windows), `\n` when it is unset
/// (nio21 wave 9 lane L4; the launcher's render always wrote `\n`).
pub(crate) fn uncaught_line_separator(ctx: &dyn cratonvm_native_api::NativeContext) -> String {
    ctx.get_system_property("line.separator")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "\n".to_string())
}

/// Render `exc` the way HotSpot does for an exception escaping `main`: run the
/// throwable's own `printStackTrace(PrintWriter)` into a `StringWriter`
/// (`StackTraceElement.toString()` for every frame, so the
/// `java.base/` module prefix and loader names; `Suppressed:` blocks; the whole
/// cause chain with `[CIRCULAR REFERENCE: ..]`), then read the text back.
///
/// Returns the report lines and the throwable's current address (the calls run
/// Java and can collect; `exc` is pinned across them). `None` when any step
/// fails (no JDK, an allocation failure, `printStackTrace` itself throwing):
/// the caller then falls back to the Rust renderer.
pub(crate) fn uncaught_report_via_print_stack_trace(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    exc: cratonvm_types::ObjectRef,
) -> (cratonvm_types::ObjectRef, Option<Vec<String>>) {
    let base = ctx.pin_native_root(exc);
    let thread_name = uncaught_thread_name(ctx);
    let text = (|| -> Option<String> {
        let sw = match ctx.new_object_initialized("java/io/StringWriter", "()V", &[]) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => return None,
        };
        let sw_pin = ctx.pin_native_root(sw);
        let sw = ctx.read_native_pin(sw_pin, sw);
        let pw = match ctx.new_object_initialized(
            "java/io/PrintWriter",
            "(Ljava/io/Writer;)V",
            &[Value::Object(Some(sw))],
        ) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => return None,
        };
        let pw_pin = ctx.pin_native_root(pw);
        let exc_now = ctx.read_native_pin(base, exc);
        let pw = ctx.read_native_pin(pw_pin, pw);
        ctx.invoke_virtual(
            exc_now,
            "printStackTrace",
            "(Ljava/io/PrintWriter;)V",
            &[Value::Object(Some(pw))],
        )
        .ok()?;
        let pw = ctx.read_native_pin(pw_pin, pw);
        ctx.invoke_virtual(pw, "flush", "()V", &[]).ok()?;
        let sw = ctx.read_native_pin(sw_pin, sw);
        match ctx.invoke_virtual(sw, "toString", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
            _ => None,
        }
    })();
    let exc_now = ctx.read_native_pin(base, exc);
    ctx.unpin_native_roots(base);
    (
        exc_now,
        text.as_deref()
            .and_then(|t| uncaught_report_lines_from_printed_trace(t, &thread_name)),
    )
}

/// Read the effective `-Xmx` before the no-VM version-banner fast path.
/// Only launcher options count; option operands and Java arguments are skipped.
/// The maximum heap the parsed path will use, read before it: the explicit
/// one ([`early_max_heap`]), else the ergonomic default
/// (`ergonomic_default_max_heap`, of the container limit unless
/// `-XX:-UseContainerSupport`), else the library default.
pub(crate) fn early_effective_max_heap(argv: &[String]) -> usize {
    early_max_heap(argv).map(|(size, _)| size).unwrap_or_else(|| {
        let container_support = !argv
            .iter()
            .skip(1)
            .take_while(|a| a.as_str() != "--")
            .any(|a| a == "-XX:-UseContainerSupport");
        let limit = container_support
            .then(|| cratonvm_vm::runtime::container::detect_container().memory_limit)
            .flatten();
        cratonvm_vm::runtime::container::ergonomic_default_max_heap(limit)
            .unwrap_or(cratonvm_vm::VmConfig::default().max_heap_size)
    })
}

pub(crate) fn early_max_heap(argv: &[String]) -> Option<(usize, String)> {
    let mut value = None;
    let mut i = 1;
    while i < argv.len() {
        let arg = argv[i].as_str();
        if arg == "--" || !arg.starts_with('-') {
            break;
        }
        if arg == "-Xmx" || arg == "--Xmx" {
            value = argv
                .get(i + 1)
                .and_then(|s| parse_max_heap_size(s).map(|size| (size, format!("-Xmx{s}"))));
            i += 2;
            continue;
        }
        if let Some(raw) = arg
            .strip_prefix("-Xmx")
            .or_else(|| arg.strip_prefix("--Xmx="))
            .or_else(|| arg.strip_prefix("-XX:MaxHeapSize="))
        {
            value = parse_max_heap_size(raw).map(|size| {
                (
                    size,
                    if arg.starts_with("--Xmx=") {
                        format!("-Xmx{raw}")
                    } else {
                        arg.to_string()
                    },
                )
            });
        }
        i += if VALUE_TAKING_OPTS.contains(&arg) {
            2
        } else {
            1
        };
    }
    value}

/// The last explicit MaxMetaspaceSize, in HotSpot's unsigned-option syntax.
/// Version queries need this before printing their banner because HotSpot
/// initializes the VM first and `MaxMetaspaceSize=0` fails that initialization.
pub(crate) fn early_max_metaspace_size(argv: &[String]) -> Option<u64> {
    argv.iter()
        .skip(1)
        .take_while(|a| a.as_str() != "--")
        .filter_map(|a| a.strip_prefix("-XX:MaxMetaspaceSize="))
        .last()
        .and_then(hotspot_parse_uintx)
}

// The general size parser deliberately rejects zero. Preserve it here so
// the maximum-heap validator can render HotSpot's distinct zero-size error.
// zt t18/tg8: under `CRATONVM_LAUNCHER_MEMORY_SIZE_OPTIONS`, HotSpot's syntax
// ([`parse_max_heap_size_with`]).
pub(crate) fn parse_max_heap_size(raw: &str) -> Option<usize> {
    parse_max_heap_size_with(raw, launcher_memory_size_options_enabled())
}

/// [`parse_max_heap_size`] with the switch passed: `hotspot_syntax` reads
/// `raw` as HotSpot 25's `parse_memory_size` does (`atojulong`: decimal or
/// `0x` hex digits, one optional `k` / `m` / `g` / `t`, within 64 bits;
/// [`hotspot_parse_uintx_flag`]), so `-Xmx1t` and `-Xmx0x4000000` are
/// sizes, as on HotSpot (local 25.0.3 boots `-Xmx1t -XX:SoftMaxHeapSize=64g
/// -version`). Off, the older syntax (`k` / `m` / `g`, decimal). Pure.
pub(crate) fn parse_max_heap_size_with(raw: &str, hotspot_syntax: bool) -> Option<usize> {
    if hotspot_syntax {
        return hotspot_parse_uintx_flag(raw.trim()).and_then(|v| usize::try_from(v).ok());
    }
    let raw = raw.trim();
    let number = if matches!(
        raw.as_bytes().last(),
        Some(b'k' | b'K' | b'm' | b'M' | b'g' | b'G')
    ) {
        &raw[..raw.len() - 1]
    } else {
        raw
    };
    if number.parse::<usize>().ok() == Some(0) {
        Some(0)
    } else {
        parse_size(raw)
    }
}

/// zt t18/tg8: an `-Xms` / `-Xss` value as the parsed path reads it: in
/// HotSpot's syntax under `CRATONVM_LAUNCHER_MEMORY_SIZE_OPTIONS`
/// (`hotspot_syntax`; [`hotspot_parse_uintx_flag`], so `t` and `0x` hex),
/// else [`parse_size`]'s (which refuses zero). Pure.
pub(crate) fn launcher_memory_size(raw: &str, hotspot_syntax: bool) -> Option<usize> {
    if hotspot_syntax {
        hotspot_parse_uintx_flag(raw).and_then(|v| usize::try_from(v).ok())
    } else {
        parse_size(raw)
    }
}

pub(crate) fn validate_minimum_max_heap(size: usize, argument: &str) -> Result<()> {
    if size == 0 {
        eprintln!("Invalid maximum heap size: {argument}\nError: Could not create the Java Virtual Machine.\nError: A fatal exception has occurred. Program will exit.");
        let _ = std::io::Write::flush(&mut std::io::stderr());
        return Err(UncaughtReported("maximum heap size refused".into()).into());
    }
    if size < 2 * 1024 * 1024 {
        let report = cratonvm_vm::error::VmInitializationError {
            detail: "Too small maximum heap".into(),
        }
        .report();
        print!("{report}");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        return Err(UncaughtReported("maximum heap size refused".into()).into());
    }
    Ok(())
}

/// `CRATONVM_LAUNCHER_MEMORY_SIZE_OPTIONS` (zt t18/tg8, page `zgc-zt12tg5-*`
/// item 5; default ON, `=0` kill). HotSpot 25 parses `-Xmx` /
/// `-XX:MaxHeapSize=`, `-Xms` and `-Xss` in `Arguments::parse_each_vm_init_arg`,
/// in command-line order with the other VM options, with `parse_memory_size`
/// (`k` / `m` / `g` / `t`, `0x` hex). Under this switch:
///
/// - their malformed values join the ordered first-refusal scan
///   ([`memory_size_option_refusal_of`]) with HotSpot's lines (`Invalid
///   maximum heap size: <option>`, `Invalid initial heap size: <option>`,
///   `Invalid thread stack size: <option>`, and for an `-Xss` above 1 GiB
///   `The specified size exceeds the maximum representable size.`), so the
///   first bad option is the one named: `-Xmx1x -XX:ZProactive=false` names
///   `-Xmx1x`. The launcher refused a malformed `-Xmx` after every other arm
///   with its own `Invalid heap size: <raw>` error, a malformed `-Xms` with
///   `Invalid initial heap size: <raw>`, and warned about a malformed `-Xss`;
/// - the values take HotSpot's syntax ([`parse_max_heap_size_with`]):
///   `-Xmx1t` is 1 TiB, as on every HotSpot collector (the syntax is the
///   argument parser's, not a collector's; `-Xmx1024g` already reached every
///   collector here); `-Xms0` and `-Xss0` are "not set" (the ergonomic
///   initial heap, the default stack), where `-Xms0` was refused and `-Xss0`
///   warned about.
///
/// This launcher's separate-value spellings (`-Xmx 256m`, `-Xms 256m`; Maven
/// Surefire splits them) are kept: a bare `-Xmx` / `-Xms` is not refused.
/// Acts on every collector; the ordered scan runs only under
/// [`launcher_zgc_flag_hotspot_parse_enabled`].
pub(crate) fn launcher_memory_size_options_enabled() -> bool {
    true
}

/// zt t18/tg8 (`CRATONVM_LAUNCHER_MEMORY_SIZE_OPTIONS`): HotSpot 25's parse
/// refusal of ONE `-Xmx<size>` / `-XX:MaxHeapSize=<size>` / `-Xms<size>` /
/// `-Xss<size>` argument, or `None` when HotSpot takes it (or it is none of
/// them). `arguments.cpp`: `-Xmx` and `-XX:MaxHeapSize=` are one branch,
/// `parse_memory_size(tail, &size, 1)` (zero is too small); `-Xms`,
/// `parse_memory_size(tail, &size, 0)`; `-Xss`, `parse_xss`: 0 to 1 GiB
/// (`ThreadStackSize`'s range `[0, 1M]` in K). A value outside the syntax
/// (`atojulong`) or below the minimum prints only the `Invalid ...` line; one
/// above the maximum adds `describe_range_error`'s line. A bare `-Xss` is
/// HotSpot's refusal; so are the bare `-Xmx` / `-Xms` (zt t31/x10,
/// [`launcher_bare_size_options_refused`]; under its kill they are this
/// launcher's separate-value spellings and left alone). Pure but for that
/// switch.
pub(crate) fn memory_size_option_refusal_of(arg: &str) -> Option<Vec<String>> {
    if (arg == "-Xmx" || arg == "-Xms") && !launcher_bare_size_options_refused() {
        return None;
    }
    let (what, tail, min, max) = if let Some(tail) = arg
        .strip_prefix("-Xmx")
        .or_else(|| arg.strip_prefix("-XX:MaxHeapSize="))
    {
        ("maximum heap size", tail, 1u64, u64::MAX)
    } else if let Some(tail) = arg.strip_prefix("-Xms") {
        ("initial heap size", tail, 0, u64::MAX)
    } else if let Some(tail) = arg.strip_prefix("-Xss") {
        ("thread stack size", tail, 0, 1u64 << 30)
    } else {
        return None;
    };
    let mut lines = vec![format!("Invalid {what}: {arg}")];
    match hotspot_parse_uintx_flag(tail) {
        Some(v) if (min..=max).contains(&v) => return None,
        Some(v) if v > max => {
            lines.push("The specified size exceeds the maximum representable size.".to_string())
        }
        _ => {}
    }
    Some(lines)
}



/// `CRATONVM_LAUNCHER_BARE_SIZE_OPTIONS_REFUSED` (zt t31/x10, page
/// `zgc-zt13x10-review-residuals-*` item 28f; default ON, `=0` kill). HotSpot
/// 25 has no separate-value spelling of `-Xmx` / `-Xms` / `-Xmn`: the bare
/// option is `parse_memory_size("")`, refused with `Invalid maximum heap
/// size: -Xmx` (`initial heap size: -Xms`, `initial young generation size:
/// -Xmn`) and exit 1, so `-Xmx 64m` never reaches its value (rows
/// `x10_xm_*`). This launcher took the next argument as the value. Its own
/// double-dash `--Xmx <size>` keeps the separate value. `=0`: the
/// single-dash bare forms take the next argument again.
pub(crate) fn launcher_bare_size_options_refused() -> bool {
    true
}

/// zt t21/tg10 (`CRATONVM_LAUNCHER_XMN_REFUSAL`): HotSpot 25's parse refusal
/// of ONE `-Xmn<size>` argument, or `None` when HotSpot takes it (or it is
/// not one). `arguments.cpp`: `parse_memory_size(tail, &size, 1)` (zero is
/// too small), then `Invalid initial young generation size: <option>` and
/// `describe_range_error` (nothing for an unreadable or too small value; no
/// maximum below `julong`'s). The bare `-Xmn` is refused too, as the bare
/// `-Xms` (zt t31/x10, [`launcher_bare_size_options_refused`]). Pure but for
/// that switch.
pub(crate) fn young_size_option_refusal_of(arg: &str) -> Option<Vec<String>> {
    if arg == "-Xmn" && !launcher_bare_size_options_refused() {
        return None;
    }
    let tail = arg.strip_prefix("-Xmn")?;
    match hotspot_parse_uintx_flag(tail) {
        Some(v) if v >= 1 => None,
        _ => Some(vec![format!("Invalid initial young generation size: {arg}")]),
    }
}





/// zt t18/tg8: the last initial and minimum heap sizes the leading section of
/// `argv` gives (`-Xms<size>` / `-Xms <size>` set both,
/// `-XX:InitialHeapSize=` the initial, `-XX:MinHeapSize=` the minimum), in
/// HotSpot's syntax; a value it cannot read is `None` (HotSpot refused it
/// while parsing). The section is read as [`early_max_heap`] reads it. Pure.
pub(crate) fn early_initial_and_min_heap(argv: &[String]) -> (Option<u64>, Option<u64>) {
    let mut initial = None;
    let mut min = None;
    let mut i = 1;
    while let Some(arg) = argv.get(i).map(String::as_str) {
        if arg == "--" || !arg.starts_with('-') {
            break;
        }
        let both = if arg == "-Xms" || arg == "--Xms" {
            Some(argv.get(i + 1).map(String::as_str).unwrap_or(""))
        } else {
            arg.strip_prefix("-Xms")
                .or_else(|| arg.strip_prefix("--Xms="))
        };
        if let Some(raw) = both {
            let size = hotspot_parse_uintx_flag(raw);
            initial = size;
            min = size;
        } else if let Some(raw) = arg.strip_prefix("-XX:InitialHeapSize=") {
            initial = hotspot_parse_uintx_flag(raw);
        } else if let Some(raw) = arg.strip_prefix("-XX:MinHeapSize=") {
            min = hotspot_parse_uintx_flag(raw);
        }
        i += if VALUE_TAKING_OPTS.contains(&arg) {
            2
        } else {
            1
        };
    }
    (initial, min)}

/// zt t18/tg8: `GCArguments::initialize_heap_flags_and_sizes`'s first checks,
/// for a maximum heap given on the command line: the detail line of the VM
/// initialization error, or `None`. Pure.
pub(crate) fn initial_or_min_heap_above_max_refusal(
    max: u64,
    initial: Option<u64>,
    min: Option<u64>,
) -> Option<&'static str> {
    if initial.is_some_and(|initial| initial > max) {
        return Some("Initial heap size set to a larger value than the maximum heap size");
    }
    if min.is_some_and(|min| max < min) {
        return Some("Incompatible minimum and maximum heap sizes specified");    }
    None
}

/// zt t18/tg8: the launcher's early heap-size checks on the leading section
/// of `argv`, in HotSpot's order: under
/// `CRATONVM_LAUNCHER_INITIAL_ABOVE_MAX_HEAP_REFUSED` the initial / minimum
/// heap against the maximum ([`initial_or_min_heap_above_max_refusal`]),
/// then the maximum's own (`Invalid maximum heap size` for 0, `Too small
/// maximum heap`; [`validate_minimum_max_heap`]).
pub(crate) fn validate_early_heap_sizes(argv: &[String]) -> Result<()> {
    if let Some((size, argument)) = early_max_heap(argv) {
        {
            let (initial, min) = early_initial_and_min_heap(argv);
            if let Some(detail) = initial_or_min_heap_above_max_refusal(size as u64, initial, min)
            {
                exit_with_vm_initialization_error(detail);
            }
        }
        validate_minimum_max_heap(size, &argument)?;
    }
    // zt t26/rt2 (`CRATONVM_LAUNCHER_TOO_SMALL_INITIAL_HEAP_REFUSED`): then
    // the initial and the minimum heap's own floors.
    {
        let (initial, min) = early_initial_and_min_heap(argv);
        if let Some(detail) = too_small_initial_or_min_heap_refusal(initial, min) {
            exit_with_vm_initialization_error(detail);
        }
    }
    // zt t27/lg1 (`CRATONVM_LAUNCHER_ZGC_HEAP_TOO_LARGE_REFUSED`): then ZGC's
    // own ceiling, at its heap initialisation.
    if scan_effective_zgc(argv) {
        if let Some(detail) = early_max_heap(argv)
            .and_then(|(size, _)| zgc_heap_too_large_refusal(size as u64))
        {
            exit_with_vm_initialization_error(&detail);        }    }
    Ok(())
}



/// zt t27/lg1: the detail line of HotSpot 25 ZGC's ceiling check for a
/// maximum heap of `max_heap` bytes, or `None` when ZGC takes it. Pure.
pub(crate) fn zgc_heap_too_large_refusal(max_heap: u64) -> Option<String> {
    const G: u64 = 1 << 30;
    let ceiling = cratonvm_vm::runtime::unified_logging::ZGC_MAX_SUPPORTED_HEAP_BYTES;
    (max_heap > ceiling).then(|| {
        format!(
            "Java heap too large (max supported heap size is {}G)",
            ceiling / G
        )
    })
}

/// zt t27/lg1 (`CRATONVM_ZGC_MAX_MAP_COUNT_WARNING`, host row `tg8_s2`:
/// `-Xmx1t -XX:SoftMaxHeapSize=64g -version`): HotSpot creates the VM before
/// a version banner, so ZGC's `max_map_count` warning prints first, in the
/// default shape on stdout. A version query on this launcher's no-VM fast
/// path prints those lines itself: an effective ZGC command line with a
/// maximum heap given, on Linux, and no `-Xlog:disable` (which turns
/// HotSpot's default output off; any other `-Xlog` boots the VM, which
/// prints them). Nothing otherwise.
pub(crate) fn print_version_zgc_max_map_count_warning(argv: &[String], uptime_secs: f64) {
    use std::io::Write as _;
    if !scan_effective_zgc(argv)
        || argv
            .iter()
            .skip(1)
            .take_while(|a| a.as_str() != "--")
            .any(|a| a == "-Xlog:disable")
    {
        return;
    }
    let Some((max_heap, _)) = early_max_heap(argv) else {
        return;
    };
    let backing = cratonvm_vm::runtime::unified_logging::zgc_linux_backing_facts();
    let lines = cratonvm_vm::runtime::unified_logging::zgc_max_map_count_warning(
        max_heap as u64,
        backing.as_ref(),
    );
    if lines.is_empty() {
        return;
    }
    let mut out = std::io::stdout().lock();
    for text in &lines {
        let _ = writeln!(
            out,
            "{}",
            cratonvm_vm::runtime::unified_logging::gc_warning_default_line(uptime_secs, 2, text)
        );
    }
    let _ = out.flush();
}



/// zt t26/rt2: the detail line of HotSpot's floor checks on the initial and
/// the minimum heap given on the command line ([`early_initial_and_min_heap`];
/// `None` or 0 is not set), or `None`. Initial first, as HotSpot checks.
/// Pure.
pub(crate) fn too_small_initial_or_min_heap_refusal(
    initial: Option<u64>,
    min: Option<u64>,
) -> Option<&'static str> {
    const M: u64 = 1024 * 1024;
    let too_small = |v: Option<u64>| v.is_some_and(|v| v != 0 && v < M);
    if too_small(initial) {
        return Some("Too small initial heap");
    }
    if too_small(min) {
        return Some("Too small minimum heap");    }
    None
}

/// zt t18/tg8 (`CRATONVM_LAUNCHER_VM_OPTION_PARSE_ORDER`): the launch argv
/// cut to what HotSpot's `Arguments::parse` reads, before the module launch
/// is resolved: the options before a `-m` / `--module` / `--module=` module
/// (stepped as [`rewrite_module_launch`] steps them), then a `--`; without a
/// module launch, [`insert_program_args_separator`]'s argv (the leading
/// section ends at its `--`). Pure.
pub(crate) fn vm_option_parse_view(argv: &[String]) -> Vec<String> {
    let mut i = 1usize;
    while let Some(a) = argv.get(i).map(String::as_str) {
        if a == "-m" || a == "--module" || a.starts_with("--module=") {
            let mut view = argv[..i].to_vec();
            view.push("--".to_string());
            return view;
        }
        if a == "--"
            || !a.starts_with('-')
            || a == "-jar"
            || a == "--jar"
            || a.starts_with("-jar=")
            || a.starts_with("--jar=")
        {
            break;
        }
        i += if a == "-p" || a == "--module-path" || VALUE_TAKING_OPTS.contains(&a) {
            2
        } else {
            1
        };
    }
    insert_program_args_separator(argv.to_vec())}

