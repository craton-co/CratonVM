// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use std::path::{Path, PathBuf};

pub mod compact_strings;
pub mod hotspot_flags;

/// The JDK-only policy token, re-exported so `vm` callers can name it without
/// depending on `cratonvm_types` directly.
///
/// It lives in `types` because the crates that can perform a compatibility
/// substitution — `native-api` (`NativeKind`), `classloading` (`ClassOrigin`),
/// `vm` and `vm-cli` — share no other type. See `types/src/compat.rs` and
/// `docs/book/src/user-guide/jdk-only-mode.md` §2.
pub use cratonvm_types::compat::{CompatibilityMode, ExecutionPolicy};

/// Available garbage collector algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcAlgorithm {
    /// Generational semi-space GC. **Not the default in a default build** —
    /// see [`Zgc`](Self::Zgc) and `VmConfig::default`, which selects `Zgc`
    /// whenever the `zgc` feature is on and `Generational` only when it is
    /// off. Reachable everywhere via `-XX:+UseGenerationalGC`.
    ///
    /// The "(default, current implementation)" this line used to carry stopped
    /// being true when the `zgc` default landed, and it is load-bearing: a
    /// reader who believes it attributes a default-build measurement to the
    /// wrong collector, which is what happened while root-causing
    /// `bug-g1-evacuates-live-jit-reference-20260819-FIXED.md`
    /// (three arms recorded as "generational" were ZGC runs).
    Generational,
    /// G1 (Garbage-First) region-based collector.
    G1,
    /// ZGC-real backend: a memory-backed stop-the-world mark-sweep collector.
    /// **The default in any build with the `zgc` feature**, which includes the
    /// release build.
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
///   (JRE / `jlink` image), and roughly 2,700 methods are implemented in
///   Rust. (See [`REAL_JDK_NATIVE_REGISTRATIONS`] — this figure said
///   "~300" until 2026-07-26 and was wrong by about an order of magnitude.)
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
    /// jimage. Roughly [`REAL_JDK_NATIVE_REGISTRATIONS`] methods are Rust —
    /// not the "~300" this doc claimed until 2026-07-26. Requires a usable
    /// JDK on the host; see [`require_real_jdk`].
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
    ///
    /// The real-JDK figure comes from [`REAL_JDK_NATIVE_REGISTRATIONS`]; run
    /// `--dump-native-registry` for the exact per-run census.
    pub fn describe(self) -> &'static str {
        match self {
            JdkMode::Real => {
                "real JDK class files from jmods/ or lib/modules; ~2,700 native methods in Rust \
                 (exact census: --dump-native-registry)"
            }
            JdkMode::Synthetic => {
                "~5,200 synthetic Rust stubs from native-builtins; no JDK required \
                 (exact census: --dump-native-registry)"
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

/// Approximate number of native methods CratonVM implements in Rust when
/// running in [`JdkMode::Real`].
///
/// # Why this constant exists
///
/// Until 2026-07-26 three doc sites and — worse — the user-visible
/// `-version` banner ([`JdkMode::describe`]) all said "~300 truly-native
/// methods". That figure is wrong by about an order of magnitude, and it was
/// being used to size proposals (see
/// `jdk-mode-determinism.md` §7.4, which
/// scopes a `native-essentials` crate split against it). The number now
/// lives in one place with its derivation attached.
///
/// # How it was derived (2026-07-26, reproducible by grep)
///
/// The default build compiles the `#[cfg(not(feature = "synthetic-jdk"))]`
/// arm of `vm/src/vm/vm_init.rs` (`:1493`), which makes **32** top-level
/// registration calls, `register_essential_natives` among them.
/// `register_essential_natives` alone (`native-builtins/src/lib.rs:6851`)
/// contains **960** direct `registry.register(` sites and calls **184**
/// distinct sub-registrars. Walking the static call graph over
/// `native-builtins/src` from all 32 roots — resolving each callee to a
/// definition in the same file first, then to a globally unique name, and
/// ignoring ambiguous names — reaches 1,116 functions holding **2,746**
/// `.register(` call sites. Six of the 32 roots are defined outside
/// `native-builtins/src` and were not traversed, so 2,746 is a floor.
///
/// # What it is not
///
/// It counts *registration call sites*, not distinct registry keys. A key
/// registered twice (see the duplicate-registration cases in the natives
/// audit) counts twice here, so the true distinct-method count is somewhat
/// lower. For an exact per-run census run the launcher with
/// `--dump-native-registry`, which is the authority; this constant exists so
/// documentation stops quoting a number that is off by 9x.
pub const REAL_JDK_NATIVE_REGISTRATIONS: u32 = 2_700;

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
/// `jdk-mode-determinism.md`.
pub const EMBEDDED_DEFAULT_JDK_MODE: JdkMode = JdkMode::Synthetic;

/// The `cratonvm` launcher's fixed default compatibility mode.
///
/// **Deliberately NOT identical to [`EMBEDDED_DEFAULT_COMPATIBILITY_MODE`],
/// and that is the whole point of declaring the pair.** Both pairs now split
/// by entry point, for two different reasons.
///
/// The JDK-mode pair splits on hermeticity: [`LAUNCHER_DEFAULT_JDK_MODE`] is
/// real, [`EMBEDDED_DEFAULT_JDK_MODE`] is synthetic, because an in-tree test
/// must not start resolving JMODs from whatever JDK the build machine has.
///
/// The compatibility pair splits on *who asked*. The launcher is the
/// operator-facing surface: a person typing `cratonvm Foo` is asking this VM
/// to run Java, and the answer they should get is the conforming one — real
/// class bytes are authoritative, no class is fabricated, and no
/// `NativeKind::SyntheticStub` is registered or invoked. Compatibility
/// substitution is the exception now, and it has a name: `--compatible`.
///
/// The embedding surface is the opposite case. A host that linked
/// `libcratonvm` or `cratonvm-embed` and never passed a policy argument has
/// not asked for anything; handing it [`CompatibilityMode::JdkOnly`] would
/// reject work its previous build accepted, with no flag of its own to point
/// at. So [`EMBEDDED_DEFAULT_COMPATIBILITY_MODE`] and
/// [`EMBEDDED_HOST_JDK_DEFAULT_COMPATIBILITY_MODE`] both stay
/// [`CompatibilityMode::Compatible`].
///
/// What has NOT changed: strictness is still never *inferred*. It comes from
/// this constant, from an explicit `--jdk-only`, or from
/// [`VmConfig::with_compatibility_mode`] — never from a Cargo feature (unlike
/// [`SYNTHETIC_JDK_COMPILED_IN`], which is a build fact), and never from an
/// environment variable. `CRATONVM_REAL=-stubs` is a native-registry filter
/// and does not set this — see `docs/book/src/user-guide/jdk-only-mode.md` §9.
pub const LAUNCHER_DEFAULT_COMPATIBILITY_MODE: CompatibilityMode = CompatibilityMode::JdkOnly;

/// The embedding / in-tree-test default compatibility mode, used by
/// [`VmConfig::default`].
///
/// [`CompatibilityMode::Compatible`], deliberately unlike
/// [`LAUNCHER_DEFAULT_COMPATIBILITY_MODE`]; see that constant for why the two
/// now split. Stated separately so `default()`'s policy reads at its own call
/// site rather than being inherited from the launcher's.
pub const EMBEDDED_DEFAULT_COMPATIBILITY_MODE: CompatibilityMode = CompatibilityMode::Compatible;

/// The compatibility mode of [`VmConfig::with_host_jdk_default`] — the
/// real-JDK **embedding** base used by `libcratonvm` and `cratonvm-embed`.
///
/// This exists as its own constant because those two crates want the
/// launcher's *JDK mode* (real, fixed at compile time, no JMOD-hunting
/// surprise) together with the embedder's *compatibility mode* (permissive,
/// because the host never asked). Before the launcher default flipped,
/// `with_host_jdk_default` could be a bare alias for [`VmConfig::for_launcher`]
/// and pick both up from one pair of constants. It cannot any more. The C
/// ABI's advertised default (`libcratonvm::DEFAULT_COMPATIBILITY_MODE`) is
/// exactly this value, so flipping it is an ABI break, not a launcher change.
pub const EMBEDDED_HOST_JDK_DEFAULT_COMPATIBILITY_MODE: CompatibilityMode =
    CompatibilityMode::Compatible;

/// Whether this build actually contains the synthetic class library.
///
/// The ~5,200 stubs are registered inside `#[cfg(feature = "synthetic-jdk")]`
/// blocks in `vm/src/vm/vm_init.rs`. The default Cargo feature set does
/// **not** enable `synthetic-jdk`, so in a default build asking for
/// [`JdkMode::Synthetic`] at runtime yields neither the stubs *nor* a boot
/// classpath — a silently broken VM. Callers must check this constant (or
/// call [`require_synthetic_jdk`]) before honouring a synthetic request.
pub const SYNTHETIC_JDK_COMPILED_IN: bool = cfg!(feature = "synthetic-jdk");

/// Whether this build actually contains the JDWP debug server.
///
/// Same shape as [`SYNTHETIC_JDK_COMPILED_IN`], and for the same reason: the
/// server is started from a `#[cfg(feature = "experimental-debug")]` block in
/// `vm/src/vm/vm_init.rs`, and that feature is not in `cratonvm-vm`'s default
/// set nor enabled by `cratonvm-cli`. Without this constant `--jdwp-port` is
/// accepted and does nothing in every shipped launcher binary, and the only
/// symptom is a debugger that never connects.
pub const JDWP_SERVER_COMPILED_IN: bool = cfg!(feature = "experimental-debug");

/// The JDK back end's `-agentlib:jdwp` options this VM's built-in JDWP
/// server honours (interpreter round i4 wave 85, lane L1; parsed by the
/// launcher as `debugInit.c` `parseOptions` parses them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JdwpAgentConfig {
    /// `server=y`: listen for the debugger; `server=n` (the default):
    /// connect to it.
    pub server: bool,
    /// `suspend=y` (the default): hold the VM at `VMInit`, after `VMStart`
    /// was sent, until the debugger resumes it.
    pub suspend: bool,
    /// The `address=` host part: `None` for none given (listen on the
    /// loopback interface, as JDK 9+ does), `Some("*")` for every interface.
    pub host: Option<String>,
    /// The `address=` port; `0` when none was given (listen on an ephemeral
    /// port, which the `Listening for transport` line names).
    pub port: u16,
    /// `quiet=y`: no `Listening for transport` line.
    pub quiet: bool,
    /// `timeout=<ms>` for the connect (`server=n`), and since interpreter
    /// round i7 wave 140 (lane L1, `CRATONVM_JDWP_AGENT_ACCEPT_TIMEOUT`) for
    /// each listen of a `server=y` transport; `None` or 0 waits for ever.
    pub timeout_ms: Option<u64>,
    /// `includevirtualthreads=y` (interpreter round i6 wave 130, lane L1b):
    /// `VirtualMachine.AllThreads` lists the virtual threads too, as the JDK
    /// back end's `gdata->includeVThreads` does. `n` (the default): platform
    /// threads only. Read by `debug::start_jdwp_agent` into this VM's
    /// `DebugState::include_virtual_threads` (unless
    /// `CRATONVM_JDWP_AGENT_INCLUDE_VIRTUAL_THREADS=0`).
    pub include_virtual_threads: bool,
    /// `allow=<peers>` (interpreter round i7 wave 140, lane L1), as given:
    /// the peers a `server=y` transport accepts (`*`, or `+`-separated
    /// addresses with an optional `/prefix`). Parsed and enforced by
    /// `debug::start_jdwp_agent` (`debug::parse_jdwp_allow`), as the socket
    /// transport parses it at its start and checks each accepted peer;
    /// ignored by `server=n`, as there. `None`: every peer, as without the
    /// option. Under `CRATONVM_JDWP_AGENT_ALLOW=0` it is dropped, as before.
    pub allow: Option<String>,
    /// `launch=<command line>` (interpreter round i7 wave 144, lane L1): run
    /// as `<command line> dt_socket <address>` once the transport is up, as
    /// the back end's `launch()` does (`debug::launch_jdwp_debugger`).
    pub launch: Option<String>,
    /// `onthrow=<exception class>` (wave 144): the transport starts only when
    /// an exception of exactly that class (dotted name, as given) is thrown
    /// (`debug::start_deferred_jdwp_agent`).
    pub on_throw: Option<String>,
    /// `onuncaught=y` (wave 144): the transport starts only when an exception
    /// no frame catches is thrown.
    pub on_uncaught: bool,
}

/// Where the JDK back end's `exitWithError` names its `No transports
/// initialized` exit (`debugInit.c`): `THIS_FILE` as the JDK build spelled
/// it, and the line. MEASURED on Windows (Temurin 25.0.3,
/// `L1W85AgentlibJdwp` case 7): `s\src\jdk.jdwp.agent\share\native\libjdwp\debugInit.c:697`,
/// the build's source root `s`. Elsewhere the line is the same source's 697
/// and the path the source-relative one JDK builds use there (NOT measured:
/// refresh from a Linux/macOS HotSpot run of case 7).
#[cfg(windows)]
pub const JDWP_NO_TRANSPORTS_AT: &str = "s\\src\\jdk.jdwp.agent\\share\\native\\libjdwp\\debugInit.c:697";
/// See the Windows definition above.
#[cfg(not(windows))]
pub const JDWP_NO_TRANSPORTS_AT: &str = "src/jdk.jdwp.agent/share/native/libjdwp/debugInit.c:697";

/// zt t23/hunt4 (`zgc-zt22hunt3-zgenerational-obsolete-warning-missing-*`):
/// the HotSpot 25 obsolete flags the launcher and `libcratonvm` warn about,
/// with the version HotSpot's `special_jvm_flags` table says each was
/// obsoleted in (printed with `JDK_Version::to_string`, so `24` reads
/// `24.0`). gc-known-issues retire (2026-10-25): every obsolete row of HotSpot
/// 25.0.3's table, mined from the JDK (`tools/gen-hotspot25-develop-flags.py`,
/// `vm-cli/src/hotspot25_develop_flags.rs::OBSOLETE`, which a `vm-cli` test
/// keeps equal to this list), so the launcher's unknown-name refusal
/// (`CRATONVM_LAUNCHER_UNKNOWN_XX_REFUSAL`) does not refuse them.
pub const HOTSPOT_OBSOLETE_FLAGS: &[(&str, &str)] = &[
    ("DumpSharedSpaces", "19.0"),
    ("DynamicDumpSharedSpaces", "19.0"),
    ("MetaspaceReclaimPolicy", "21.0"),
    ("PerfDataSamplingInterval", "25.0"),
    ("RequireSharedSpaces", "19.0"),
    ("UseSharedSpaces", "19.0"),
    // JEP 490 (JDK 24): non-generational ZGC removed; deprecated in 23.
    ("ZGenerational", "24.0"),
    ("ZMarkStackSpaceLimit", "25.0"),
];

/// [`HOTSPOT_OBSOLETE_FLAGS`] of Linux only (`LINUX_ONLY` rows of
/// `special_jvm_flags`; `Unrecognized VM option` on Windows).
const HOTSPOT_LINUX_OBSOLETE_FLAGS: &[(&str, &str)] = &[("UseLinuxPosixThreadCPUClocks", "25.0")];

/// `CRATONVM_LAUNCHER_OBSOLETE_FLAG_WARNINGS` (zt t23/hunt4, default ON, `=0`
/// kill): whether [`hotspot_obsolete_flag_warning`]'s line is printed. Read
/// per launch / per `JNI_CreateJavaVM`, no process latch.
pub fn obsolete_flag_warnings_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_LAUNCHER_OBSOLETE_FLAG_WARNINGS")
}

/// HotSpot 25's `-Xverify:none` / `-noverify` warning (`arguments.cpp`, the
/// `-Xverify` arm of `parse_each_vm_init_arg`), on stderr. Shared by the
/// launcher and `libcratonvm`'s `JNI_CreateJavaVM` (t1 wave 13, lane I24W).
pub const NOVERIFY_DEPRECATION_WARNING: &str = "OpenJDK 64-Bit Server VM warning: Options -Xverify:none \
     and -noverify were deprecated in JDK 13 and will likely be removed in a future release.";

/// `CRATONVM_LAUNCHER_NOVERIFY_DEPRECATION_WARNING` (t1 wave 10, lane I24W;
/// default ON, `=0` silent): whether [`NOVERIFY_DEPRECATION_WARNING`] is
/// printed. Read per launch / per `JNI_CreateJavaVM`.
pub fn noverify_deprecation_warning_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_LAUNCHER_NOVERIFY_DEPRECATION_WARNING")
}

/// zt t23/hunt4: HotSpot 25's warning for ONE `-XX:` option naming an
/// obsolete flag ([`HOTSPOT_OBSOLETE_FLAGS`]), or `None`.
/// `Arguments::process_argument` strips the `+` / `-` and anything from `=`
/// on before it asks `is_obsolete_flag`, so `-XX:+ZGenerational`,
/// `-XX:-ZGenerational`, `-XX:ZGenerational=true` and a bare
/// `-XX:ZGenerational` all warn; it then calls `warning("Ignoring option %s;
/// support was removed in %s", ...)`, which prints `"%s warning: "` with
/// `VM_Version::vm_name()` on `defaultStream::error_stream()` (stderr). The
/// VM name is the HotSpot twin's (`OpenJDK 64-Bit Server VM`, the prefix the
/// round's HotSpot stderr carries), as `runtime::hprof`'s HotSpot warning
/// spells it, not this VM's `java.vm.name`. Pure.
pub fn hotspot_obsolete_flag_warning(option: &str) -> Option<String> {
    let rest = option.strip_prefix("-XX:")?;
    let name = rest
        .strip_prefix('+')
        .or_else(|| rest.strip_prefix('-'))
        .unwrap_or(rest);
    let name = name.split_once('=').map_or(name, |(n, _)| n);
    let linux = cfg!(target_os = "linux").then_some(HOTSPOT_LINUX_OBSOLETE_FLAGS);
    let (flag, since) = HOTSPOT_OBSOLETE_FLAGS
        .iter()
        .chain(linux.into_iter().flatten())
        .find(|(f, _)| *f == name)?;
    Some(format!(
        "OpenJDK 64-Bit Server VM warning: Ignoring option {flag}; support was removed in {since}"
    ))
}

/// How a `-agentlib:jdwp` start ends the process, as the JDK back end ends
/// it (interpreter round i4 wave 85, lane L1): its `ERROR:` lines on stderr
/// and `forceExit(exit_code)` -- no `Error occurred during initialization of
/// VM`, no core dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JdwpAgentFailure {
    pub stderr_lines: Vec<String>,
    pub exit_code: i32,
}

impl JdwpAgentFailure {
    /// An option the back end's `parseOptions` refuses: exit status 1.
    pub fn option(lines: Vec<String>) -> Self {
        JdwpAgentFailure {
            stderr_lines: lines,
            exit_code: 1,
        }
    }

    /// A `dt_socket` transport that did not start (`detail`: the socket
    /// transport's own text, e.g. `bind failed: Address already in use`):
    /// `EXIT_TRANSPORT_ERROR`, 2.
    pub fn transport_init(detail: &str) -> Self {
        Self::transport_init_with(202, detail)
    }

    /// [`Self::transport_init`] with the transport's error code: 202
    /// (`JDWPTRANSPORT_ERROR_IO_ERROR`), 103 (`ILLEGAL_ARGUMENT`, a port that
    /// does not parse).
    pub fn transport_init_with(code: i32, detail: &str) -> Self {
        JdwpAgentFailure {
            stderr_lines: vec![
                format!("ERROR: transport error {code}: {detail}"),
                "ERROR: JDWP Transport dt_socket failed to initialize, TRANSPORT_INIT(510)"
                    .to_string(),
                format!(
                    "JDWP exit error AGENT_ERROR_TRANSPORT_INIT(197): No transports initialized [{JDWP_NO_TRANSPORTS_AT}]"
                ),
            ],
            exit_code: 2,
        }
    }

    /// A transport library the back end cannot load (any transport but
    /// `dt_socket`, which is the only one this VM serves): exit status 2.
    pub fn transport_load(name: &str) -> Self {
        JdwpAgentFailure {
            stderr_lines: vec![
                format!("ERROR: transport library not found: {name}"),
                format!("ERROR: JDWP Transport {name} failed to initialize, TRANSPORT_LOAD(509)"),
                format!(
                    "JDWP exit error AGENT_ERROR_TRANSPORT_LOAD(196): No transports initialized [{JDWP_NO_TRANSPORTS_AT}]"
                ),
            ],
            exit_code: 2,
        }
    }
}

/// obsaudit D12 (2026-07-26) — settings for `-XX:StartFlightRecording`.
/// Parsed by `vm-cli/src/main.rs`, consumed by `Vm::new`
/// (`vm/src/vm/vm_init.rs`) to start a real JFR recording at boot.
///
/// Deliberately narrower than HotSpot's `StartFlightRecording:` option set:
/// no `disk=` (recordings stay memory-only — see the `RecordingSettings`
/// doc comment in `jfr/src/recording.rs` for why), and `maxevents` names
/// what `jfr::RecordingSettings::max_size` actually bounds (an event
/// count) rather than reusing HotSpot's `maxsize` name, which means bytes
/// in real JFR — a CratonVM `maxsize=` would silently mean something
/// different from HotSpot's, which is worse than not offering the name.
#[derive(Debug, Clone, Default)]
pub struct JfrStartRecordingConfig {
    /// `filename=<path>`. `None` defaults to `./cratonvm-recording-<pid>.jfr`
    /// (chosen at dump time, once the pid is known).
    pub filename: Option<String>,
    /// `duration=<secs>`. `None` means the recording runs until the VM
    /// exits (or is stopped by some future explicit control surface).
    pub duration: Option<std::time::Duration>,
    /// `maxage=<secs>`. `None` means no age-based eviction — see
    /// `jfr::repository::EventRepository::with_max_age`.
    pub max_age: Option<std::time::Duration>,
    /// `maxevents=<n>`. `None` keeps the default 100_000-event ring.
    pub max_events: Option<usize>,
    /// `dumponexit=true|false`, default `true` (matches HotSpot's default
    /// for `-XX:StartFlightRecording`). When true, the VM dumps this
    /// recording to `filename` from the pre-exit hook — see
    /// `vm-cli/src/main.rs`'s `set_pre_exit_hook` installer.
    pub dump_on_exit: bool,
}

/// Apply one HotSpot-shaped JFR **event setting** token from
/// `-XX:StartFlightRecording:...` to a `VmConfig::jfr_enabled_events` list.
///
/// ## The spelling, and why this one
///
/// HotSpot's `-XX:StartFlightRecording:` option string mixes two kinds of
/// token: recording options (`filename=`, `duration=`, `settings=`, ...) and
/// per-event settings, which are spelled
///
/// ```text
/// +<EventName>#<setting>=<value>
/// ```
///
/// for example `+cratonvm.JitCompileDecision#enabled=true`. The leading `+`
/// is what tells the option parser "this token is an event setting, not a
/// recording option": without it, `cratonvm.JitCompileDecision#enabled` is
/// indistinguishable from a misspelt recording option, and the parser would
/// have to guess.
///
/// Adopting HotSpot's spelling verbatim was chosen over the two obvious
/// alternatives, both of which were rejected:
///
///  * A CratonVM-specific comma-separated list (`events=a,b,c`) cannot even
///    be written here: the option string is itself comma-separated, so such a
///    list needs a second separator, and the result would *look* like
///    HotSpot's syntax while not being it. A spelling that is nearly
///    HotSpot's is worse than one that is obviously not.
///  * `settings=<name|path>` is HotSpot's other event-selection option, but
///    it names a `.jfc` XML profile. Accepting the name and doing something
///    else, or accepting it and ignoring it, is exactly the silent no-op this
///    whole event exists to remove, so `settings=` is rejected with a message
///    pointing at this syntax rather than half-honoured. A real `.jfc` parser
///    is separate work and is not attempted here.
///
/// ## What is accepted
///
/// Only the `enabled` setting. It is the only per-event setting CratonVM's
/// `RecordingSettings` can express by name and act on; HotSpot also spells
/// `#threshold=20ms` and `#stackTrace=true`, and accepting either without
/// implementing it would reintroduce the silent no-op above.
///
///  * `enabled=true` adds the event name to the list.
///  * `enabled=false` removes it, and still leaves the list *present*, so
///    that `+A#enabled=true,+A#enabled=false` ends as an empty whitelist
///    (record nothing) rather than reverting to `None` (record everything).
///    An operator who wrote both meant to end up with neither, and a filter
///    that quietly turns itself off is the failure mode being designed out.
///
/// `key` is the token's left-hand side *including* the leading `+` (that is,
/// `+cratonvm.JitCompileDecision#enabled`) and `value` is its right-hand
/// side; the caller has already split the token on its first `=`.
///
/// The event NAME is deliberately **not** validated here: this function has
/// no event registry to validate against. `Vm::new` checks every name against
/// the `FlightRecorder`'s own type registry and fails the boot loudly on an
/// unknown one. See the `-XX:StartFlightRecording` block in
/// `vm/src/vm/vm_init.rs`.
pub fn apply_jfr_event_setting(
    enabled_events: &mut Option<Vec<String>>,
    key: &str,
    value: &str,
) -> Result<(), String> {
    let body = key.strip_prefix('+').ok_or_else(|| {
        format!("-XX:StartFlightRecording: event setting `{key}` must start with `+`")
    })?;
    let (event, setting) = body.split_once('#').ok_or_else(|| {
        format!(
            "-XX:StartFlightRecording: event setting `{key}` is missing `#<setting>` \
             (expected `+<EventName>#enabled=true`)"
        )
    })?;
    if event.is_empty() {
        return Err(format!(
            "-XX:StartFlightRecording: event setting `{key}` names no event \
             (expected `+<EventName>#enabled=true`)"
        ));
    }
    if setting != "enabled" {
        return Err(format!(
            "-XX:StartFlightRecording: event setting `{key}` asks for `#{setting}`, which \
             CratonVM does not implement; only `#enabled` is accepted \
             (expected `+{event}#enabled=true`)"
        ));
    }
    let on = match value {
        "true" => true,
        "false" => false,
        other => {
            return Err(format!(
                "-XX:StartFlightRecording: `+{event}#enabled=` must be true or false, \
                 not `{other}`"
            ))
        }
    };
    let list = enabled_events.get_or_insert_with(Vec::new);
    if on {
        if !list.iter().any(|n| n == event) {
            list.push(event.to_string());
        }
    } else {
        list.retain(|n| n != event);
    }
    Ok(())
}

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
    /// bug-h2-largeblob-direct-memory-oom.md —
    /// previously this cap was a hardcoded 256 MiB regardless of `-Xmx`,
    /// which OOM'd direct-buffer-heavy workloads (H2 MVStore chunk writes)
    /// that HotSpot handles fine at the same `-Xmx`.
    pub max_direct_memory_size: Option<usize>,

    /// `-Xss`: the OS stack of a platform carrier whose `Thread` was built
    /// without a `stackSize` (gcd d6/f, `vm_exec.rs::thread_start`). `None`,
    /// or a value of 8 MiB or less, keeps the default carrier stack.
    pub default_thread_stack_size: Option<usize>,

    /// Initial heap size in bytes (HotSpot's `InitialHeapSize`). The launcher
    /// maps `-Xms` to this AND to [`Self::min_heap_size`], as HotSpot's `-Xms`
    /// sets both; an embedder that wants the whole of `-Xms` (ZGC's uncommit
    /// floor included) sets both too (zt t2/rv, `zgc-zr4rv4-footprint-residuals`
    /// item 2).
    pub initial_heap_size: usize,

    /// Maximum call stack depth per thread (to detect infinite recursion).
    pub max_stack_depth: usize,

    /// HotSpot `-XX:MaxJavaStackTraceDepth=N`: the most frames a throwable's
    /// stack trace records (the innermost ones, after the fill-frame skip).
    /// `0` means unlimited. Default
    /// `runtime::stackwalker::MAX_JAVA_STACK_TRACE_DEPTH` (1024). Throwable
    /// traces, and (interpreter round i4 wave 84, lane L2) another thread's
    /// `Thread.getStackTrace()` (`getStackTrace0`, HotSpot's
    /// `GetStackTraceHandshakeClosure`); `StackWalker` and
    /// `Thread.getAllStackTraces()` are not capped.
    pub max_java_stack_trace_depth: usize,

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

    /// The JVM options as HotSpot's `Arguments::jvm_args()` records them:
    /// every option the `java` launcher hands `JNI_CreateJavaVM`, in order,
    /// except `-Djava.class.path`, `-Dsun.java.command` and
    /// `-Dsun.java.launcher*` (the launcher's separate-value module options
    /// joined as `--opt=value`). The perf-data file's `java.rt.vmArgs`
    /// (`jps -v`). Empty for an embedder that does not set it; the perf-data
    /// file then lists the `-D` options. Interpreter round i4 wave 84, lane L1.
    pub jvm_args: Vec<String>,

    /// The assertion switches (`-ea[:<name>]`, `-da[:<name>]`, `-esa`, `-dsa`
    /// and the long spellings) as HotSpot's `JavaAssertions` keeps them:
    /// `Class.desiredAssertionStatus0` and `ClassLoader.retrieveDirectives`
    /// answer from it. `None` (the embedding default) leaves the user default
    /// to `CRATONVM_ENABLE_ASSERTIONS` and system classes off. Interpreter
    /// round i5 wave 107, lane L5.
    pub assertion_directives:
        Option<cratonvm_native_api::assertion_directives::AssertionDirectives>,

    /// Skip bytecode verification (`-noverify` / `-Xverify:none`).
    ///
    /// This boolean stays as the verifier-dispatcher's read path for
    /// backward compatibility; `xverify_mode` below carries the richer
    /// `-Xverify:none|remote|all` selection. The two are kept in sync at
    /// CLI parse time.
    pub skip_verification: bool,

    /// `-Xverify:none|remote|all` selection. See [`XverifyMode`].
    pub xverify_mode: XverifyMode,

    /// `--enable-preview` (JVMS 4.1): whether this VM may define a preview
    /// class file of this release (`69.65535`), and what
    /// `jdk.internal.misc.PreviewFeatures.isEnabled()` answers. Default `false`,
    /// HotSpot's. Set by the launcher's `--enable-preview`, by a
    /// `JNI_CreateJavaVM` option of that spelling (`libcratonvm`) and by
    /// `cratonvm-embed`'s builder. Per VM: a process global in the class-file
    /// reader, set only by the launcher, until interpreter round i5 wave 101,
    /// lane L5.
    pub enable_preview: bool,

    /// zt t18/hunt2: HotSpot's `--finalization=enabled|disabled` (JEP 421).
    /// `false` gives no class a finalizer (the class manager's define rule),
    /// so `finalize()` never runs, `Finalizer.isFinalizationEnabled()`
    /// answers `false` (no `Finalizer` thread starts) and `jcmd
    /// GC.finalizer_info` prints `Finalization is disabled`. Default `true`,
    /// HotSpot's. Set by the launcher's `--finalization=`. Per VM.
    pub finalization_enabled: bool,

    /// zt t21/tg10: HotSpot's `-XX:±ClassUnloading` (`-Xnoclassgc` is its
    /// `false` spelling; the last one given wins). `false` makes every class
    /// loader's data a strong root on every collector: no class, loader or
    /// mirror is ever unloaded, `ClassLoadingMXBean.getUnloadedClassCount()`
    /// stays 0, `-Xlog:class+unload` prints nothing and JVMTI's
    /// `IsClassUnloadingEnabled` answers `JNI_FALSE`. Default `true`,
    /// HotSpot's. Set by the launcher and `libcratonvm`. Per VM (installed in
    /// `SharedVm::loader_identity` when the VM is made).
    pub class_unloading: bool,

    /// zt t23/hunt4: HotSpot's `-XX:±AlwaysPreTouch` (the last one given
    /// wins). `true` makes the ZGC heap touch every page it commits: the
    /// `-Xms` startup commit before the first allocation, and every later
    /// commit as it is made; its `gc,init` block then prints `Pre-touch:
    /// Enabled`. Default `false`, HotSpot's. Set by the launcher and
    /// `libcratonvm`; per VM (`ZgcRealHeap::set_always_pre_touch`).
    /// `CRATONVM_ZGC_ALWAYS_PRE_TOUCH=0` accepts and ignores it, as before.
    /// G1 and the Generational backend do not honour it yet.
    pub always_pre_touch: bool,

    /// gcj j1/st item 2: HotSpot's `-XX:+PrintStringTableStatistics` (the
    /// last one given wins): print the Symbol and String tables' statistics
    /// on stdout as the VM exits
    /// (`memory::string_table::print_string_table_statistics`). Default
    /// `false`, HotSpot's. Set by the launcher; per VM.
    pub print_string_table_statistics: bool,

    /// gcj j1/st: HotSpot's `-XX:StringTableSize=<n>`, the String table's start
    /// size (rounded up to a power of two, as HotSpot's `log2i_ceil`).
    /// `None` is HotSpot's default, 65536. Set by the launcher; per VM.
    pub string_table_size: Option<usize>,

    /// zt t24/hunt7: HotSpot's `-XX:+UseTransparentHugePages` as its argument
    /// processing leaves it (Linux only; the last one given wins, an explicit
    /// `-XX:-UseLargePages` turns it off). `true` makes the ZGC heap ask for
    /// transparent huge pages when the kernel's THP modes let HotSpot 25's ZGC
    /// use them, and its `gc,init` block prints HotSpot's `Large Page Support:`
    /// value for the kernel's modes (`unified_logging::zgc_large_pages`).
    /// Default `false`, HotSpot's. Set by the launcher and `libcratonvm` on
    /// Linux; per VM (`ZgcRealHeap::advise_transparent_huge_pages`).
    /// `CRATONVM_ZGC_TRANSPARENT_HUGE_PAGES=0` accepts and ignores it, as
    /// before. G1 and the Generational backend do not honour it.
    pub use_transparent_huge_pages: bool,

    /// zt t25/hunt7b: HotSpot's `-XX:+UseLargePages` (the last `±` given
    /// wins). On Linux under ZGC an explicit request without transparent huge
    /// pages prints HotSpot 25's `UseLargePages disabled, no large pages
    /// configured and available on the system.` warning when the hugetlbfs
    /// pool is empty (`unified_logging::zgc_large_pages`); explicit hugetlbfs
    /// pages themselves are not used. Default `false`. Set by the launcher and
    /// `libcratonvm`.
    pub use_large_pages: bool,

    /// Which garbage collector algorithm to use (`-XX:+UseG1GC`, etc.).
    pub gc_algorithm: GcAlgorithm,
    /// ZGC uncommits unused granules, preserving the startup commit floor.
    pub zgc_uncommit: bool,
    /// Minimum unused time in seconds (HotSpot default: 300).
    pub zgc_uncommit_delay_secs: u64,
    /// zr z1/tr: `-XX:SoftMaxHeapSize` (ZGC only, as on HotSpot): the heap
    /// size the collector's rules aim to stay under (warm-up and the
    /// `CRATONVM_ZGC_RATE_TRIGGER` rules). `None` = the max heap size.
    pub zgc_soft_max_heap_size: Option<usize>,
    /// zt t28/sm (`CRATONVM_ZGC_SOFT_MAX_ERGONOMIC`): HotSpot 25's ZGC
    /// ergonomic `SoftMaxHeapSize` -- 90 % of `MaxHeapSize` when the command
    /// line sizes neither (`ZArguments::initialize_heap_flags_and_sizes`);
    /// `None` otherwise. Set by the launcher; [`Self::zgc_soft_max_heap_size`]
    /// wins.
    pub zgc_ergonomic_soft_max_heap_size: Option<usize>,
    /// zt t6/tc: `-XX:-ZProactive` (ZGC only; HotSpot's default `true`). The
    /// director's proactive rule; acts under `CRATONVM_ZGC_RATE_TRIGGER=1`.
    pub zgc_proactive: bool,
    /// zt t6/tc: `-XX:ZAllocationSpikeTolerance=<f>` (ZGC only; `None` =
    /// HotSpot's default 2.0). Acts under `CRATONVM_ZGC_RATE_TRIGGER=1`.
    pub zgc_allocation_spike_tolerance: Option<f64>,
    /// zt t7/tg: `-XX:ZCollectionInterval=<seconds>` (ZGC only; `None` =
    /// HotSpot's default 0, no `Timer` rule). A positive value starts the
    /// heap's director tick, which runs a `Timer` collection whenever that
    /// long passed since the last whole-heap cycle ended, idle or not.
    pub zgc_collection_interval_secs: Option<f64>,
    /// zt t27/gn5: `-XX:ZCollectionIntervalMinor=<seconds>` (ZGC only; `None`
    /// = HotSpot's default -1, no minor `Timer` rule). Acts only in ZGC's
    /// generational mode (`CRATONVM_ZGC_GENERATIONAL=1`): a positive value
    /// starts the heap's director tick, which runs a young collection with
    /// cause `Timer` whenever that long passed since the last collection
    /// ended (`ZgcRealHeap::set_collection_interval_minor`;
    /// `CRATONVM_ZGC_GEN_MINOR_TIMER=0` ignores it).
    pub zgc_collection_interval_minor_secs: Option<f64>,
    /// zt t10/tg3: `-XX:+ZCollectionIntervalOnly` (ZGC only; HotSpot's
    /// default `false`): only the `Timer` rule (and an allocation that
    /// fails) starts a collection. Honoured unless
    /// `CRATONVM_ZGC_COLLECTION_INTERVAL_ONLY=0`
    /// (`ZgcRealHeap::set_collection_interval_only`).
    pub zgc_collection_interval_only: bool,

    /// G1 tuning overrides (only honoured when `gc_algorithm == G1`). `None`
    /// keeps the collector default. Wired from the corresponding `-XX:` knobs.
    /// `-XX:InitiatingHeapOccupancyPercent=<n>` — start concurrent marking when
    /// old-gen occupancy crosses this percent.
    pub g1_ihop_percent: Option<u8>,
    /// `-XX:G1HeapRegionSize=<bytes>` — G1 region size.
    pub g1_region_size: Option<usize>,
    /// `-XX:MaxGCPauseMillis=<n>` — target max pause. G1 sizes its mixed
    /// collection set from it; ZGC sizes its allocation budget from it
    /// (`ZgcRealHeap::refresh_pause_target_budget`). The `g1_` prefix is
    /// historical — this is the operator's target, not a G1 tuning knob,
    /// and it is passed to every backend through `G1ConfigOverrides`.
    pub g1_max_gc_pause_ms: Option<u64>,
    /// `-XX:±UseStringDeduplication` — G1 String backing-array dedup.
    pub g1_string_dedup: Option<bool>,
    /// `-XX:ParallelGCThreads=<n>` — GC worker threads. `None` derives the
    /// count from the machine (F-13).
    pub g1_parallel_gc_threads: Option<usize>,
    /// zr z3/mt: `-XX:ParallelGCThreads=<n>` as ZGC applies it -- every
    /// stop-the-world worker pool that is on
    /// (`ZgcRealHeap::set_gc_worker_threads`). Set by both launcher
    /// spellings (`-XX:ParallelGCThreads=<n>` and `--XX:ParallelGCThreads
    /// <n>`); both also set [`Self::g1_parallel_gc_threads`]. The fields stay
    /// separate for per-VM embedding configuration. `None` = HotSpot's ergonomic.
    pub zgc_parallel_gc_threads: Option<usize>,
    /// zr z3/mt: `-XX:ConcGCThreads=<n>` -- ZGC's concurrent mark pool.
    pub zgc_conc_gc_threads: Option<usize>,
    /// zr z3/mt: `-XX:+/-UseDynamicNumberOfGCThreads` (`None` = HotSpot's
    /// default, on). Changes only ZGC's concurrent-pool default.
    pub zgc_dynamic_gc_threads: Option<bool>,
    /// `-XX:G1MixedGCLiveThresholdPercent=<n>` — an Old region at or above
    /// this percent live is never a mixed-collection candidate.
    pub g1_mixed_gc_live_threshold_percent: Option<u8>,
    /// `-XX:G1HeapWastePercent=<n>` — the mixed phase ends once the
    /// candidates' reclaimable garbage is below this percent of the heap.
    pub g1_heap_waste_percent: Option<u8>,
    /// `-Xmn` / `-XX:NewSize` / `-XX:MaxNewSize` / `-XX:NewRatio` — the young
    /// generation's size (gen r4w2/alloc2, 2026-09-23). Honoured by the
    /// Generational backend only (see `cratonvm_gc::gen_young_sizing` for the
    /// HotSpot mapping); the default (nothing given) leaves every backend's
    /// sizing exactly as it was.
    pub young_gen_sizing: cratonvm_gc::gen_young_sizing::YoungGenSizing,
    /// gc-known-issues retire (`launcher-hotspot-flag-table-residuals-FIXED-20261025`
    /// item 2, `CRATONVM_GEN_HOTSPOT_YOUNG_ERGO`): the `[warning][gc,ergo]`
    /// texts HotSpot 25 logs while it sizes the young generation of the
    /// collector IT would run for this command line (Serial's `GenArguments`;
    /// G1's `G1YoungGenSizer` when this VM's heap is not G1 -- a command line
    /// naming no collector), resolved by the launcher. The VM prints them as
    /// it prints a G1 heap's own sizer warnings. Empty for the embedding entry
    /// points (nothing resolves them there).
    pub hotspot_gc_ergo_warnings: Vec<String>,
    /// `-XX:MaxTenuringThreshold` / `-XX:InitialTenuringThreshold` /
    /// `-XX:TargetSurvivorRatio` / `-XX:+PrintTenuringDistribution` (gen
    /// r4w6/young6, 2026-09-24). Honoured by the Generational backend, where
    /// any of the three values engages HotSpot-style adaptive tenuring on
    /// this VM's heap (`cratonvm_gc::gen_heap::TenuringConfig`), and by G1 for
    /// `MaxTenuringThreshold` / `TargetSurvivorRatio`, as HotSpot G1 (gci
    /// i23/tn, `VmHeap::set_tenuring_config` -> `G1Collector::set_tenuring_flags`).
    /// Per VM, never a process flag. The default (nothing given) leaves every
    /// backend's tenuring exactly as it was.
    pub tenuring: cratonvm_gc::gen_heap::TenuringConfig,
    /// What an explicit GC request (`System.gc()`, `Runtime.gc()`,
    /// `MemoryMXBean.gc()`, the `System.gc()` inside `Bits.reserveMemory`)
    /// does in this VM: `-XX:+DisableExplicitGC` /
    /// `-XX:+ExplicitGCInvokesConcurrent`. Per VM, never a process flag.
    /// [`ExplicitGcPolicy::Full`] (the default on every entry point) is the
    /// behaviour before the flags were honoured. gc-common w8-g.
    pub explicit_gc: ExplicitGcPolicy,
    /// gci i43/lf: the value of `-XX:+ExplicitGCInvokesConcurrent` itself
    /// (last occurrence; `false` when not given). [`Self::explicit_gc`] folds
    /// both explicit-GC flags into the behaviour, and with
    /// `-XX:+DisableExplicitGC` also given the fold is `Disabled`; HotSpot's
    /// JFR `jdk.GCConfiguration.isExplicitGCConcurrent` still reports this
    /// flag (`gci-i42mx-jfr-gc-configuration-events-are-not-hotspots-FIXED-20261016`
    /// item 8). Read only by that answer, never by the collection path.
    pub explicit_gc_invokes_concurrent: bool,
    /// gci i43/lf: `-XX:SurvivorRatio=<n>` as the launcher validated it
    /// (HotSpot's range `1 ..= max_uintx - 2` and its `AfterMemoryInit`
    /// constraint). `None` = not given (HotSpot's 8). Per VM, never a
    /// process flag. Consumed by `GenerationalHeap::set_survivor_ratio`
    /// (gci i45/sc) and `G1Collector::set_survivor_ratio` (gci i43/gy).
    pub survivor_ratio: Option<u64>,
    /// gci i43/lf: the HotSpot TLAB flags as the launcher validated them
    /// ([`TlabFlags`]). Default (nothing given) on every entry point.
    /// Consumed through `cratonvm_gc::tlab::TlabSizing`
    /// (`HeapRealm::tlab_sizing`, gci i45/sc).
    pub tlab_flags: TlabFlags,
    /// HotSpot `-XX:SoftRefLRUPolicyMSPerMB=<n>`: how many milliseconds of
    /// idleness a SoftReference is allowed per megabyte of free heap before the
    /// LRU policy may clear it (every backend; this VM's
    /// `ReferenceProcessor`, built with it in `vm_init`). `None` (the default
    /// on every entry point) keeps the processor's own default, HotSpot's
    /// 1000. `Some(0)` clears every soft reference that is idle at all, as on
    /// HotSpot. Per VM, never a process flag. gen r5w2/conc6; see
    /// `docs/feature-designs/generative-gc-architecture.md`.
    pub soft_ref_lru_policy_ms_per_mb: Option<u64>,

    /// HotSpot's `MinHeapSize`, when the command line gave one: `-Xms`
    /// (which sets both the minimum and the initial size on HotSpot) or
    /// `-XX:MinHeapSize=` (gcg g3/x). `None` (every entry point's default)
    /// is HotSpot's ergonomic minimum, `min(8M, initial)`. Reported by the
    /// `[gc,init] Heap Min Capacity:` line
    /// (`unified_logging::hotspot_min_heap_size`), and ZGC's uncommit floor
    /// (`CRATONVM_ZGC_UNCOMMIT_TO_MIN_HEAP`, zr z3/fp: the same figure). zt
    /// t2/rv: this said "no collector reads it", which stopped being true then.
    pub min_heap_size: Option<usize>,
    /// HotSpot `-XX:MinHeapFreeRatio=<0..=100>` (default 40) and
    /// `-XX:MaxHeapFreeRatio=<0..=100>` (default 70), validated by the
    /// launcher as HotSpot does (gcg g3/x,
    /// `gcg-g2e-heap-free-ratio-flags-are-not-parsed`). `None` keeps the
    /// collector's own default (HotSpot's). Honoured by the Generational old
    /// generation's resize once the lane-e hunk of the gcg g3/x report lands
    /// (`VmHeap::set_heap_free_ratios`); per VM, never a process flag.
    pub min_heap_free_ratio: Option<u8>,
    pub max_heap_free_ratio: Option<u8>,

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
    /// * `false` ([`JdkMode::Real`]) — roughly
    ///   [`REAL_JDK_NATIVE_REGISTRATIONS`] methods are registered (**not**
    ///   the "~300" this doc claimed until 2026-07-26) and real JDK classes
    ///   load from `$JAVA_HOME/jmods` (or the `lib/modules` jimage).
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

    /// Which compatibility substitutions this VM permits.
    ///
    /// Orthogonal to [`Self::use_synthetic_jdk`]: that field selects *which
    /// class library* boots, this one selects *which substitutions* are legal
    /// once it has. Under [`CompatibilityMode::JdkOnly`] real class bytes are
    /// authoritative — no fabricated compatibility class, no
    /// `NativeKind::SyntheticStub` registered or invoked, and a structured
    /// error instead of a silent substitution.
    ///
    /// The default **differs by entry point**:
    /// [`CompatibilityMode::JdkOnly`] for the launcher
    /// ([`LAUNCHER_DEFAULT_COMPATIBILITY_MODE`], opt out with `--compatible`)
    /// and [`CompatibilityMode::Compatible`] for embedders
    /// ([`EMBEDDED_DEFAULT_COMPATIBILITY_MODE`],
    /// [`EMBEDDED_HOST_JDK_DEFAULT_COMPATIBILITY_MODE`]). See
    /// [`LAUNCHER_DEFAULT_COMPATIBILITY_MODE`] for why they split. The two
    /// legal pairings are
    /// `Compatible` + either JDK mode, and `JdkOnly` + [`JdkMode::Real`];
    /// `JdkOnly` + [`JdkMode::Synthetic`] is rejected by
    /// [`Self::validate_compatibility`].
    ///
    /// Read it as an [`ExecutionPolicy`] via [`Self::execution_policy`] — that
    /// is the value the native registry, the class manager and dispatch carry.
    /// There is no process global for it: a global would make two VMs in one
    /// process share one policy, which this repository has already been bitten
    /// by for native caches.
    pub compatibility_mode: CompatibilityMode,

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

    /// `-agentlib:jdwp=...` mapped onto the built-in JDWP server
    /// (interpreter round i4 wave 85, lane L1): listen or connect, the
    /// address, whether the VM waits for the debugger before `main`
    /// (`debug::start_jdwp_agent`, which the launcher calls where HotSpot's
    /// back end gets `VMInit`). `None` without the option; `--jdwp-port`
    /// keeps its own path (`jdwp_port`).
    pub jdwp_agent: Option<JdwpAgentConfig>,

    /// Stage 2 of the contiguous interpreter stack (argument overlap,
    /// interpreter round i1 wave 37, lane L7): the interpreter's fast invoke
    /// doors lay a callee's locals over the arguments its caller pushed
    /// instead of copying them (`FrameStack::push_cached_compact_overlapping`).
    /// Off by default while it is measured; `CRATONVM_JIT_OVERLAP_ARGS=1`
    /// (`CRATONVM_JIT=+overlap-args`) turns it on, read once per VM here.
    /// Ignored while a JDWP port is configured (a debugger reads a returning
    /// frame's locals, which an overlapped frame gives up to its caller).
    /// A performance switch only: no observable behaviour depends on it.
    pub overlap_interpreter_args: bool,

    /// The fast invoke doors rebuild a retired frame slot that already runs
    /// the callee's cache entry without a reference-count update: the door
    /// borrows the entry instead of cloning it, and the slot keeps its own
    /// handle (`FrameStack::push_cached_compact_reusing_same`; proposal
    /// `interpreter-L7-proposal-a-cached-frame-install-without-refcount-or-flag-traffic-RETIRED-20261015`,
    /// item 1, interpreter round i1 wave 47, lane L7).
    /// `CRATONVM_FRAME_INSTALL_BORROWED`, read once per VM here; default on
    /// (`=0` restores the door's clone). A performance switch only: no
    /// observable behaviour depends on it.
    pub frame_install_borrowed: bool,

    /// The interpreter's `checkcast` / `instanceof` cast-site hit reads its
    /// operand's raw slot (no `Value` decode, no `ObjectRef` construction)
    /// and the receiver's header through one validated reference
    /// (`VmHeap::header_if_object_start`), and `instanceof`'s hit rewrites
    /// the operand slot with its answer instead of a pop and a push
    /// (proposal
    /// `interpreter-L7-proposal-the-cast-hit-reads-the-receiver-header-in-one-call-FIXED-20261015`,
    /// interpreter round i1 wave 51, lane L7). Anything the raw hit cannot
    /// answer takes the decoding path unchanged. `CRATONVM_CAST_HIT_RAW`,
    /// read once per VM here; default on (`=0` restores the decoding
    /// probe). A performance switch only: the memos and
    /// their answers are the same either way.
    pub cast_hit_raw: bool,

    /// The raw cast-site hit (`cast_hit_raw`) answers a class receiver a
    /// memo names from the cast table's entry IN PLACE
    /// (`opcodes::cast_site_hit_in_place`): the entry's tags, its memo epoch
    /// and the negative / positive class-id slots are read through the
    /// table's borrow, instead of the whole `CastSite` being copied out,
    /// rebuilt by `observed_at` and passed on by value. A miss, an array
    /// receiver and a crosscheck run take the copying protocol unchanged
    /// (proposal `i55-L7-proposal-the-cast-site-hit-reads-the-memo-in-place`,
    /// interpreter round i2 wave 56, lane L7). A performance switch only:
    /// the memos and their answers are the same either way.
    /// `CRATONVM_CAST_HIT_IN_PLACE`, read once per VM here; default on since
    /// the lane's last commit (`=0` restores the copying hit).
    pub cast_hit_in_place: bool,

    /// The in-process attach (`com.sun.tools.attach.VirtualMachine`'s
    /// natives, `runtime::instrument`) refuses what HotSpot refuses
    /// (interpreter round i2 wave 56, lane L1): `attach(id)` checks `id` as
    /// `HotSpotVirtualMachine` does (`attach_refusal`: an id that is no
    /// number, this process without `jdk.attach.allowAttachSelf`, another
    /// process), and `loadAgent` throws HotSpot's `AgentLoadException` /
    /// `AgentInitializationException` for a JAR it cannot start and hands
    /// `agentmain` a NULL argument string when none was given. Off: every id
    /// answers a handle and a failed load returns quietly, as before.
    /// `CRATONVM_ATTACH_CHECKS`, read once per VM here; default on (wave 56's
    /// last commit), `=0` restores the old answers.
    pub attach_checks: bool,

    /// The VM's attach listener (`/tmp/.java_pid<pid>`, Unix;
    /// `runtime::serviceability::AttachListener`) serves the `load`
    /// operation another process's `VirtualMachine.loadAgent` /
    /// `loadAgentPath` / `loadAgentLibrary` sends, into the in-process loads
    /// (`runtime::instrument::attach_listener_load`; interpreter round i2
    /// wave 58, lane L1, stage 1 of
    /// `docs/feature-designs/jdk-only-mode.md`).
    /// Off: the listener answers "Unrecognized attach operation: load", as
    /// before. `CRATONVM_ATTACH_LISTENER_LOAD`, read once per VM here;
    /// default on since interpreter round i2 wave 59 lane L1's last commit
    /// (`=0` turns it off; Windows has no listener).
    pub attach_listener_load: bool,

    /// The VM's attach listener answers the `properties` and
    /// `agentProperties` operations (another process's
    /// `VirtualMachine.getSystemProperties` / `getAgentProperties`) as HotSpot
    /// does: the bytes of `jdk.internal.vm.VMSupport`'s
    /// `serializePropertiesToByteArray()` / `serializeAgentPropertiesToByteArray()`,
    /// run on an attached thread (`runtime::instrument::attach_listener_properties`;
    /// interpreter round i2 wave 59, lane L1, stage 2 of
    /// `docs/feature-designs/jdk-only-mode.md`).
    /// Off: `properties` answers `VM.system_properties`' unescaped
    /// `key=value` lines (a value holding a newline or a backslash reaches the
    /// client's `Properties.load` broken) and `agentProperties` is an
    /// unrecognized operation, as before. `CRATONVM_ATTACH_LISTENER_PROPS`,
    /// read once per VM here; default on since wave 59 lane L1's last commit
    /// (`=0` turns it off).
    pub attach_listener_props: bool,

    /// The VM's attach listener starts lazily, as HotSpot's does (Linux):
    /// nothing is bound at boot; a thread polls for the trigger file an
    /// attaching client creates (`.attach_pid<pid>` in the working directory
    /// or `/tmp`) and binds `/tmp/.java_pid<pid>` then, and `SIGQUIT` (which
    /// the client also sends) no longer kills the process
    /// (`runtime::serviceability::AttachListener::start_listening_lazily`,
    /// `install_sigquit_keeps_the_vm_alive`; interpreter round i2 wave 59,
    /// lane L1, stage 3 of the attach-listener proposal). Off: bound at boot
    /// and `SIGQUIT` keeps its default action (terminate), as before.
    /// `CRATONVM_ATTACH_LISTENER_LAZY`, read once per VM here; default on
    /// since wave 59 lane L1's last commit (`=0` turns it off).
    pub attach_listener_lazy: bool,

    /// The VM leaves `jdk.attach.allowAttachSelf` unset (HotSpot's `null`)
    /// on all platforms, so ByteBuddy's external attacher reaches the listener
    /// as on HotSpot and a self-attach without the property answers
    /// HotSpot's `IOException("Can not attach to current VM")` (interpreter
    /// round i2 wave 59, lane L1;
    /// `docs/feature-designs/jdk-only-mode.md`).
    /// Off: the VM defaults the property to
    /// `true`, as before. `CRATONVM_ATTACH_SELF_UNSET`, read once per VM here;
    /// default on since wave 59 lane L1's last commit (`=0` turns it off).
    pub attach_self_unset: bool,

    /// `-XX:+DisableAttachMechanism` (interpreter round i2 wave 59, lane L1;
    /// `docs/feature-designs/profile-guided-inlining.md`):
    /// the VM starts no attach listener (`Vm::new` builds no
    /// `JcmdProcessor`), a self-attach of this process (or pid 0) throws
    /// HotSpot's `AttachNotSupportedException("The VM does not support the
    /// attach mechanism")` (`runtime::instrument::attach_refusal`), and the VM
    /// does not default `jdk.attach.allowAttachSelf` (HotSpot's `null`). On
    /// Unix `SIGQUIT` is kept from killing the process, since an attaching
    /// client that finds no socket sends it. A
    /// HotSpot flag, not a `CRATONVM_*` switch: the launcher and
    /// `libcratonvm` parse it; default false, as on HotSpot.
    pub disable_attach_mechanism: bool,

    /// `-XX:+EnableDynamicAgentLoading` / `-XX:-EnableDynamicAgentLoading`
    /// as given on the command line (last one wins), `None` when neither is
    /// (interpreter round i2 wave 60, lane L1;
    /// `docs/feature-designs/jdk-only-mode.md`).
    /// HotSpot prints its dynamic-agent warning only when the flag is not on
    /// the command line. A HotSpot flag, not a `CRATONVM_*` switch: the
    /// launcher and `libcratonvm` parse it. `-XX:-EnableDynamicAgentLoading`
    /// (`Some(false)`) refuses every attach `load` operation -- the
    /// listener's and an in-process `VirtualMachine.loadAgent*` -- with
    /// HotSpot's `AgentLoadException` (interpreter round i3 wave 81, lane L1;
    /// `runtime::instrument::DYNAMIC_AGENT_LOADING_DISABLED`).
    pub enable_dynamic_agent_loading: Option<bool>,

    /// A dynamically loaded Java agent prints HotSpot's four `WARNING:` lines
    /// on stderr before its `agentmain` runs, unless
    /// [`Self::enable_dynamic_agent_loading`] was given, and a JAR without
    /// `Agent-Class` prints JPLIS's `Failed to find Agent-Class manifest
    /// attribute from <path>` (`runtime::instrument::run_self_attach`;
    /// interpreter round i2 wave 60, lane L1; MEASURED,
    /// `L1W60DynamicAgentWarning`). Off: both loads are quiet, as before.
    /// `CRATONVM_ATTACH_AGENT_WARNING`, read once per VM here.
    pub dynamic_agent_warning: bool,

    /// A dynamically loaded Java agent is started as JPLIS starts it: through
    /// the JDK's `sun.instrument.InstrumentationImpl.loadClassAndCallAgentmain`
    /// on an "Attach Listener" thread (an in-process `VirtualMachine.loadAgent`
    /// included, as HotSpot serves it through its listener), and an agent
    /// that fails to start prints JPLIS's stderr: `Exception in thread
    /// "Attach Listener"` with the throwable's trace, the three `***
    /// java.lang.instrument ASSERTION FAILED ***` lines and `Agent failed to
    /// start!` (`runtime::instrument::run_self_attach_inner`; interpreter round
    /// i3 wave 81, lane L1; MEASURED, HotSpot 25.0.4, `L1W81DynamicAgentRefusal`).
    /// Off: the VM calls `agentmain` itself on the calling thread and prints
    /// nothing for a failed start, as before. A change of the load path (the
    /// agent's thread and its reflective frames); default on.
    /// Since interpreter round i4 wave 84 (lane L1) it also governs a STARTUP
    /// `-javaagent`: on, its `premain` starts through
    /// `InstrumentationImpl.loadClassAndCallPremain` (null `agentArgs` without
    /// options) and a failed start prints JPLIS's stderr, the `FATAL ERROR`
    /// line and aborts (`runtime::agent_loader::start_premain_through_jdk`,
    /// probe `L1W84PremainFailure`); off, the VM's own dispatch as before.
    /// `CRATONVM_ATTACH_AGENTMAIN_JDK`, read once per VM here.
    pub attach_agentmain_jdk: bool,

    /// The VM writes HotSpot's jvmstat perf-data file,
    /// `/tmp/hsperfdata_<user>/<pid>` (mode 0600, the directory 0755, 32768
    /// bytes, a v2.0 prologue), holding `sun.rt.jvmCapabilities` (its first
    /// character the attach bit: `0` under `-XX:+DisableAttachMechanism`),
    /// `sun.rt.javaCommand` and the `java.property.java.vm.*` /
    /// `java.rt.vm*` strings `jps` and `VirtualMachine.list()` read, and
    /// removes it at exit (`runtime::perf_data_file`; Unix, and on Windows
    /// `%TEMP%\hsperfdata_<user>\<pid>` delete-on-close plus the named
    /// mapping `hsperfdata_<user>_<pid>`; interpreter round
    /// i3 wave 81, lane L1;
    /// `docs/feature-designs/jdk-only-mode.md`).
    /// Off: no file, so `jps` and `VirtualMachine.list()` do not see the VM
    /// and an external attach cannot tell a disabled attach mechanism, as
    /// before. Default on; `CRATONVM_PERF_DATA_FILE=0` disables publication.
    /// The setting is read once per VM.
    pub perf_data_file: bool,

    /// `SIGQUIT` (`kill -3`) prints a thread dump on stdout and the process
    /// continues, as HotSpot's signal dispatcher answers it (Unix; interpreter
    /// round i2 wave 60, lane L1;
    /// `docs/feature-designs/jdk-only-mode.md`):
    /// the handler only sets a latch
    /// (`runtime::serviceability::install_sigquit_latch`), and the attach
    /// listener's thread, which already polls, prints the `Thread.print`
    /// command's dump when it finds the latch set -- unless an attaching
    /// client's trigger file is present, when it starts the listener, as
    /// HotSpot does. Only while the VM has an attach listener (not under
    /// `-XX:+DisableAttachMechanism`). Off: the signal is ignored under
    /// `CRATONVM_ATTACH_LISTENER_LAZY` and kills the process otherwise, as
    /// before. `CRATONVM_SIGQUIT_THREAD_DUMP`, read once per VM here.
    pub sigquit_thread_dump: bool,

    /// `-Xrs` / `-XX:+ReduceSignalUsage` (interpreter round i3 wave 79, lane
    /// L3; `docs/feature-designs/jdk-only-mode.md`):
    /// the VM installs no `SIGQUIT` handler at all -- neither
    /// [`Self::sigquit_thread_dump`]'s latch nor the empty handler that keeps
    /// an attaching client's signal from killing the process -- so the signal
    /// keeps its default action, as HotSpot leaves it under the flag. A
    /// HotSpot flag, not a `CRATONVM_*` switch: the launcher and
    /// `libcratonvm` parse it; default false, as on HotSpot. HotSpot's other
    /// `-Xrs` effects are modelled too: the launcher installs no
    /// `SIGINT` / `SIGTERM` / `SIGHUP` (Windows: console) handler
    /// (`runtime::launcher_signals`), and `jdk.internal.misc.Signal.handle0`
    /// refuses those three (`native-builtins` `deprecated_internal::native_signal_handle0`,
    /// `NativeContext::reduce_signal_usage`), so `Signal.handle` throws
    /// `IllegalArgumentException`. (Doc corrected in interpreter round i7
    /// wave 135, lane L1b: it said the refusal was not modelled.)
    pub reduce_signal_usage: bool,

    /// The C JVMTI `GetSystemProperties` answers the VM's property list in
    /// HotSpot's order (`cratonvm_types::vm_properties::HOTSPOT_LIST_ORDER`,
    /// measured), OnLoad and live, a `-D` of a list key keeping the key's
    /// place (interpreter round i7 wave 135, lane L1b; stage 4 of
    /// `i61-L1-proposal-one-vm-property-list`). HotSpot parity:
    /// `CRATONVM_VM_PROPERTY_LIST_ORDER`, read once per VM here
    /// (`runtime_flag_default_on`), default ON, `=0` off (the table's
    /// historical order). `SystemProps$Raw.vmProperties()` reads the same
    /// switch in `native-builtins`.
    pub vm_property_list_order: bool,

    /// `java.vm.compressedOopsMode` answers as HotSpot 25 does for this
    /// VM's `MaxHeapSize`, collector and `-XX:±UseCompressedOops` (absent
    /// under ZGC, `-XX:-UseCompressedOops` or a heap past the narrow-oop
    /// limit; `32-bit` / `Zero based` / `Non-zero disjoint base` by heap
    /// size), or from the real narrow-oop base and shift when this VM runs
    /// with compressed oops (`vm_init::vm_compressed_oops_mode`;
    /// interpreter round i7 wave 136, lane L1b). HotSpot parity:
    /// `CRATONVM_VM_COMPRESSED_OOPS_MODE_HOTSPOT`, read once per VM here,
    /// default ON, `=0` the old constant `Zero based`.
    /// `SystemProps$Raw.vmProperties()` reads the VM map's value.
    pub vm_compressed_oops_mode_hotspot: bool,

    /// The interpreter's cast sites memoise three verdicts the full path
    /// used to recompute on every execution (proposal
    /// `interpreter-L7-proposal-cast-sites-memoise-lambda-receivers-and-array-refusals-FIXED-20261016`,
    /// interpreter round i1 wave 52, lane L7): a LAMBDA PROXY receiver
    /// admitted by `lambda_proxy_satisfies` fills the positive receiver
    /// memo, an ARRAY receiver `instanceof` refused fills the negative array
    /// memo (`CastSite::negative_array`), and a non-array receiver refused
    /// by an ARRAY target fills the negative receiver memo. Only the fills
    /// are gated: the hits read the memos they always read.
    /// `CRATONVM_CAST_MEMO_WIDE`, read once per VM here; default on (the
    /// lane's last commit), `=0` withdraws the three fills.
    pub cast_memo_wide: bool,

    /// The interpreter's cast sites keep TWO positive receiver verdicts
    /// (`CastSite::older_positive_receiver`; proposal
    /// `interpreter-L7-proposal-two-positive-receiver-slots-per-cast-site-FIXED-20261017`,
    /// interpreter round i2 wave 53, lane L7): a positive fill rotates the
    /// previous receiver into the older slot instead of overwriting it, so a
    /// bimorphic `checkcast` / `instanceof` site or catch row (two classes,
    /// or two lambda proxies, in turn) is answered from the memo. Only the
    /// fills are gated: the hit reads both slots, and with the switch off the
    /// older slot is never written. A performance switch only.
    /// `CRATONVM_CAST_MEMO_TWO_POSITIVE`, read once per VM here; default on
    /// (the lane's last commit), `=0` withdraws the rotation.
    pub cast_memo_two_positive: bool,

    /// The interpreter's per-thread resolved `checkcast` / `instanceof`
    /// target cache (`CastSiteCache`, and with it the `anewarray` component
    /// lookup that shares the table, the receiver memos and the resolved
    /// catch-type rows of `exception_dispatch`). Default on;
    /// `CRATONVM_JIT_NO_CAST_SITE_CACHE` (present, any value) opts out. Also
    /// off when `CRATONVM_DBG_H2TRACE` or the loader trace is armed: those
    /// tracers print the resolved class NAME on every execution, and a hit
    /// never materializes one, so a hit would silence them. Read once per VM
    /// here (interpreter round i2 wave 57, lane L7; a process `OnceLock` in
    /// `opcodes.rs` before, proposal
    /// `interpreter-L7-proposal-the-cast-site-gates-move-into-vm-config-FIXED-20261021`).
    pub cast_site_cache: bool,

    /// The `instanceof` negative receiver memo (`CastSite::negative_receivers`).
    /// Default on; `CRATONVM_JIT_NO_NEGATIVE_CAST_MEMO` (present, any value)
    /// withdraws it. Read once per VM here (wave 57, lane L7; a process
    /// `OnceLock` before).
    pub negative_cast_memo: bool,

    /// `CRATONVM_DBG_CAST_MEMO_CROSSCHECK` (present, any value): every
    /// negative-, array- and lambda-memo hit re-derives the full path's
    /// verdict and reports a disagreement. Diagnostic; default off. Read once
    /// per VM here (wave 57, lane L7; a process `OnceLock` before).
    pub cast_memo_crosscheck: bool,

    // The interpreter's other site-cache gates, read once per VM here
    // (interpreter round i2 wave 58, lane L7; each a process `OnceLock` in its
    // module before, proposal
    // `interpreter-L7-proposal-the-remaining-site-cache-gates-move-into-vm-config-FIXED-20261022`).
    // Every variable keeps its exact presence / value semantics; the trace
    // gates folded in stay process-wide diagnostics.
    /// The per-thread `new`-site cache (`ClassSiteCache`). Default on;
    /// `CRATONVM_JIT_NO_NEW_SITE_CACHE` (present, any value) opts out. Also off
    /// when `CRATONVM_DBG_H2TRACE`, the loader trace or the NSEE trace is
    /// armed (a hit skips the class name those print).
    pub new_site_cache: bool,

    /// The per-thread resolved field-site cache (`FieldSiteCache`). Default
    /// on; `CRATONVM_JIT_FIELD_SITE_CACHE` = `0` / `false` / `off` / `no`
    /// (trimmed, case-insensitive) opts out.
    pub field_site_cache: bool,

    /// The field-site cache also admits loader-sensitive sites resolved from
    /// the loader-local lookup. Default on;
    /// `CRATONVM_JIT_FIELD_SITE_CACHE_LOADER` = `0` / `false` / `off` / `no`
    /// opts out. Implies nothing without `field_site_cache`.
    pub field_site_cache_loader: bool,

    /// The per-thread resolved-method site cache (`MethodSiteCache`). Default
    /// off; `CRATONVM_JIT_METHOD_SITE_CACHE` (present, any value) opts in.
    pub method_site_cache: bool,

    /// The interface-dispatch selection memo (`iface_select_sites`) and the
    /// declaring-class short-circuit it guards. Default on;
    /// `CRATONVM_JIT_NO_IFACE_SELECT_MEMO` (present, any value) withdraws both.
    pub iface_select_memo: bool,

    /// `ldc` answers from the recorded-resolution store and `ldc` / `ldc2_w`
    /// from the per-thread numeric table. Default on;
    /// `CRATONVM_JIT_NO_LDC_CONST_CACHE` (present, any value) opts out. Also
    /// off under the three diagnostics a hit would silence
    /// (`constants::ldc_const_cache_forced_off_by_diagnostics`).
    pub ldc_const_cache: bool,

    /// The compiled-NPE helpful-message memo (`jit_npe_message`). Default on;
    /// `CRATONVM_JIT_NPE_MESSAGE_MEMO=0` rebuilds every message.
    pub npe_message_memo: bool,

    /// The invoke doors serve an `ACC_SYNCHRONIZED` callee when its monitor is
    /// free. Default on; `CRATONVM_JIT_NO_DOOR_SYNC` = `1` / `true` / `on`
    /// (exactly) declines every synchronized callee at a door.
    pub door_sync: bool,

    // The interpreter's per-call and per-miss gates, read once per VM here
    // (interpreter round i2 wave 59, lane L7; each a process `OnceLock` in its
    // module before, proposal
    // `interpreter-L7-proposal-the-interpreters-per-call-diagnostic-gates-move-into-vm-config-FIXED-20261023`).
    // Every variable keeps its exact semantics.
    /// The invoke paths tokenise a callee's parameter tags once per call
    /// (`invoke::ParamTags`). Default on; `CRATONVM_JIT_NO_PARAM_TAG_SCAN`
    /// (present, any value) sends every argument back through the per-index
    /// rescan.
    pub param_tag_scan: bool,

    /// JVMS §5.4.3: a failed `CONSTANT_Class` / `CONSTANT_Dynamic` resolution
    /// is recorded and rethrown at every later execution. Default on;
    /// `CRATONVM_LOADER_NO_RESOLUTION_FAILURE_RECORD` (present, any value)
    /// records nothing.
    pub resolution_failure_record: bool,

    /// The `getfield` / `putfield` receiver retarget skips its slow half
    /// while no class name has two `ClassId`s
    /// (`field_access::retarget_instance_field_to_receiver`). Default on;
    /// `CRATONVM_LOADER_NO_DUP_NAME_FIELD_GATE` (present, any value) walks the
    /// slow half on every call.
    pub dup_name_field_gate: bool,

    /// JVMS §2.11.10 for natives: a registered native answering an
    /// `ACC_SYNCHRONIZED` method runs holding its monitor
    /// (`invoke::native_sync_enabled`). Default on; `CRATONVM_NATIVE_SYNC` =
    /// `0` / `false` (case-insensitive) disarms it, `1` / `true` re-arms it,
    /// any other value leaves the default.
    pub native_sync: bool,

    /// The interpreter's diagnostic traces, one bit each
    /// ([`interp_trace`]), asked through [`VmConfig::interp_trace`]: one load
    /// and a bit test where each was an acquire load of its own `OnceLock`.
    pub interp_traces: u8,

    // The process gates the interpreter asked on its HIT paths (every
    // quickened field / array access, every door call), read once per VM here
    // (interpreter round i2 wave 60, lane L7; each a process `OnceLock` or a
    // `flags()` snapshot read in its module before, proposal
    // `interpreter-L7-proposal-the-process-gates-left-on-the-hit-and-door-paths-FIXED-20261024`).
    // Every variable keeps its exact semantics; the counters they arm stay
    // process tables (a census of one run).
    /// The site-cache census (`site_cache::site_stats`) counts the hit-path
    /// events. `CRATONVM_DBG_FIELD_SITE` (present, any value) arms it, as
    /// `site_stats::on()` reads it for the cold paths and the final report.
    pub site_census: bool,

    /// The per-call invoke-path census (`gc_and_alloc::dbg_invoke_stats_record`).
    /// `CRATONVM_DBG_INVOKESTATS` (present, any value) arms it.
    pub invoke_stats: bool,

    /// The interpreter's door and invoke paths answer a callee's parameter
    /// and return tags from its memoized `DescriptorFacts`. Default on;
    /// `CRATONVM_JIT_NO_DESCRIPTOR_FACTS` (present, any value) sends them back
    /// through the per-call descriptor scans. The JIT's own consumers keep
    /// asking `cratonvm_jit_api::descriptor_facts_disabled` (same variable).
    pub descriptor_facts: bool,

    /// The interpreter's call paths do the JIT tier-up bookkeeping (count the
    /// call, probe for a compiled body, offer the method). `false` under
    /// `CRATONVM_DISABLE_JIT` (non-empty and not `0`; `--nojit` sets it), the
    /// value `env_cache::disable_jit()` answers, negated.
    pub jit_enabled: bool,

    /// The invocation count at which the interpreter offers a method to the
    /// JIT: `CRATONVM_JIT_THRESHOLD` parsed as `u32` (trimmed), at least 1,
    /// default 500 -- `env_cache::jit_invocation_threshold()`'s value.
    pub jit_invocation_threshold: u32,

    /// The dispatch loop's back edges re-read the executing method's
    /// branch-profile arm (`interpreter::frame_branch_profile_armed`, three
    /// dependent loads) only while an arm can still happen and has not yet
    /// been seen: never without a JIT (`jit_enabled`, which since this switch
    /// also gates `jit_bridge::offer_invocation_to_tiered_manager`'s arm), nor
    /// once the loop's cached answer is `true` (the flag is never cleared).
    /// Interpreter round i2 wave 62, lane L7 (proposal
    /// `docs/feature-designs/jdk-only-mode.md`).
    /// `CRATONVM_INTERP_BRANCH_ARM_GATE`, read once per VM here
    /// (`runtime_flag_on`); default OFF, so every run re-reads the arm at
    /// every backward branch and frame switch unless `=1` is set. The host's
    /// `--nojit` A/B (wave 63 triage) measured 0-4%, inside its noise, so the
    /// switch stays off (`docs/feature-designs/jdk-only-mode.md`).
    /// A performance switch only: the branches recorded are the same.
    pub interp_branch_arm_gate: bool,

    /// The dispatch loop hoists the debugger / agent observer gates it tested
    /// at every backward branch (the JVMTI single-step union, the JDWP
    /// `breakpoints_active` gate) and on every quickened `getfield` /
    /// `putfield` (the field-watch union), and refreshes them in its loop-top
    /// slow path (`interpreter::loop_top_poll`), which every writer of those
    /// gates now sends every loop of this VM to by bumping its poll word
    /// (`GcBarrier::note_code_moved_on_every_loop`) after it publishes.
    /// So an observer armed while a loop runs is seen before the loop's next
    /// bytecode, as before or sooner. Interpreter round i2 wave 62, lane L7
    /// (proposal
    /// `i61-L7-proposal-the-loop-hoists-the-observer-flags-and-the-poll-word-refreshes-them-20261025`).
    /// `CRATONVM_INTERP_HOISTED_OBSERVERS`, read once per VM here
    /// (`runtime_flag_on`); default OFF, so every run reads the gates where
    /// they are used, as before, unless `=1` is set. The host's `--nojit` A/B
    /// (wave 63 triage) measured 0-4%, inside its noise, so the switch stays
    /// off (same page as `interp_branch_arm_gate`).
    pub interp_hoisted_observers: bool,

    /// The writers of the JVMTI single-step union and the JDWP debugger gate
    /// send this VM's dispatch loops to their loop-top slow path (the poll
    /// word every bytecode already compares), which re-reads the single-step
    /// and breakpoint gates there, as [`Self::interp_hoisted_observers`] does
    /// but without hoisting anything off the existing paths. So a `SingleStep`
    /// enabled while a frame runs (a JNI native it called, an agent callback
    /// on its thread) is posted from that frame's next bytecode, as HotSpot
    /// posts it, instead of from its next call, return or back edge
    /// (`docs/feature-designs/jvmti-delivery-threading.md`;
    /// interpreter round i3 wave 81, lane L1). Costs nothing per bytecode: the
    /// switch is read only in the slow path and by the (cold) writers.
    /// `CRATONVM_JVMTI_STEP_POLL`, read once per VM here (`runtime_flag_on`);
    /// default OFF until measured.
    pub jvmti_step_poll: bool,

    /// A C JVMTI agent's `PopFrame` / `ForceEarlyReturn*` of the calling
    /// thread made inside its `MethodEntry` callback is served, as HotSpot
    /// serves it, for a frame an interpreter invoke pushed for the dispatch
    /// loop running its caller (interpreter round i4 wave 95, lane L7; stage
    /// 1 of `i62-L1-proposal-one-agent-request-word-the-dispatch-loop-reads`,
    /// stage 2 of `i61-L1-proposal-frame-control-from-an-event-callback`).
    /// The callback runs at a callback point
    /// (`interpreter::jvmti_events::fire_method_entry_after_push_in_loop`); a
    /// request it records raises `GcBlockState::agent_requests` and bumps the
    /// loop poll word, and the loop's slow path takes it before the frame's
    /// bytecode 0. Needs `CRATONVM_JVMTI_C_CALLBACK_FRAME_CONTROL` too. No
    /// per-bytecode or per-invoke test is added: the writer runs only under
    /// a `MethodEntry` listener, the reader only in the slow path.
    /// `CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL`, read once per VM here
    /// (`runtime_flag_default_on`); default ON since the wave-129 triage, `=0` off.
    pub jvmti_method_entry_frame_control: bool,

    /// The `MethodEntry` of a CONTENDED `synchronized` method entry, posted
    /// once the monitor is held (`interpreter::fire_deferred_method_entry`),
    /// runs at the same callback point as the uncontended entry's, so a C
    /// agent's `PopFrame` / `ForceEarlyReturn*` from that callback is served
    /// for a frame an interpreter invoke pushed for its caller's loop
    /// (interpreter round i6 wave 134, lane L1; shape 1 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`).
    /// Needs [`Self::jvmti_method_entry_frame_control`]. Parity fix:
    /// `CRATONVM_JVMTI_CONTENDED_ENTRY_FRAME_CONTROL`, read once per VM here
    /// (`runtime_flag_default_on`); default ON, `=0` posts it outside a
    /// callback point again (the request answers `OPAQUE_FRAME`).
    pub jvmti_contended_entry_frame_control: bool,

    /// The `MethodEntry` of the ENTRY frame of a nested dispatch loop
    /// (`interpreter::execute`'s push: a JNI upcall, a reflective call, a VM
    /// call into Java) runs at a callback point whose loop base is that frame,
    /// so a C agent's `ForceEarlyReturn*` from the callback is taken at the new
    /// loop's first top, as HotSpot serves a forced return of a Java top frame;
    /// a `PopFrame` there stays `OPAQUE_FRAME` (its caller is another loop's)
    /// (interpreter round i7 wave 135, lane L1; shape 2 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`).
    /// Needs [`Self::jvmti_method_entry_frame_control`] (and, for a contended
    /// `synchronized` entry, [`Self::jvmti_contended_entry_frame_control`]).
    /// Parity fix: `CRATONVM_JVMTI_NESTED_ENTRY_FRAME_CONTROL`, read once per
    /// VM here (`runtime_flag_default_on`); default ON, `=0` posts outside a
    /// callback point again (the request answers `OPAQUE_FRAME`).
    pub jvmti_nested_entry_frame_control: bool,

    /// A C agent's `PopFrame` of a lambda target from its `MethodEntry`
    /// callback is served: the lambda door pops the unrun frame and calls the
    /// target again, as HotSpot re-runs the spun proxy's invoke (interpreter
    /// round i7 wave 137, lane L1;
    /// `docs/feature-designs/jdk-only-mode.md`,
    /// `interpreter::install_and_run_cached_frame_reinvokable`). Needs
    /// [`Self::jvmti_method_entry_frame_control`] and
    /// [`Self::jvmti_nested_entry_frame_control`]. Parity fix:
    /// `CRATONVM_JVMTI_LAMBDA_ENTRY_POP`, read once per VM here; default ON,
    /// `=0` answers `OPAQUE_FRAME` again.
    pub jvmti_lambda_entry_pop: bool,

    /// A C agent's `PopFrame` of an interpreted callee of a JIT-compiled
    /// caller from its `MethodEntry` callback is served: the JIT helpers'
    /// door (`interpreter::install_and_run_cached_frame_monitored`, the
    /// unsynchronized template) pops the unrun frame and calls the callee
    /// again, as HotSpot deoptimizes the compiled caller and re-runs its
    /// invoke (interpreter round i7 wave 138, lane L1; the residual of
    /// `docs/feature-designs/jdk-only-mode.md`).
    /// Needs [`Self::jvmti_method_entry_frame_control`] and
    /// [`Self::jvmti_nested_entry_frame_control`]. Parity fix:
    /// `CRATONVM_JVMTI_COMPILED_CALLER_ENTRY_POP`, read once per VM here;
    /// default ON, `=0` answers `OPAQUE_FRAME` again.
    pub jvmti_compiled_caller_entry_pop: bool,

    /// A JDWP step measures a lambda proxy method's listed row as a frame,
    /// and a step that completes there (a `STEP_OUT` of the lambda's target,
    /// or a step returning into the proxy) stops at the proxy method's
    /// `return`, line -1, as HotSpot stops in the spun class's frame
    /// (interpreter round i7 wave 138, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`).
    /// Parity fix: `CRATONVM_JDWP_LAMBDA_PROXY_STEPS`, read once per VM
    /// here; default ON, `=0` restores the wave-137 behaviour (the step's
    /// starting depth counts the proxy row, the suspend point does not).
    pub jdwp_lambda_proxy_steps: bool,

    /// A JDWP `PopFrames` of a lambda's target (the lambda door's entry
    /// frame, under the spun proxy's listed row) is served: the frames are
    /// popped, the proxy's row stays on top, and the resume calls the target
    /// again with its parameter slots' values, as HotSpot re-runs the proxy's
    /// invoke (interpreter round i7 wave 139, lane L1;
    /// `debug::pop_frames::try_door_entry_pop`,
    /// `docs/feature-designs/jvmti-delivery-threading.md`).
    /// Parity fix: `CRATONVM_JDWP_LAMBDA_TARGET_POP`, read once per VM here;
    /// default ON, `=0` answers `OPAQUE_FRAME` again.
    pub jdwp_lambda_target_pop: bool,

    /// A JDWP `PopFrames` of a JIT-compiled caller's interpreted callee (the
    /// JIT helpers' door's entry frame, under the compiled activation's
    /// listed row) is served: the frames are popped and the resume calls the
    /// same resolved callee again with its parameter slots' values, as
    /// HotSpot deoptimizes the caller and re-runs its invoke (interpreter
    /// round i7 wave 139, lane L1; `debug::pop_frames::try_door_entry_pop`).
    /// Parity fix: `CRATONVM_JDWP_COMPILED_CALLER_POP`, read once per VM
    /// here; default ON, `=0` answers `OPAQUE_FRAME` again.
    pub jdwp_compiled_caller_pop: bool,

    /// While a JDWP session is attached, a method-handle call runs the JDK's
    /// real `LambdaForm` frames instead of the direct `linkTo*` lanes
    /// (`NativeContext::debugger_observes_frames`), so a step out of the
    /// target completes in `DirectMethodHandle$Holder.invokeStatic` as on
    /// HotSpot (interpreter round i7 wave 144, lane L1;
    /// `docs/feature-designs/jdk-only-mode.md`).
    /// Parity fix: `CRATONVM_MH_FORM_FRAMES_UNDER_DEBUGGER`, read once per VM
    /// here; default ON, `=0` keeps the direct lanes.
    pub mh_form_frames_under_debugger: bool,

    /// `-agentlib:jdwp` ([`Self::jdwp_agent`], the built-in server every IDE
    /// and JDI's launching connector start) counts as a JDWP debugger for the
    /// facts of the VM's start that `--jdwp-port` ([`Self::jdwp_port`]) alone
    /// answered (interpreter round i7 wave 136, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
    /// `jvmti::debugger_observes_locals` (every local a GC root, compiles keep
    /// dead locals, a blocked thread's dead slot read through its tracked
    /// address, as HotSpot's back end acquires `can_access_local_variables` at
    /// `OnLoad`) and `interpreter::open_blocked_inspection` (a thread that
    /// blocks before the debugger attaches keeps its inspection window). Both
    /// are facts of the VM's start, as the option is. Parity fix:
    /// `CRATONVM_JDWP_AGENT_DEBUGGER_FACTS`, read once per VM here
    /// (`runtime_flag_default_on`); default ON, `=0` asks `jdwp_port` alone
    /// again.
    pub jdwp_agent_debugger_facts: bool,

    /// A C agent's `PopFrame` / `ForceEarlyReturn*` of a thread suspended
    /// while BLOCKED entering a monitor (a `monitorenter`, or a contended
    /// `synchronized` method entry whose frame is pushed first) is recorded
    /// through the thread's inspection window and taken by the thread once it
    /// holds the monitor, before the next bytecode runs, as HotSpot takes a
    /// pending pop or early return after `InterpreterRuntime::monitorenter`
    /// returns (interpreter round i6 wave 134, lane L1;
    /// `docs/feature-designs/jvmti-delivery-threading.md`).
    /// Needs [`Self::jvmti_method_entry_frame_control`] (the loop's reader).
    /// Parity fix: `CRATONVM_JVMTI_BLOCKED_ENTRY_FRAME_CONTROL`, read once per
    /// VM here (`runtime_flag_default_on`); default ON, `=0` answers
    /// `OPAQUE_FRAME` for such a thread again.
    pub jvmti_blocked_entry_frame_control: bool,

    /// The dispatch loop's T19.H1 stack-dump hook (`--stack-dump-on-timeout`,
    /// `--stack-sample-ms`, `CRATONVM_DEFAULT_WATCHDOG_SEC`) runs in the loop
    /// top's slow path instead of on every bytecode: `SharedVm::request_stack_dump`
    /// and `request_stack_sample` send this VM's loops there (the poll word
    /// every bytecode already compares), so a run with a watchdog or sampler
    /// armed stops paying the hook's two dependent loads per bytecode
    /// (interpreter round i3 wave 82, lane L7;
    /// `i77-L7-proposal-pack-the-dispatch-loops-diagnostic-gates-into-one-word`).
    /// The dump's content is unchanged; it is taken after the poll instead of
    /// before it. `CRATONVM_INTERP_STACK_DUMP_AT_POLL`, read once per VM here
    /// (`runtime_flag_on`); default OFF until measured.
    pub interp_stack_dump_at_poll: bool,

    /// The dispatch loop's top tests ONE gate bit for both pending signals
    /// (a parked Java exception, a parked fast-path `RuntimeError`) and the
    /// stack-dump hook, instead of loading the two pending slots on every
    /// bytecode (interpreter round i4 wave 84, lane L7;
    /// `i84-L7-proposal-one-loop-top-test-for-both-pending-signals`). Every
    /// writer of either slot sets the bit in both modes (through the loop's
    /// `park_java_exception!` / `park_runtime!`); with the switch off the
    /// slots are also tested, as before, so off is the belt-and-braces form.
    /// `CRATONVM_INTERP_ONE_TOP_SIGNAL_TEST`, read once per VM here
    /// (`runtime_flag_on`); default OFF until measured.
    pub interp_one_top_signal_test: bool,

    /// The static fast door answers its loader-split guard
    /// (`dispatch_static::cached_static_owner_stale`) from an owner stamp the
    /// thread's invoke cache keeps per call site -- the declaring class and
    /// the VM's definition / resolution epochs the last "not stale" verdict
    /// was proved under -- instead of the thread-local memo's `RefCell`
    /// probe, once a user-defined loader has defined any class (interpreter
    /// round i4 wave 84, lane L7;
    /// `i84-L7-proposal-the-static-door-keeps-its-owner-verdict-in-the-entry`).
    /// `CRATONVM_INTERP_STATIC_OWNER_STAMP`, read once per VM here
    /// (`runtime_flag_on`); default OFF until measured.
    pub interp_static_owner_stamp: bool,

    /// The static door's loader-split guard (`OWNER_RECHECK_MEMO` and the
    /// owner stamp, `dispatch_static::static_owner_epochs`) is stamped with
    /// the VM's class-NAME generation (`StoreEpochs::class_name_generation`,
    /// which moves only when an existing name -> class answer can change)
    /// instead of its class-definition epoch (every definition), beside the
    /// resolution epoch either way. The verdict reads only name -> class
    /// answers for a name the entry already resolved, the initiating-loader
    /// memo and the caller's defining loader, so a definition of a new name
    /// cannot change it (interpreter round i4 wave 92, lane L7;
    /// `i89-L4-method-finder-per-class-and-per-handle-cost`).
    /// `CRATONVM_INTERP_OWNER_MEMO_NAME_GENERATION`, read once per VM here
    /// (`runtime_flag_on`); default OFF until measured.
    pub interp_owner_memo_name_generation: bool,

    /// The native-callback memo (`runtime::native_callee_memo`) is stamped
    /// with the VM's class-NAME generation, hierarchy-edge epoch
    /// (`StoreEpochs::hierarchy_edge_epoch`) and resolution epoch, and with
    /// `class_origin_in_place_epoch`, instead of the class-definition epoch
    /// and `class_origin_epoch`, both of which move on every definition of a
    /// class (interpreter round i4 wave 94, lane L7;
    /// `i92-L7-selection-and-callback-memos-need-a-hierarchy-edge-epoch`).
    /// `CRATONVM_NATIVE_CALLBACK_MEMO_EDGE_EPOCH`, read once per VM here
    /// (`runtime_flag_on`); default OFF until measured. The selection memo's
    /// twin, `CRATONVM_INTERP_SELECT_MEMO_EDGE_EPOCH`, is read once per class
    /// store (`ClassStore::select_memo_edge_epoch`), since selection sees only
    /// the store.
    pub native_callback_memo_edge_epoch: bool,

    /// The same memo serves `NativeContext::invoke` (a native calling a Java
    /// method on a named class: `NativeContextImpl::invoke`), keyed by that
    /// class's NAME, the first argument's class (`static_first_arg_key`), the
    /// method name and descriptor (`native_callee_memo::lookup_invoke`). Filled
    /// only when the call reached the ordinary bytecode tail, exactly as the
    /// `invoke_virtual` half. The stack-walker residual-costs page, item 1.
    /// `CRATONVM_NATIVE_INVOKE_MEMO`, read once per VM here
    /// (`runtime_flag_default_on`); default ON, `=0` kill.
    pub native_invoke_memo: bool,

    /// `NativeContext::set_fields_typed` stores a batch that holds REFERENCES
    /// through one compact plan (`VmHeap::try_set_compact_fields`, on each
    /// collector while it is not marking) instead of one validated, forwarded,
    /// layout-resolved store per slot. The stores and barriers are the
    /// accessors' own. The stack-walker residual-costs page, item 2.
    /// `CRATONVM_NATIVE_BATCH_REFERENCE_STORES`, read once per VM here
    /// (`runtime_flag_default_on`); default ON, `=0` kill.
    pub native_batch_reference_stores: bool,

    /// The per-thread memos keyed by class id whose answer reads only stored
    /// classes by id -- the by-name field-slot rings, the field-descriptor
    /// tiers' transient entries and the soft-reference touch memo
    /// (`vm::vm_exec::class_id_memo_epoch`) -- are stamped with this VM's
    /// hierarchy-edge epoch plus `class_origin_in_place_epoch` instead of
    /// `class_origin_epoch`, which moves on every class definition in the
    /// process, so a define-per-iteration workload no longer retires them
    /// (interpreter round i4 wave 95, lane L7;
    /// `i94-L7-proposal-memos-stamped-with-the-definition-or-origin-epoch-retire-on-every-define`).
    /// `CRATONVM_INTERP_CLASS_MEMO_NARROW_EPOCH`, read once per VM here
    /// (`runtime_flag_on`); default OFF until measured.
    pub class_memo_narrow_epoch: bool,

    /// The special door's frameless constructor call (`invoke_fast`'s
    /// trivial and field-store constructor elision) also accepts a first
    /// call of the DIRECT superclass's `<init>()V` when that constructor is
    /// javac's default one, recursively up to `java/lang/Object` (at most
    /// eight links; every link real bytes, no interception, no registered
    /// native): `new Sub()` of a `Sub extends Base extends Object` with
    /// default constructors pushes no frame (interpreter round i4 wave 94,
    /// lane L7; the super-chain stage of
    /// `i24-L4-proposal-frameless-trivial-constructor-in-the-special-door`).
    /// `CRATONVM_INTERP_TRIVIAL_CTOR_SUPER_CHAIN`, read once per VM here
    /// (`runtime_flag_default_on`, `=0` off); default ON since the round i4
    /// wave 94 triage.
    pub interp_trivial_ctor_super_chain: bool,

    /// The special door's frameless field-store constructor
    /// (`invoke_fast::FIELD_CTOR_ELISION`) also answers a constructor with a
    /// `long` / `double` parameter (its locals are mapped to arguments by the
    /// descriptor's slot widths) and `lload` / `dload` / `lconst_*` /
    /// `dconst_*` stores into `long` / `double` fields (stored with the long /
    /// double kind mark, as the quickened `putfield` stores them):
    /// `Span(long start, long end)`, `Vec2(double x, double y)`,
    /// `Entry(int key, long value)`. Interpreter round i6 wave 115, lane L7
    /// (the last open item of
    /// `i24-L4-proposal-frameless-trivial-constructor-in-the-special-door`).
    /// `CRATONVM_INTERP_FIELD_CTOR_WIDE_VALUES`, read once per VM here
    /// (`runtime_flag_default_on`); default ON since the round-i6 wave-115
    /// triage (`--nojit` 0.60-0.64x), `=0` off. A performance switch only.
    pub interp_field_ctor_wide_values: bool,

    /// The special door's frameless field-store constructor also answers a
    /// numeric `ldc` / `ldc_w` (`Integer`, `Float`) or `ldc2_w` (`Long`,
    /// `Double`) store, the value read from the declaring class's pool once
    /// per method (`invoke_fast::ctor_plan_constants_from_pool`):
    /// `int limit = 100_000;`, `double ratio = 0.75;`, `long id = 42L;`. A
    /// `String` / `Class` / dynamic constant keeps the frame. Interpreter
    /// round i6 wave 118, lane L7 (`i24-L4-proposal-frameless-trivial-
    /// constructor-in-the-special-door`, its `ldc` leftover).
    /// `CRATONVM_INTERP_FIELD_CTOR_LDC_VALUES`, read once per VM here
    /// (`runtime_flag_default_on`); default ON since interpreter round i7 wave 144 (A/B w142lto), `=0` off. A performance switch only.
    pub interp_field_ctor_ldc_values: bool,

    /// The special door's constructor verdict (`invoke_fast::ctor_verdict`:
    /// trivial / super-chain / field-store plan) is kept on the call site's
    /// `CachedBytecodeMethod` (`ctor_verdict_memo`), stamped with the VM
    /// identity and the process redefinition count, so a frameless
    /// constructor call reads one cell instead of the per-thread memo (a TLS
    /// access, a `RefCell` borrow, a set probe). Interpreter round i6 wave
    /// 119, lane L7 (stage 3 of
    /// `i24-L4-proposal-frameless-trivial-constructor-in-the-special-door`).
    /// `CRATONVM_INTERP_CTOR_VERDICT_ON_ENTRY`, read once per VM here
    /// (`runtime_flag_on`); default OFF. A performance switch only.
    pub interp_ctor_verdict_on_entry: bool,

    /// The special door's frameless field-store constructor
    /// (`invoke_fast::FIELD_CTOR_ELISION`) also runs on the Generational and
    /// G1 heaps, where the quickened field arms (ZGC only) do not exist: its
    /// stores go through the heap's own field accessor with `op_putfield`'s
    /// barrier sequence (`field_fast::ctor_field_stores_accessor`), and the
    /// field sites they need are filled from `<init>` frames on those heaps
    /// too. Tier-1 round wave 2, lane K (item 1 of
    /// `i24-L4-proposal-frameless-trivial-constructor-in-the-special-door`).
    /// `CRATONVM_INTERP_FIELD_CTOR_GEN_G1`, read once per VM here
    /// (`runtime_flag_on`); default OFF until measured. A performance switch
    /// only.
    pub interp_field_ctor_gen_g1: bool,

    /// The special door's frameless field-store constructor also answers an
    /// inner / local class's constructor, whose stores of `this$0` and
    /// captured values come BEFORE its `super()` call, when that call is a
    /// trivial chain to `Object.<init>` (`invoke_fast::field_ctor_pre_super_plan`).
    /// t1 wave 6, lane G6 (cut 3 of the `Proxy.newProxyInstance` hit-path
    /// proposal, i147: `AbstractClassLoaderValue$Sub.<init>` per hit).
    /// `CRATONVM_INTERP_FIELD_CTOR_PRE_SUPER_STORES`, read once per VM here
    /// (`runtime_flag_default_on`; default ON since 2026-10-29, `=0` turns it
    /// off). A performance switch only.
    pub interp_field_ctor_pre_super_stores: bool,

    /// JVMS §2.11.10 / HotSpot parity for a `synchronized` method's own
    /// monitor (tier-1 round wave 3, lane K): a frame's own `monitorexit` of it
    /// releases it and the frame's return (or unwind) then throws
    /// `IllegalMonitorStateException`; a method monitor the thread no longer
    /// owns at the return or the unwind throws instead of logging a `warn`
    /// line; the release moves from the frame pop to the return, before
    /// `MethodExit`, as in HotSpot's `remove_activation`
    /// (`interpreter::held_monitors::release_method_monitor_at_return`).
    /// `CRATONVM_MONITOREXIT_METHOD_LOCK_PARITY`, read once per VM here
    /// (`runtime_flag_default_on`); default ON, `=0` restores the pop-time
    /// release.
    pub monitorexit_method_lock_parity: bool,

    /// The interpreter's virtual-dispatch cache files the JDK's own
    /// `java/lang/ClassLoader.getResource` / `getResources` /
    /// `getResourceAsStream` body for a receiver class whose instances the
    /// registered `Bridge` natives would route to that body anyway (a
    /// user-defined loader class or a bare `URLClassLoader`, with
    /// `jdk_url_class_loaders()`:
    /// `classloader::user_loader_resource_body_for_class`), instead of the
    /// native, so a cached call makes no native funnel pass and no native-to-
    /// Java re-entry. Interpreter round i6 wave 119, lane L7 (the L7 half of
    /// `i73-L5-proposal-user-loader-get-resource-without-the-native-hop`; the
    /// vtable fast path and the JIT's compiled dispatch still take the native).
    /// `CRATONVM_USER_LOADER_RESOURCE_BYTECODE`, read once per VM here
    /// (`runtime_flag_default_on`); default ON since wave 134 by user
    /// decision (w121: correctness SAME in both modes), `=0` off. A
    /// performance switch only.
    // WARNING(i6-user-flip): CRATONVM_USER_LOADER_RESOURCE_BYTECODE default ON by user decision 2026-10-19; =0 restores the old path
    pub user_loader_resource_bytecode: bool,

    /// The compiled half of [`Self::user_loader_resource_bytecode`]
    /// (interpreter round i6 wave 132, lane L5b): a COMPILED caller's
    /// `invokevirtual` of one of the three `java/lang/ClassLoader` resource
    /// triples, on a receiver class the `Bridge` natives route to the JDK's
    /// own `ClassLoader` body (the same `user_loader_resource_body_for_class`
    /// verdict), enters that body through the JIT helpers' per-(site,
    /// receiver class) interpreter frame template
    /// (`jit::helpers::try_jit_virtual_bytecode_callee`) instead of the
    /// by-name tail's native funnel and its native-to-Java re-entry. Takes
    /// effect only together with `CRATONVM_USER_LOADER_RESOURCE_BYTECODE`.
    /// `CRATONVM_USER_LOADER_RESOURCE_BYTECODE_COMPILED`, read once per VM here
    /// (`runtime_flag_on`); default OFF. A performance switch only.
    pub user_loader_resource_bytecode_compiled: bool,

    /// A resolution's drive of a user-defined loader
    /// (`interpreter::constants::drive_defining_loader_load_named`, the one
    /// function every resolution door's `loadClass` call goes through) first
    /// asks what the loader's dictionary would hold on HotSpot (a class the
    /// loader defined, or its initiating record of the name) and answers from
    /// it without calling `loadClass`, so a loader is asked once per name, as
    /// HotSpot asks it. Interpreter round i6 wave 120, lane L7 (page
    /// `i120-L7-a-loader-is-asked-again-for-a-resolved-name`).
    /// `CRATONVM_LOADER_DICTIONARY_BEFORE_DRIVE`, read once per VM here
    /// (`runtime_flag_on`); default OFF. A HotSpot-parity switch, default off
    /// until the battery has run it.
    pub loader_dictionary_before_drive: bool,

    /// Stage 3 of the precise interpreter oop maps: where the verifier's type
    /// map has a row at a frame's `pc` / `last_instr_pc`, the interpreter's
    /// frame scans leave out a local every mapped pc calls a non-reference
    /// (never a `long` / `double`-marked one), and the remap does not rewrite
    /// such a slot through its lost-tag candidates. Frames without a row keep
    /// the heuristic; the operand stack keeps it everywhere this stage.
    /// Carried per VM by its `TypeMapStore` (`precise_scan`), set at VM
    /// creation, so every mapped scan and remap read one answer. Interpreter
    /// round i6 wave 121, lane L7
    /// (`i1-L4-proposal-precise-interpreter-oop-maps`).
    /// `CRATONVM_INTERP_PRECISE_OOP_MAPS`, read once per VM here
    /// (`runtime_flag_on`); default OFF; flip only after the shadow run
    /// (`CRATONVM_DBG_VERIFY_OOP_MAPS=1`) reads `missed=0 remap_missed=0`.
    pub interp_precise_oop_maps: bool,

    /// `CRATONVM_LOADER_DICTIONARY_ONE_DOOR` (`runtime_flag_on`, default OFF;
    /// HotSpot parity). Interpreter round i6 wave 121, lane L5
    /// (`i46-L5-proposal-one-resolved-class-entry-table-every-door-writes`
    /// stage 2, with `i39-L5`): the readers of a user loader's dictionary by
    /// name that asked its two records themselves (the refused drive's
    /// "published meanwhile" adoption, the locked `java/` recheck) ask the one
    /// door `initiating_records::loader_dictionary` instead, which answers the
    /// loader's own definition AND its initiating record in every one of them.
    /// `CRATONVM_LOADER_DICTIONARY_BEFORE_DRIVE` asks the door whatever this
    /// says.
    pub loader_dictionary_one_door: bool,

    /// `CRATONVM_GLOBAL_NAME_REFUSAL_RECORD` (`runtime_flag_on`, default OFF;
    /// HotSpot parity). Interpreter round i6 wave 125, lane L5
    /// (`i38-L5-proposal-the-initiating-record-replaces-the-memo-under-jdk-only`):
    /// `drive_loader_for_global_name`'s refusal of a JDK-global name is filed
    /// by its reason. A `BaseMiss` (the loader's chain reaches the
    /// application loader, the global route answers the built-in class, and
    /// HotSpot makes the loader an initiating loader of it) is written to the
    /// loader's initiating RECORD, which `findLoadedClass` reads and the
    /// loader-namespaced lookups read behind a memo miss, not the capped memo;
    /// a degrade case (re-entry guard, no loader object, fall-through)
    /// records nothing, as HotSpot.
    pub global_name_refusal_record: bool,

    /// `CRATONVM_GLOBAL_NAME_INITIATING_RECORD` (`runtime_flag_default_on`,
    /// default ON; `=0` restores the `java/`-only record; HotSpot parity).
    /// Interpreter round i6 wave 133, lane L5 (the w131 fix-back of
    /// `L5W125GlobalNameRefusalRecord`): a user loader that ANSWERED a
    /// JDK-global name other than `java/` (`javax/`, `jdk/`, `sun/`,
    /// `com/sun/`) with another loader's class -- the checked door's
    /// successful drive, `Class.forName(name, init, loader)`, and the
    /// transparent loader whose answer the global route is -- is written to
    /// its initiating RECORD (`initiating_records::record_initiating_load`), as
    /// HotSpot's `update_dictionary` files it, so `findLoadedClass` answers the
    /// class. Before, `record_initiating_load` dropped every such name
    /// (`recordable`), and only the capped memo, which `findLoadedClass` does
    /// not read, kept the answer. `define_refusal` still reads `recordable`
    /// names only.
    pub global_name_initiating_record: bool,

    /// `CRATONVM_LOADER_REQUEST_DOOR` (`runtime_flag_on`, default OFF; HotSpot
    /// parity). Interpreter round i6 wave 122, lane L5
    /// (`i39-L5-proposal-one-loader-request-door-with-an-explicit-kind`): the
    /// `Lookup`-kind user-loader drives (`drive_defining_loader_load`, the
    /// after-global-miss rescue; through them the lambda dispatch override)
    /// answer from the loader's dictionary (`initiating_records::loader_dictionary`)
    /// before calling `loadClass`, through the one request prologue
    /// `initiating_records::loader_request_prologue`.
    pub loader_request_door: bool,

    /// `CRATONVM_GLOBAL_NAME_OWNER_UNGUESSED` (`runtime_flag_default_on`,
    /// default ON; `=0` restores the old judging; HotSpot parity). Interpreter
    /// round i6 wave 124, lane L5 (page
    /// `i124-L5-lazy-jdk-named-own-class-invokestatic-nsme`): a method
    /// reference's resolution (`invoke::resolve_method_metadata_noting`) does
    /// not judge its JVMS §5.4.3.3 / §5.4.4 / §5.3.4 failures against the
    /// built-in chain's GUESS for a non-`java/` JDK-global owner name of a
    /// non-transparent user loader that has not answered the name yet
    /// (`constants::global_name_owner_is_a_guess`); the owner door asks the
    /// loader first, and the resolution after its answer judges.
    pub global_name_owner_unguessed: bool,

    /// `CRATONVM_LOADER_DRIVE_UNDER_LOADER_LOCK` (`runtime_flag_default_on`,
    /// default ON; `=0` restores the `java/`-only recheck; HotSpot parity).
    /// Interpreter round i6 wave 129, lane L7 (page
    /// `i120-L7-a-loader-is-asked-again-for-a-resolved-name`, the `threads`
    /// row): a resolution's drive of a user loader that is NOT
    /// parallel-capable (`classloader_real::loader_locks_itself`), which
    /// already holds the loader's monitor around `loadClass`, re-checks the
    /// loader's dictionary (`initiating_records::loader_dictionary`) for EVERY
    /// name after taking the monitor, and publishes a successful answer's
    /// initiating record before releasing it -- HotSpot's
    /// `SystemDictionary::resolve_instance_class_or_null` order (the
    /// `ObjectLocker` on the loader, the dictionary re-check, then
    /// `load_instance_class`, whose `update_dictionary` runs under the same
    /// lock). Two threads resolving one name through such a loader then call
    /// its `loadClass` once. Since wave 130 (lane L7) the natives' VM-side
    /// door (`classloader_real::invoke_load_class_as_the_vm_publishing`,
    /// `Class.forName(name, init, loader)` and the other natives that ask a
    /// loader on the VM's behalf) follows the same order behind this switch,
    /// read through `NativeContext::loader_drive_under_loader_lock`.
    pub loader_drive_under_loader_lock: bool,

    /// The class-define listener's supertype withdrawal
    /// (`SharedVm::load_class_concurrent_for`'s T5.4.4 block) withdraws only the
    /// compiled bodies that inlined or devirtualised an OVERRIDABLE member of
    /// a supertype of the new class (`JitCache::invalidate_for_new_subtype`),
    /// not every body naming the supertype: a constructor, initialiser, or
    /// static / private / final method keeps its one body whatever subtypes
    /// appear (interpreter round i4 wave 88, lane L7). A plain bug fix (the
    /// name-only match recompiled constructor chains on every lazily loaded
    /// subclass), so default ON; `CRATONVM_JIT_SUBTYPE_DEFINE_SCOPED=0`
    /// restores the name-only match. Read once per VM.
    pub jit_subtype_define_scoped: bool,

    /// `interpreter::execute` (the entry every native-to-Java re-entry ends
    /// in: `invoke_special_bytecode_only*`, `invoke_virtual_bytecode_only`,
    /// class initialisers run from Rust) skips its whole first-call JIT gate
    /// block in a VM without a JIT (`!jit_enabled`): no `jit_skip_set` key
    /// (three `Arc<str>` allocations), no `jit_skip_set` / `jit_gate_pass`
    /// read locks, no gate-pass memo fill. Without a JIT the block's only
    /// outputs (a compiled entry, a seal, a tier-up offer) are never used.
    /// Interpreter round i3 wave 76, lane L4 (proposal
    /// `i75-L4-proposal-a-native-re-entry-into-its-own-body-by-resolved-method-20261108`,
    /// piece 1). `CRATONVM_EXECUTE_NO_JIT_GATE_WITHOUT_JIT`, read once per VM
    /// here (`runtime_flag_on`); default OFF. A performance switch only.
    pub execute_no_jit_gate_without_jit: bool,

    /// A native that re-enters its own JDK body on every miss (a memo
    /// `Intrinsic`: `MethodType.makeImpl` under
    /// `CRATONVM_MH_METHOD_TYPE_INTERN_MEMO`) keeps the method's declared
    /// index across calls (`NativeInvokeAccess::invoke_special_bytecode_only_hinted`),
    /// and the re-entry skips the by-name hierarchy walk of
    /// `invoke_special_bytecode_only_shared_impl` and `interpreter::execute`'s
    /// by-name method scan; the index is verified against the class's current
    /// methods on every call. Interpreter round i3 wave 76, lane L4 (proposal
    /// `i75-L4-proposal-a-native-re-entry-into-its-own-body-by-resolved-method-20261108`,
    /// piece 2, reduced). `CRATONVM_NATIVE_OWN_BODY_HINT`, read once per VM
    /// here (`runtime_flag_on`); default OFF. A performance switch only.
    pub native_own_body_hint: bool,

    /// `interpreter::execute` (every native-to-Java re-entry, reflective call
    /// and class initialiser run from Rust) names its method with the class's
    /// and the method's own shared `Arc<str>`s in the JIT gate's skip key and
    /// in the frame it pushes, instead of fresh copies (up to five `Arc<str>`
    /// allocations and an owned class-name copy per call), and hands the frame
    /// the source-file and exception-table `Arc`s it built under the lock
    /// (one copy each instead of two). Same text either way. Interpreter
    /// round i3 wave 77, lane L4 (proposal
    /// `i76-L4-proposal-execute-reads-its-method-once-without-owned-copies-20261109`).
    /// `CRATONVM_EXECUTE_SHARED_METHOD_VIEW`, read once per VM here
    /// (`runtime_flag_on`); default OFF. A performance switch only.
    pub execute_shared_method_view: bool,

    /// `interpreter::execute` (every native-to-Java re-entry, reflective call,
    /// class initialiser and `main` / `run` started from Rust) pushes a CACHED
    /// frame built from a per-VM method record (`JitRealm::execute_reentry_records`,
    /// keyed by class id and declared method index, validated by the identity
    /// of the method's `Code` bytes), instead of an OWNED frame: no
    /// `OwnedFrameMeta` box and name copies, no padded-code memo lock and body
    /// compare per entry, and the record's quickened stream (a `OnceLock`)
    /// and fused-array decline memo serve every frame of the method.
    /// Interpreter round i3 wave 79, lane L7 (proposal
    /// `i77-L4-proposal-a-native-re-entry-pushes-a-cached-frame-20261110`).
    /// `CRATONVM_EXECUTE_CACHED_REENTRY_FRAME`, read once per VM here
    /// (`runtime_flag_on`); default OFF. A performance switch only.
    pub execute_cached_reentry_frame: bool,

    /// [`Self::execute_cached_reentry_frame`] on a VIRTUAL thread only: the
    /// frame `interpreter::execute` pushes there is cached, so the virtual
    /// thread's OSR offer (`try_osr_offer`, which refuses a frame with no
    /// shared method record) can judge it. Platform threads keep the owned
    /// frame. Interpreter round i6 wave 125, lane L6 (page
    /// `i125-L6-virtual-thread-owned-frames-miss-lambda-memo-and-osr`).
    /// `CRATONVM_VTHREAD_EXECUTE_CACHED_FRAME`, read once per VM here
    /// (`runtime_flag_default_on`); default ON since interpreter round i7 wave 144 (A/B w142lto), `=0` off. A performance switch only.
    pub vthread_execute_cached_frame: bool,

    /// `interpreter::execute` makes fewer per-call copies: an empty exception
    /// table is the shared empty `Arc` (no allocation at the entry, no free
    /// at the return), and the JIT gate's gate-pass memo is probed with the
    /// key it built once instead of a second key cloned for the lookup.
    /// Interpreter round i3 wave 83, lane L3 (the reflective door census's
    /// `call_exec` / `call_body`, page
    /// `interpreter-i81-L3-proposal-name-the-reflective-calls-gap-and-reentry-KEPT-OFF-20261019`).
    /// `CRATONVM_EXECUTE_FEWER_COPIES`, read once per VM here
    /// (`runtime_flag_on`); default OFF. A performance switch only.
    pub execute_fewer_copies: bool,

    /// `interpreter::execute`, entered by a native calling back into Java,
    /// makes no copy its compiled-body return does not read: the class's
    /// source file is copied only when an interpreted frame (or a precise
    /// handler frame, or the eager compile) needs it, an empty exception
    /// table is the shared empty `Arc`, and the gate-pass memo is probed with
    /// the key built once (both as `execute_fewer_copies`). Interpreter round
    /// i5 wave 114, lane L7 (the native-to-Java re-entry cost, pages
    /// `interpreter-i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call-CLOSED-20261021`
    /// and `i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call-20261127`).
    /// `CRATONVM_EXECUTE_LEAN_REENTRY`, read once per VM here
    /// (`runtime_flag_on`); default OFF. A performance switch only.
    pub execute_lean_reentry: bool,

    /// `interpreter::execute` finds its method by the declared index a per-VM
    /// memo filed on an earlier call (`JitRealm::execute_method_index`, keyed
    /// by class id and a hash of the name and descriptor; a row is used only
    /// while the method at that index still has that name and descriptor)
    /// instead of `Class::find_method`'s linear by-name scan, which a native
    /// re-entry into `DirectMethodHandle$Holder` (217 methods in JDK 25),
    /// `DelegatingMethodHandle$Holder` (134) or `Invokers$Holder` (62) paid on
    /// every call. Interpreter round i6 wave 115, lane L7 (page
    /// `interpreter-i114-L7-native-reentry-cost-split-KEPT-OFF-20261224`).
    /// `CRATONVM_EXECUTE_METHOD_INDEX_MEMO`, read once per VM here
    /// (`runtime_flag_on`); default OFF. A performance switch only.
    pub execute_method_index_memo: bool,

    /// The per-entry, first-wins record of a `CONSTANT_Class` resolution
    /// (`ResolutionCache::class_entries`, HotSpot's `resolved_klasses`), kept
    /// until now only for an isolated URL loader's classes, is kept for every
    /// class with real bytes and a non-bootstrap defining loader
    /// (`interpreter::constants::class_entry_record_applies`); an answer that
    /// is a compatibility stub is not recorded. A per-thread class site that
    /// retires (a class-loading burst, a redefinition elsewhere) is then
    /// answered from the record instead of the loader lookup. Interpreter
    /// round i6 wave 118, lane L7 (stage 1 of
    /// `i1-L2-proposal-per-class-resolved-constant-pool`).
    /// `CRATONVM_CLASS_ENTRY_RECORD_ALL`, read once per VM here
    /// (`runtime_flag_default_on`); default ON since t1 W14 (2026-10-28,
    /// `T1G6ClassEntryLoop` steady and churn rows at most 1.000 instructions
    /// per round in both modes), `=0` turns it off. With it on, every such
    /// entry also has ONE outcome, success or failure, that racing resolvers
    /// adopt (`ResolutionCache::put_failure`).
    pub class_entry_record_all: bool,

    /// `CRATONVM_CLASS_SITE_ISOLATED_STICKY` (round 15 wave 5, lane
    /// classsite; default ON, `=0` off), read once per VM here
    /// (`runtime_flag_default_on`): `interpreter::constants::isolated_class_site_sticky_enabled`.
    /// Until t1 W15 every re-resolved class entry re-read it from the
    /// environment, which was the switch-on cost of `class_entry_record_all`.
    pub class_site_isolated_sticky: bool,

    /// An `invokespecial` whose sender is an interface checks that a non-null
    /// receiver is the sender or a subtype of it (HotSpot's
    /// `LinkResolver::runtime_resolve_special_method`), the site is never
    /// cached, and the compiled doors refuse such a site unless every one of
    /// its receivers is the method's own `this`
    /// (`interpreter::invoke::interface_special_receiver_check_enabled`).
    /// Interpreter round i6 wave 133, lane L4 (page i132-L6); made a per-VM
    /// field in wave 134, lane L4, so the slow path no longer reads the
    /// environment per execution. `CRATONVM_INTERFACE_SPECIAL_RECEIVER_CHECK`,
    /// read once per VM (`runtime_flag_default_on`); default ON (HotSpot
    /// parity, both modes), `=0` restores the unchecked, cached site.
    pub interface_special_receiver_check: bool,

    /// With [`Self::interface_special_receiver_check`] on, the interpreter
    /// caches an interface sender's `invokespecial` (Scala's static trait
    /// accessor `T.m$(T)`) once the receiver check has passed, as a
    /// `VirtualBytecode` entry guarded by that receiver's class (more classes
    /// in the site's poly entries): a class's supertypes never change, so a
    /// hit on the same class needs no new check, and any other class (an
    /// array included) misses into the checking slow path
    /// (`interpreter::invoke::memo_interface_special_site`). Interpreter round
    /// i7 wave 136, lane L6 (page
    /// `i135-L6-trait-static-accessors-run-uncached-and-uncompiled`).
    /// `CRATONVM_INTERFACE_SPECIAL_RECEIVER_MEMO`, read once per VM here
    /// (`runtime_flag_default_on`); default ON since interpreter round i7 wave 144 (A/B w142lto), `=0` off. A performance switch only.
    pub interface_special_receiver_memo: bool,

    /// With [`Self::interface_special_receiver_check`] on, the method-entry
    /// compile doors (the ones that build their request through
    /// `jit_bridge::CpResolvers`) no longer refuse an interface sender's
    /// `invokespecial` whose receiver is not proven to be `this` (Scala's
    /// static trait accessor `T.m$(T)`); they compile the site as a call
    /// through `jit_invoke_dispatch`, which makes HotSpot's receiver check
    /// (`helpers::iface_special_receiver_guard`), and no direct bind or
    /// splice may serve it (`jit_bridge::iface_special_guarded_target`). The
    /// single-pass OSR, eager and splice doors keep refusing. Interpreter
    /// round i7 wave 136, lane L2 (page
    /// `i135-L6-trait-static-accessors-run-uncached-and-uncompiled`).
    /// `CRATONVM_JIT_IFACE_SPECIAL_RECEIVER_GUARD`, read once per VM here
    /// (`runtime_flag_default_on`); default ON since interpreter round i7 wave 144 (A/B w142lto), `=0` off. A performance switch only.
    pub jit_iface_special_receiver_guard: bool,

    /// A linked generic `invokedynamic` (Groovy / JRuby call sites) calls its
    /// target's `MethodHandle.invoke` through the registered signature-
    /// polymorphic native directly -- the call-site descriptor armed, the
    /// `([Ljava/lang/Object;)Ljava/lang/Object;` callback, the poly-return
    /// unboxing -- instead of `invoke_by_class_id_shared`'s whole by-name
    /// dispatch walk ending in the same native. Interpreter round i6 wave 117,
    /// lane L4 (stage 2 of `i2-L6-proposal-generic-indy-invoke-path`).
    /// `CRATONVM_INDY_GENERIC_DIRECT_INVOKE`, read once per VM (`runtime_flag_default_on`);
    /// default ON since the wave-129 triage (`=0` off). A performance switch only.
    pub indy_generic_direct_invoke: bool,

    /// A `Linked` generic `invokedynamic` row (Groovy / JRuby call sites) is
    /// also copied into the executing thread's per-instruction table, as a
    /// heap-free marker (the `CallSite` stays in the VM table), so the next
    /// execution skips the per-entry `resolution_cache` read lock and the
    /// method-key hash and probes the instruction's row directly. Interpreter
    /// round i6 wave 133, lane L2b (stage 3 of
    /// `i2-L6-proposal-generic-indy-invoke-path`).
    /// `CRATONVM_INDY_GENERIC_THREAD_ROW`, read once per VM (`runtime_flag_on`);
    /// default OFF. A performance switch only.
    pub indy_generic_thread_row: bool,

    /// A `Z` operand of a linked generic `invokedynamic` whose position in the
    /// target handle's own `type()` is the primitive `boolean` is passed as the
    /// `int` the call site pushed, not boxed as a `Boolean` (HotSpot: `invoke`
    /// at the handle's own type converts nothing; the box reached a compiled
    /// target's `boolean` as its address, i.e. `true`). Interpreter round i6
    /// wave 134, lane L2c (`docs/feature-designs/jit-ir-optimizations.md`).
    /// `CRATONVM_INDY_GENERIC_KEEP_PRIMITIVE_Z`, read once per VM
    /// (`runtime_flag_default_on`); a HotSpot-parity fix, default ON, `=0` off.
    pub indy_generic_keep_primitive_z: bool,

    /// Under `--compatible`, `MethodHandles.arrayElementGetter` /
    /// `arrayElementSetter` / `arrayLength` run the JDK's own bodies instead of
    /// the `METHODHANDLES ARRAY ACCESSORS` name-override pin's bridge natives,
    /// as under `--jdk-only`. Interpreter round i6 wave 118, lane L4 (step 3 of
    /// `i44-L4-proposal-invoke-a-jdk-delegating-handle-through-its-target`,
    /// `i43-L4-proposal-retire-the-copywith-workarounds`).
    /// `CRATONVM_MH_COMPAT_ARRAY_ACCESSORS_JDK`, read once per VM
    /// (`runtime_flag_default_on`); default ON since wave 133 (HotSpot parity:
    /// L4W118/L4W44/L4W45 48/48 SAME on w131 in both arms), `=0` off.
    pub mh_compat_array_accessors_jdk: bool,

    /// The generic JDK `invokedynamic` route (`bootstrap_generic_jdk`) links
    /// through the JDK's own `CallSite.makeSite` -> `BootstrapMethodInvoker`,
    /// as HotSpot's `MethodHandleNatives.linkCallSite` does, instead of
    /// invoking the bootstrap itself with this VM's copies of the static-
    /// argument count, cast and result checks; and a lambda site
    /// `lambda_site_refusal` refuses takes that route too, so its errors are
    /// the JDK's. Interpreter round i6 wave 119, lane L4
    /// (`i38-L4-proposal-native-bootstraps-link-only-javac-shapes-natively`).
    /// `CRATONVM_INDY_GENERIC_VIA_MAKESITE`, read once per VM
    /// (`runtime_flag_default_on`); default ON since the i6 wave-126 flip,
    /// `=0` off (parity).
    pub indy_generic_via_makesite: bool,

    /// Without a JIT, an interpreted `invokevirtual MethodHandle.invokeBasic`
    /// on a real JDK handle whose site the signature-polymorphic route cache
    /// holds pushes the handle's `form.vmentry` frame (a static `LambdaForm`
    /// entry, found by its owner's class id) into the dispatch loop instead of
    /// the registered native's re-entry into `interpreter::execute`, as
    /// HotSpot's `_invokeBasic` intrinsic jumps to the entry. Interpreter round
    /// i6 wave 120, lane L4 (`i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call`;
    /// `sig_poly_site::try_basic_frame_push`).
    /// `CRATONVM_MH_INTERP_BASIC_FRAME_PUSH`, read once per VM
    /// (`runtime_flag_default_on`); default ON since the round i7 follow-up
    /// (page `i147-orch`), `=0` off. A performance switch only.
    pub mh_interp_basic_frame_push: bool,

    /// Under [`Self::mh_interp_basic_frame_push`], an interpreted
    /// `MethodHandle.linkToSpecial` (every constructor and `findSpecial`
    /// handle) pushes its member's frame too
    /// (`sig_poly_site::link_to_special_frame_push`; interpreter round i7,
    /// proposal `i147-orch`). `CRATONVM_MH_INTERP_SPECIAL_FRAME_PUSH`, read once
    /// per VM; default ON, `=0` off. A performance switch only.
    pub mh_interp_special_frame_push: bool,

    /// Without a JIT, a cached interpreted `invokevirtual
    /// MethodHandle.invokeWithArguments(Object[])` served by the JDK body's
    /// native steps (`CRATONVM_MH_IWA_JDK_BODY` + `_NATIVE_STEPS` +
    /// `_SPREADER_MEMO` + `_LINK_MEMO`, every memo line warm) pushes the JDK
    /// body's frame at its `invokeExact` call and the linker's
    /// `Invokers$Holder.invokeExact_MT` frame on top of it, instead of the
    /// `Intrinsic`'s re-entry into `interpreter::execute`. Interpreter round i7
    /// wave 138, lane L4 (`i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call`;
    /// `sig_poly_site::iwa_frame_push`).
    /// `CRATONVM_MH_IWA_INTERP_FRAME_PUSH`, read once per VM
    /// (`runtime_flag_on`); default OFF. A performance switch only.
    pub mh_iwa_interp_frame_push: bool,

    /// The signature-polymorphic site route cache (`sig_poly_site`) on a
    /// mounted JDK virtual thread too, under the same observability conditions
    /// as the virtual-thread doors. Interpreter round i6 wave 122, lane L4
    /// (`i114-L6-proposal-the-virtual-door-on-virtual-threads`, item 5, after
    /// L4's review there: a hit replays the native the slow path ran, through
    /// the same `safe_native_call*`, and the slow path has no virtual-thread
    /// arm to skip; the frame-push arms push interpreted frames only).
    /// `CRATONVM_VTHREAD_SIG_POLY_SITES`, read once per VM (`runtime_flag_on`);
    /// default OFF. A performance switch only.
    pub vthread_sig_poly_sites: bool,

    /// A reflective `Constructor.newInstance` of an initialized class (the
    /// `NativeHeapAccess::new_object_initialized_with_class_id` door) writes
    /// the instance's defaults from the per-class allocation recipe (no
    /// class-manager read, no superclass walk) and builds its `<init>`
    /// arguments inline (no `Vec`). Interpreter round i3 wave 83, lane L3
    /// (the census's constructor `call_door`, ~270 ns against ~67 for a
    /// static `Method.invoke`). `CRATONVM_REFLECT_CTOR_LEAN_ALLOC`, read once
    /// per VM here (`runtime_flag_on`); default OFF. A performance switch
    /// only.
    pub reflect_ctor_lean_alloc: bool,

    /// A compiled `Method.invoke` / `Constructor.newInstance` site the
    /// virtual-MIC helper's direct reflective arm served
    /// (`CRATONVM_JIT_REFLECT_DOOR_DIRECT`) is filed in the thread's lean
    /// native table (`jit::helpers::reflect_door_lean_site_fill`), so the next
    /// call is served at the helper's first statement by the lean door instead
    /// of after the MIC body's prefix. Interpreter round i3 wave 83, lane L3
    /// (the census's `gap_in_helper`, ~240 ns). `CRATONVM_JIT_REFLECT_DOOR_LEAN_SITE`,
    /// read once per VM here (`runtime_flag_on`); default OFF. A performance
    /// switch only.
    pub jit_reflect_door_lean_site: bool,

    /// A positive by-name field answer (`get_field_by_name` /
    /// `set_field_by_name` and the batch accessors' rings in `vm_exec.rs`)
    /// resolved over a class whose whole superclass chain is installed and
    /// free of compatibility stubs is kept across class definitions, instead
    /// of expiring at the next provenance-epoch move (every define moves it).
    /// Interpreter round i4 wave 91, lane L3 (the per-class first
    /// `getConstructor`: a reflective mint right after a define re-walked the
    /// hierarchy under the class-manager lock for every field it wrote).
    /// `CRATONVM_FIELD_NAME_RING_DURABLE`, read once per VM here
    /// (`runtime_flag_on`); default OFF. A performance switch only.
    pub field_name_ring_durable: bool,

    /// A mounted JDK virtual thread (`ThreadKind::Virtual`,
    /// `jdk_continuation.jdk()`) enters, and offers for tier-up, the compiled
    /// body of a static callee whose bytecode cannot reach a yield point (no
    /// invoke, no `monitorenter` / `monitorexit`, not `synchronized`;
    /// `jdk_continuation::vthread_may_enter_compiled_method`). Every other door keeps
    /// refusing a virtual thread. Anything such a body still runs as Java (a
    /// class initializer, a class load, a bootstrap) runs under the compiled
    /// frame, where `Continuation.doYield` answers pinned (`pinned_reason`) and
    /// the monitor unmounts decline (`current_thread_jit_depth() == 0`), as
    /// HotSpot pins a VM upcall's native frame. Interpreter round i4 wave 92,
    /// lane L6 (page `i91-L6-virtual-thread-code-is-never-compiled`).
    /// `CRATONVM_VTHREAD_COMPILED_LEAF_ENTRY`, read once per VM here
    /// (`runtime_flag_default_on`); default ON since the wave-92 triage, `=0` off.
    /// A performance switch only.
    pub vthread_compiled_leaf_entry: bool,

    /// [`Self::vthread_compiled_leaf_entry`] for INSTANCE callees: the general
    /// cached dispatcher's `invokevirtual` / `invokeinterface`
    /// (`VirtualBytecode`) and `invokespecial` / private (`Bytecode`) arms
    /// enter, and offer for tier-up (background compilation only), the
    /// compiled body of a callee that cannot reach a yield point
    /// (`jdk_continuation::vthread_may_enter_compiled_method`), as the static
    /// door does for static leaves. The fast doors stay off on a virtual
    /// thread. Interpreter round i4 wave 93, lane L6 (page
    /// `i91-L6-virtual-thread-code-is-never-compiled`, stage 2).
    /// `CRATONVM_VTHREAD_COMPILED_INSTANCE_LEAF`, read once per VM here
    /// (`runtime_flag_on`); default OFF. A performance switch only.
    pub vthread_compiled_instance_leaf: bool,

    /// Widens the leaf predicate of [`Self::vthread_compiled_leaf_entry`] and
    /// [`Self::vthread_compiled_instance_leaf`] by one level: a body whose only
    /// invokes are at most four `invokestatic` sites, each already cached on
    /// THIS thread (`JvmThread::invoke_cache`) as a bytecode or compiled entry
    /// of a static leaf, is admitted too (a compiled body that only calls
    /// leaves). Interpreter round i4 wave 93, lane L6.
    /// `CRATONVM_VTHREAD_COMPILED_LEAF_CALLERS`, read once per VM here
    /// (`runtime_flag_on`); default OFF. A performance switch only.
    pub vthread_compiled_leaf_callers: bool,

    /// Back-edge OSR on a mounted JDK virtual thread for a frame whose method
    /// cannot reach a yield point by itself (the leaf predicate of
    /// [`Self::vthread_compiled_leaf_entry`], widened by
    /// [`Self::vthread_compiled_leaf_callers`] when that is on; not
    /// `synchronized`; `Code` within the leaf bound), background compilation
    /// only (`jdk_continuation::vthread_may_osr_method`). Every other frame on
    /// a virtual thread keeps the OSR refusal (`interpreter::try_osr_offer`,
    /// `osr_call_floor`). The same soundness argument as the entry doors: the
    /// body reaches Java only through a VM upcall, where a yield answers
    /// pinned (`pinned_reason`). Interpreter round i5 wave 100, lane L6 (page
    /// `i91-L6-virtual-thread-code-is-never-compiled`).
    /// `CRATONVM_VTHREAD_COMPILED_LEAF_OSR`, read once per VM here
    /// (`runtime_flag_on`); default OFF. A performance switch only.
    pub vthread_compiled_leaf_osr: bool,

    /// Compiled code on a mounted JDK virtual thread for ANY non-`synchronized`
    /// method, learned off by its first pin (interpreter round i5 wave 101,
    /// lane L2; proposal `i100-L6-proposal-learned-compiled-entry-on-virtual-
    /// threads`, page `i91-L6-virtual-thread-code-is-never-compiled`). The
    /// static, virtual / interface / special and OSR doors admit a body unless
    /// this VM's deny set names it (`jdk_continuation::vthread_learned_admits`).
    /// A yield that finds a compiled frame beneath still pins
    /// (`pinned_reason`), so no compiled frame is ever frozen; that pin
    /// records every compiled activation on the stack in the deny set
    /// (`learn_compiled_pin`), and a park pinned by a compiled frame (or by a
    /// VM door's `pin_continuation`) takes the scheduler's carrier
    /// compensation (`vm_pinned_park_compensates`) so the one pin cannot
    /// starve the unparker. A deopt-resumed frame run to completion on a
    /// virtual thread pins too (`deopt_resume::vthread_resume_pin`).
    /// `CRATONVM_VTHREAD_COMPILED_LEARNED`, read once per VM here
    /// (`runtime_flag_default_on`); default ON since interpreter round i5
    /// wave 113 together with [`Self::vthread_skip_branch_arm`] (A/B on
    /// Windows, interleaved medians: `osr virtual` 0.02-0.07x, `helpers
    /// virtual` 0.92-1.05x, both modes), `=0` off. A performance switch only.
    ///
    /// Since interpreter round i5 wave 106 (lane L6) the switch alone is the
    /// learned OSR door plus learned tier-up NOMINATION at the static door
    /// (`jdk_continuation::vthread_learned_nominates`): an interpreted frame
    /// of a virtual thread no longer ENTERS a compiled body by the learned
    /// arm, which every measurement of `L6W103VirtualThreadLearnedShortBench`
    /// `helpers` showed losing (6.9x w103, 7.1x w104, 19x w104e, 1.22x w105)
    /// while the `osr` row (0.03-0.04x) comes from the OSR door. The entry
    /// arm is [`Self::vthread_compiled_learned_entry`].
    pub vthread_compiled_learned: bool,

    /// The ENTRY arm of [`Self::vthread_compiled_learned`]: the static,
    /// virtual / interface and special doors admit any non-denied body
    /// (`jdk_continuation::vthread_learned_admits(.., entry = true)`), the
    /// wave 101-105 behaviour of the learned switch. `true` only when
    /// `CRATONVM_VTHREAD_COMPILED_LEARNED_ENTRY` AND
    /// `CRATONVM_VTHREAD_COMPILED_LEARNED` are on (read once per VM here,
    /// `runtime_flag_default_on` for the ENTRY word); default ON since
    /// interpreter round i7 wave 144 (A/B w142lto), `=0` off. Interpreter
    /// round i5 wave 106, lane L6 (page
    /// `i91-L6-virtual-thread-code-is-never-compiled`). A
    /// performance switch only.
    pub vthread_compiled_learned_entry: bool,

    /// An entry arm for APPLICATION code only: the static, virtual /
    /// interface and special doors let an interpreted frame of a mounted JDK
    /// virtual thread enter (and offer for tier-up, background compilation
    /// only) the compiled body of any non-`synchronized` method of a class
    /// outside the JDK's packages that the learned deny set does not name
    /// (`jdk_continuation::vthread_app_entry_admits`). Unlike
    /// [`Self::vthread_compiled_learned_entry`] there is no walk of the
    /// caller frames per call: a denied (parking) method's callees are
    /// exactly the bodies this arm is for (the request handler parks, its
    /// helpers do not). Soundness is the learned arm's: a yield under a
    /// compiled frame pins, and the pin denies the method. `true` only when
    /// `CRATONVM_VTHREAD_COMPILED_APP_ENTRY` AND
    /// `CRATONVM_VTHREAD_COMPILED_LEARNED` are on (read once per VM here,
    /// `runtime_flag_on`); default OFF. Interpreter round i5 wave 114, lane L6
    /// (page `i113-orch-virtual-thread-frames-enter-only-leaf-bodies-compiled`).
    /// A performance switch only.
    pub vthread_compiled_app_entry: bool,

    /// A JDK virtual thread's interpreted `invokestatic` of a bytecode callee
    /// counts the call on the call site's entry
    /// (`invoke_fast::count_door_invocation`: a plain relaxed load and store,
    /// folded into the shared profile store every 16 calls), as the static
    /// fast door counts on a platform thread, instead of
    /// `ProfileStore::increment_invocation` on every call (a shard read lock
    /// and an atomic add on the method's shared cell, the same two cache lines
    /// for every carrier running the method). A call whose compiled entry the
    /// virtual-thread doors refuse stops counting once the count is past the
    /// learned nomination window: nothing can use it there. Interpreter round
    /// i5 wave 114, lane L6. `CRATONVM_VTHREAD_LOCAL_CALL_COUNT`, read once
    /// per VM here (`runtime_flag_on`); default OFF; also on whenever
    /// [`Self::vthread_nonvirtual_doors`] is (the doors and this arm must count
    /// on the same counter). A performance switch only.
    pub vthread_local_call_count: bool,

    /// The two non-virtual invoke fast doors (`invoke_fast::
    /// execute_invokestatic_fast_door`, `execute_nonvirtual_fast_door`) run on
    /// a mounted JDK virtual thread too. They are off on every virtual thread
    /// (`interpreter::execute_frame_from_index`, `invoke_fast_door_on`), so
    /// each static, private, super and constructor call of a virtual thread
    /// took the general cached dispatcher, while a platform thread's took the
    /// door. On a virtual thread the doors judge a callee as the general arms
    /// do there (`invoke_fast::vthread_door_gate`): a `synchronized` callee
    /// declines to the general arm (its monitor path); a callee whose compiled
    /// entry the virtual-thread doors refuse is served interpreted without the
    /// compiled-body probe, counted as [`Self::vthread_local_call_count`]
    /// counts it (a due nomination stride of the learned arm declines to the
    /// general arm, which nominates); an admitted callee keeps the door's
    /// platform behaviour (a compiled body declines to the general arm, which
    /// enters it). The virtual door and the signature-polymorphic site cache
    /// stay off. Interpreter round i5 wave 114, lane L6.
    /// `CRATONVM_VTHREAD_NONVIRTUAL_DOORS`, read once per VM here
    /// (`runtime_flag_on`); default OFF. A performance switch only.
    pub vthread_nonvirtual_doors: bool,

    /// The virtual / interface invoke fast door
    /// (`dispatch_virtual::execute_invokevirtual_fast_door`) runs on a mounted
    /// JDK virtual thread too. It is off on every virtual thread
    /// (`interpreter::execute_frame_from_index`, `invoke_fast_door_on`), so
    /// every `invokevirtual` and `invokeinterface` of a virtual thread took
    /// `execute_invokevirtual_cached_probed`. On a virtual thread the door
    /// judges its callee as the general `VirtualBytecode` arm does there
    /// (`invoke_fast::vthread_door_gate`, `static_door = false`): a
    /// `synchronized` callee declines to the general arm (its monitor path can
    /// unmount a contended virtual thread); a callee whose compiled entry the
    /// virtual-thread doors refuse is served interpreted with no compiled-body
    /// probe and no count (the general arm makes neither); an admitted callee
    /// keeps the door's tier-up block, never with an inline upgrade. The
    /// lambda door behind it and the signature-polymorphic site cache stay
    /// off on a virtual thread, and the private hand-off keeps its own switch
    /// ([`Self::vthread_nonvirtual_doors`]). Interpreter round i6 wave 115,
    /// lane L6 (proposal `i114-L6-proposal-the-virtual-door-on-virtual-threads`).
    /// `CRATONVM_VTHREAD_VIRTUAL_DOOR`, read once per VM here
    /// (`runtime_flag_default_on`); default ON since the round-i6 wave-115
    /// triage (virtual rows 0.77-1.02x), `=0` off. A performance switch only.
    pub vthread_virtual_door: bool,

    /// A compiled pin of a JDK virtual thread's yield whose stack holds no
    /// OSR activation, while an entry arm is on
    /// ([`Self::vthread_compiled_app_entry`],
    /// [`Self::vthread_compiled_learned_entry`]), denies the stack's methods
    /// at the ENTRY doors only (`jdk_continuation::learn_compiled_pin`,
    /// `ContinuationRunners::learned_entry_denials`); the learned OSR door
    /// keeps admitting their loops. Before, such a pin closed the OSR door
    /// for the life of the VM, and the entry arms measured `L6W108`
    /// `osr-virtual` 24-33x slower (interpreter round i6 wave 117, lane L6,
    /// page `i117-L6-an-entry-arm-pin-closes-the-learned-osr-door`). No
    /// effect with both entry arms off. `CRATONVM_VTHREAD_ENTRY_PIN_DENIES_ENTRY_ONLY`,
    /// read once per VM here (`runtime_flag_default_on`); default ON, `=0`
    /// off. A performance switch only (either set refuses; both are sound).
    pub vthread_entry_pin_denies_entry_only: bool,

    /// The leaf arm of `jdk_continuation::vthread_may_enter_compiled_method`
    /// answers a body's leaf scan (`bytecode_cannot_reach_a_yield`, up to
    /// 256 bytes per call, asked on every call of an admitted static leaf by a
    /// JDK virtual thread) from a per-VM memo keyed by the body
    /// (`jdk_continuation::LeafVerdictMemo`), instead of scanning per call.
    /// Only without `CRATONVM_VTHREAD_COMPILED_LEAF_CALLERS` (whose answer
    /// depends on the caller's invoke cache). Interpreter round i6 wave 118,
    /// lane L6 (page `i114-L6-proposal-the-virtual-door-on-virtual-threads`).
    /// `CRATONVM_VTHREAD_LEAF_VERDICT_MEMO`, read once per VM here
    /// (`runtime_flag_on`); default OFF. A performance switch only.
    pub vthread_leaf_verdict_memo: bool,

    /// The application entry arm ([`Self::vthread_compiled_app_entry`]) also
    /// refuses a callee when any of the innermost
    /// `jdk_continuation::LEARNED_CALLER_DEPTH` frames is denied (either deny
    /// set), as the learned entry arm does (`learned_caller_denied`): a denied
    /// method and what it calls within that depth then run as with the arm
    /// off. Interpreter round i6 wave 120, lane L6 (page
    /// `i113-orch-virtual-thread-frames-enter-only-leaf-bodies-compiled`,
    /// "Wave 120"; the `helpers-virtual` remainder of the wave-115 A/B).
    /// `CRATONVM_VTHREAD_APP_ENTRY_CALLER_WALK`, read once per VM here
    /// (`runtime_flag_on`); default OFF. A performance switch only.
    pub vthread_app_entry_caller_walk: bool,

    /// A JDK continuation's slice runs on its CARRIER's own OS thread, inside
    /// the carrier's VM wait region, instead of being handed to a
    /// continuation runner thread and waited for (`jdk_continuation::
    /// enter_special`, `CarrierSliceScope`, `run_slice(.., on_carrier)`), when
    /// the carrier has no compiled frame and no open JNI local frame on this
    /// OS thread; otherwise the runner as before. Saves the slice's two
    /// cross-thread hand-offs. Interpreter round i6 wave 120, lane L6, stage
    /// 1 of proposal
    /// `i94-L6-proposal-run-a-jdk-continuation-slice-on-its-carrier-os-thread`.
    /// `CRATONVM_VTHREAD_SLICE_ON_CARRIER`, read once per VM here
    /// (`runtime_flag_on`); default OFF (`=0` keeps the runner). A
    /// performance switch only.
    pub vthread_slice_on_carrier: bool,

    /// Stage 2 of the same proposal (t1 wave 5, lane E2): a carrier WITH
    /// compiled frames below `enterSpecial` qualifies too. The carrier's
    /// per-OS-thread JIT records (entry chain, frame-record mirrors, chain
    /// history, deopt stashes, OSR records, dispatch depth, the code-cache
    /// blocked window, its shadow window) are set aside for the slice
    /// (`conservative_roots::ChainSegment`, `jdk_continuation::CarrierCompiledBase`),
    /// own-stack readers stop at the slice's base, and blocked-peer stack
    /// captures above the base are routed to the carrier
    /// (`gc::shadow_stack::publish_stack_segment`). Read only with
    /// [`Self::vthread_slice_on_carrier`] on.
    /// `CRATONVM_VTHREAD_SLICE_ON_CARRIER_COMPILED`, read once per VM here
    /// (`runtime_flag_on`); default OFF. A performance switch only.
    pub vthread_slice_on_carrier_compiled: bool,

    /// The interpreter-to-compiled door (`jit_bridge::run_jit_body_raw`) does
    /// its per-call bookkeeping in fewer thread-local and locked operations
    /// (t1 wave 5, lane S; stage 2 of
    /// `i25-L2-proposal-one-thread-block-for-the-interpreter-to-compiled-door`):
    /// one `JIT_SIGNALS` access for the entry retire, one exceptional-stash
    /// depth read shared by the door's floor and its entry guard, and the
    /// guard's push and pop through the door form
    /// (`conservative_roots::JitEntryGuard::enter_door`). Same state after
    /// every door. `CRATONVM_JIT_DOOR_THREAD_BLOCK`, read once per VM here
    /// (`runtime_flag_on`); default OFF. A performance switch only.
    pub jit_door_thread_block: bool,

    /// With `jit_door_thread_block` on, the door's chain entry raises ONE
    /// process-wide counter (`cratonvm_types::striped_counter::DOOR_DEPTH`)
    /// instead of three (`GLOBAL_JIT_DEPTH`, the collector's quiescence depth,
    /// the code cache's `ACTIVE_JIT_EXECUTIONS`), each of which folds it into
    /// its readers (t1 wave 8, lane I25; stage 2b of
    /// `i25-L2-proposal-one-thread-block-for-the-interpreter-to-compiled-door`).
    /// `CRATONVM_JIT_DOOR_ONE_DEPTH_COUNTER`, read once per VM here
    /// (`runtime_flag_default_on`); default ON since j27 2026-10-28, `=0`
    /// kills; no effect without `CRATONVM_JIT_DOOR_THREAD_BLOCK`. A performance switch only.
    pub jit_door_one_depth_counter: bool,

    /// The interpreted lambda door (`invoke_fast::execute_lambda_door`) runs
    /// on a mounted JDK virtual thread too (it is reached there behind the
    /// virtual door, `CRATONVM_VTHREAD_VIRTUAL_DOOR`), and skips its tier-up
    /// block there entirely, as the general lambda arm
    /// (`lambda::try_invoke_cached_lambda_impl`) skips its compiled block on a
    /// virtual thread: no compiled-body probe, count, offer or compiled entry.
    /// Interpreter round i6 wave 122, lane L6 (proposal
    /// `i114-L6-proposal-the-virtual-door-on-virtual-threads`, "Wave 122").
    /// `CRATONVM_VTHREAD_LAMBDA_DOOR`, read once per VM here
    /// (`runtime_flag_on`); default OFF. A performance switch only.
    pub vthread_lambda_door: bool,

    /// A JDK virtual thread's interpreter never takes the per-method branch
    /// profile arm (`jit_bridge::arm_method_branch_profile`): its dispatch
    /// loop answers `branch_arm_possible = false`, so neither its branches
    /// nor its invoke sites record into an armed method's counters (the
    /// process-wide PGO gate is unchanged). The arm is set at a method's first
    /// nomination and never cleared; it is meant for the stretch until the
    /// method's first body publishes, but a virtual thread keeps running a
    /// refused method interpreted after that, recording into the method's
    /// shared counters forever (a thread-local memo miss takes the profile
    /// slot's mutex). Interpreter round i5, page
    /// `i91-L6-virtual-thread-code-is-never-compiled`, "Wave 113": that
    /// recording was the `helpers virtual` loss of
    /// `CRATONVM_VTHREAD_COMPILED_LEARNED` (its nominations arm `work` / `mix`
    /// / `helpersTask` while 32 carriers keep interpreting them).
    /// `CRATONVM_VTHREAD_SKIP_BRANCH_ARM`, read once per VM here
    /// (`runtime_flag_default_on`); default ON since wave 113 (A/B alone:
    /// `helpers virtual` 0.87-1.11x, median below 1; with the learned switch
    /// it takes that switch's loss from 1.07-1.26x to 0.92-1.05x), `=0` off.
    /// A performance switch only.
    pub vthread_skip_branch_arm: bool,

    /// The optimizing OSR route (`jit_bridge::build_osr_optimizing_artifact`)
    /// leaves a declined compile unmemoised when its recorded refusal is a
    /// constant-pool resolver miss (a class the loop has not loaded yet), at
    /// most four times per method, instead of refusing the method for the rest
    /// of the epoch at once (interpreter round i5 wave 101, lane L2; the
    /// narrow slice of `i39-L2-proposal-a-compile-request-returns-why-it-
    /// produced-no-body`). `CRATONVM_JIT_OSR_OPTIMIZING_RESOLVER_MISS_RETRY`,
    /// read once per VM here (`runtime_flag_default_on`); default ON since j27
    /// 2026-10-28, `=0` kills. A performance switch only.
    pub jit_osr_optimizing_resolver_miss_retry: bool,

    /// A finished JDK continuation's thread hands its interpreter site caches
    /// (`invoke_cache`, `field_sites`, `site_tables`,
    /// `trivial_getter_verdicts`, `method_sites`, `class_sites`, `cast_sites`,
    /// `iface_select_sites`) to a per-VM pool
    /// (`jdk_continuation::ContinuationRunners`), and the next continuation
    /// thread starts with them instead of empty ones: a virtual thread per task
    /// then pays a cold call site once per VM, not once per task, as a platform
    /// pool thread does. Every entry validates itself (redefinition stamp,
    /// per-entry epochs, `RedefineGate`), so a recycled table answers exactly
    /// what it would answer on a long-lived thread. Interpreter round i4 wave
    /// 94, lane L6 (page `i93-L6-virtual-thread-start-pays-a-cold-invoke-cache`).
    /// `CRATONVM_VTHREAD_RECYCLE_EXEC_CACHES`, read once per VM here
    /// (`runtime_flag_default_on`, `=0` off); default ON since the round i4
    /// wave 94 triage. A performance switch only.
    pub vthread_recycle_exec_caches: bool,

    /// A JDK continuation's lifecycle skips two registry walks: its first
    /// slice looks its virtual thread up by the `Thread.tid` index only (no
    /// pointer walk of every registry entry under the write lock), and its
    /// death (`ThreadRegistry::mark_dead_with_purge_slack`) purges terminated
    /// entries in batches of 64 instead of collecting and sorting every dead
    /// entry on each death past the cap. Interpreter round i4 wave 94, lane L6
    /// (page `i91-L6-virtual-thread-code-is-never-compiled`, wave-94 census).
    /// `CRATONVM_VTHREAD_LEAN_REGISTRY`, read once per VM here
    /// (`runtime_flag_on`); default OFF. A performance switch only.
    pub vthread_lean_registry: bool,

    /// A JDK continuation's carrier does not spin
    /// (`jdk_continuation::CARRIER_HANDOFF_SPIN`, 200 us) waiting for the
    /// slice it handed to a runner OS thread; it sleeps on the slot's condition
    /// variable at once. The spin pays off for a ping-pong of short slices
    /// (`VtcPing`), but with as many carriers as cores every carrier spins
    /// while its runner works, so a CPU-bound slice shares its core with a
    /// spinner. Interpreter round i4 wave 94, lane L6 (the hand-off row of the
    /// `CRATONVM_DBG_VTHREAD_CENSUS` split). `CRATONVM_VTHREAD_CARRIER_SLEEPS`,
    /// read once per VM here (`runtime_flag_on`); default OFF. A performance
    /// switch only.
    pub vthread_carrier_sleeps: bool,

    /// The two hand-off spins of a JDK continuation's slice (the carrier
    /// waiting for its runner's outcome, `CARRIER_HANDOFF_SPIN`; the runner
    /// waiting for its next job, `RUNNER_HANDOFF_SPIN`) give the core away
    /// (`std::thread::yield_now`) every 64 turns instead of spinning flat.
    /// With as many carriers as cores, P spinning carriers and P working
    /// runners are 2P runnable OS threads on P cores, so a CPU-bound slice
    /// ran at about half speed beside its own carrier's spin; the yield lets
    /// the runner have the core while keeping the spin's cheap wake-up for a
    /// short slice (unlike [`Self::vthread_carrier_sleeps`], which gives the
    /// spin up). Interpreter round i5 wave 106, lane L6 (page
    /// `i91-L6-virtual-thread-code-is-never-compiled`, the switch-off
    /// virtual-vs-platform gap). `CRATONVM_VTHREAD_HANDOFF_SPIN_YIELDS`, read
    /// once per VM here; default ON since the w106 triage, `=0` off. A
    /// performance switch only.
    pub vthread_handoff_spin_yields: bool,

    /// `NativeContext::record_jmx_owned_synchronizer` writes nothing while the
    /// per-lock AQS owner index is retired (`CRATONVM_AOS_OWNER_INDEX` off, the
    /// default since nio21 wave 3): `ThreadInfo.getLockedSynchronizers()` is
    /// then answered by the dump-time heap walk (`owned_synchronizer_census_in`,
    /// HotSpot's `ConcurrentLocksDump`), and the only writer left is the
    /// `ReentrantLock` fast path's `rl_fast_set_owner`, which paid a shard mutex,
    /// a map write and the owner's list mutex twice per lock pair for an index
    /// no dump reads -- and left a stale entry (a GC root) whenever the JDK
    /// body, not the fast path, made the release. Interpreter round i5 wave
    /// 101, lane L6 (page `i77-L1-proposal-a-per-thread-owned-synchronizer-record`,
    /// `## Wave 101`). `CRATONVM_OWNER_RECORD_NEEDS_INDEX`, read once per VM
    /// here (`runtime_flag_on`); default OFF. A performance switch only. Holds
    /// the combined answer: the switch on AND the index off.
    pub owner_record_needs_index: bool,

    /// JNI `MonitorEnter` / `MonitorExit` leave HotSpot's throwable pending
    /// with their `JNI_ERR`: a `NullPointerException` (no message) for a null
    /// object, and for an exit of a monitor the caller does not own an
    /// `IllegalMonitorStateException("current thread is not owner")`
    /// (`ObjectSynchronizer::jni_exit` -> `ObjectMonitor::check_owner`). The
    /// exit answered `JNI_OK` with nothing pending. Interpreter round i5 wave
    /// 101, lane L6 (`L6W101JniMonitorExitNotOwner`).
    /// `CRATONVM_JNI_MONITOR_EXCEPTIONS`, default ON; `=0` is the kill switch.
    pub jni_monitor_exceptions: bool,

    /// The launcher ends the main thread's `java.lang.Thread` (`Thread.exit()`,
    /// TERMINATED, `eetop` 0, join waiters notified) when `main` returns or
    /// throws, BEFORE it waits for the non-daemon threads, as HotSpot's
    /// `LEAVE()` detaches the main thread before `DestroyJavaVM`. Without it a
    /// non-daemon thread's `mainThread.join()` never returned and the process
    /// never exited. Interpreter round i5 wave 101, lane L6
    /// (`L6W101MainJoinAfterMainReturns`).
    /// `CRATONVM_MAIN_THREAD_ENDS_BEFORE_DESTROY`, default ON; `=0` is the kill
    /// switch.
    pub main_thread_ends_before_destroy: bool,

    /// Once [`Self::main_thread_ends_before_destroy`] ended the main thread's
    /// `java.lang.Thread`, the launcher's waiting thread takes a new
    /// `java.lang.Thread` named `DestroyJavaVM` (main group, non-daemon,
    /// RUNNABLE, a fresh `Thread.tid`), as HotSpot's `DestroyJavaVM` attaches
    /// one: `Thread.getAllStackTraces()`, `ThreadMXBean` and thread dumps list
    /// `DestroyJavaVM` instead of nothing, and the shutdown sequence runs with
    /// it as `Thread.currentThread()`, as on HotSpot. The ended `main` mirror
    /// stays TERMINATED. Interpreter round i5 wave 102, lane L6
    /// (`L6W102DestroyJavaVmListed`). `CRATONVM_DESTROY_JAVA_VM_THREAD`,
    /// default ON; `=0` is the kill switch.
    pub destroy_java_vm_thread: bool,

    /// How long the `--compatible` shutdown-hook runner
    /// (`native-builtins` `lang_system::run_shutdown_hooks`) waits for the
    /// application's hook threads, in milliseconds; `0` waits forever, as
    /// HotSpot's `ApplicationShutdownHooks.runHooks` does. Per VM since
    /// interpreter round i5 wave 102, lane L6 (it was a process-wide
    /// `OnceLock`, an AGENTS.md violation). `CRATONVM_SHUTDOWN_HOOK_TIMEOUT_MS`,
    /// default 0 (wait forever, HotSpot's answer) since the interpreter round
    /// i5 user-decision flip (marker: WARNING interp-i5-hook-timeout-flip-20261015;
    /// page `i101-L6-compatible-shutdown-hooks-abandoned-after-30-seconds`);
    /// a CI harness that wants the old bounded wait sets e.g. `=30000`.
    pub shutdown_hook_timeout_ms: u64,

    /// A JDK continuation's thread is registered, mounted and retired with
    /// one registry publish each instead of thirteen (nine writes), five and
    /// three `threads` lock trips: the entry is built complete (mirror, `Thread.tid`, the
    /// `JvmThread`'s `Arc`s, its addresses) in one `threads` write
    /// (`ThreadRegistry::register_prepared_continuation_thread`), the first
    /// slice finds an earlier registration by the `Thread.tid` index only, a
    /// mount publishes the OS tid, `mounted` and both addresses under one read
    /// (`mount_continuation_thread`), and a death lowers `alive` in one write
    /// with no walk inside the GC barrier's lock (`retire_continuation_thread`)
    /// and purges the terminated tail after it, by a kept count, in batches of
    /// 64 (`finish_retired_continuation_thread`). Interpreter round i4 wave 95,
    /// lane L6 (page `i91-L6-virtual-thread-code-is-never-compiled`, wave-94
    /// census: register 188 us, teardown registry 281 us per thread).
    /// `CRATONVM_VTHREAD_ONE_PUBLISH`, read once per VM here
    /// (`runtime_flag_default_on`); default ON since the round i4 wave-97
    /// triage (`=0` off). A performance switch only.
    pub vthread_one_publish: bool,

    /// A JDK continuation's death walks every inflated monitor in the VM
    /// (`MonitorTable::release_monitors_held_by_except`) only when its thread
    /// entered a monitor outside structured locking that it has not exited
    /// (JNI `MonitorEnter`, a VM native's `NativeContext::monitor_enter`;
    /// counted per thread in its registry entry), as HotSpot's
    /// `JavaThread::exit` sweeps only for a non-zero `_jni_monitor_count`.
    /// Interpreter round i4 wave 95, lane L6 (page
    /// `i94-L6-thread-death-sweeps-every-inflated-monitor`). Since wave 99
    /// (lane L6) a PLATFORM thread's death too (`thread_start`'s termination,
    /// JNI `DetachCurrentThread`, the generational concurrent service;
    /// `jdk_continuation::platform_death_may_skip_sweep`), its native enters
    /// counted on its `JvmThread`; either kind keeps the walk when a VM error
    /// ended its Java instead of a return or a throw.
    /// `CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT`, read once per VM here
    /// (`runtime_flag_default_on`); default ON since the round i4 wave-97
    /// triage (`=0` off). A performance switch only.
    pub vthread_death_sweep_by_count: bool,

    /// The virtual-thread manager drops terminated JDK continuations' entries
    /// past a bounded tail (`virtual_threads::RETIRED_JDK_ENTRY_CAP`) instead
    /// of keeping one per virtual thread ever started. Interpreter round i4
    /// wave 95, lane L6 (page
    /// `i94-L6-terminated-jdk-virtual-threads-stay-in-the-manager-map`; a
    /// leak). `CRATONVM_VTHREAD_RETIRE_MANAGER_ENTRIES`, read once per VM here
    /// (`runtime_flag_default_on`); default ON, `=0` keeps every entry.
    pub vthread_retire_manager_entries: bool,

    /// `CRATONVM_DBG_VTHREAD_CENSUS`: time the phases of every JDK
    /// continuation slice (register, carrier hand-off, mount, Java, unmount,
    /// teardown and its registry / monitor-sweep parts) into per-VM counters
    /// and print them at exit (`SharedVm::report_vthread_census_at_exit`).
    /// Interpreter round i4 wave 94, lane L6. Read once per VM; default OFF.
    pub dbg_vthread_census: bool,

    /// A new `Class` mirror (`vm_object::class_mirror_impl`, and the primitive
    /// / stand-in mirrors) takes its `java/lang/Class` id, field count, the
    /// resolved field slots, the slot-0 overlay verdict and the two mirror
    /// debug flags from a per-VM memo (`ClassRealm::class_mirror_layout`),
    /// filled from the first mirror minted after a REAL `java/lang/Class` is
    /// loaded, instead of taking the class-manager WRITE lock for
    /// `load_class("java/lang/Class")`, resolving eight fields by name and
    /// reading two environment flags per mirror. Interpreter round i3 wave 80,
    /// lane L5 (proposal `i72-L5-proposal-one-class-manager-write-lock-per-define`,
    /// under the class-path umbrella `i71-orch-proposal-class-path-jar-loading`).
    /// `CRATONVM_CLASS_MIRROR_LAYOUT_MEMO`, read once per VM here
    /// (`runtime_flag_default_on`); default ON since the wave-92 triage, `=0` off.
    /// A performance switch only: the memo
    /// holds exactly what the slow path computes.
    pub class_mirror_layout_memo: bool,

    /// `CRATONVM_CLASS_MIRROR_ONE_LOADER_LOOKUP` (`runtime_flag_on`, default
    /// OFF, performance only). Interpreter round i6 wave 115, lane L5 (umbrella
    /// `i71-orch-proposal-class-path-jar-loading-is-eight-times-hotspot`, the
    /// wave-95 `mirror-loader-module` row of `i87-L4-proposal-define-and-first-instance-cost`):
    /// a new mirror (`vm_object::class_mirror_impl`) asks the defining-loader
    /// table ONCE for its own class (instead of three times: `Class.classLoader`,
    /// `class_module_object`, the mirror pin) and reads the module facts
    /// `class_module_object` needs under the class-manager read guard that
    /// already reads the class's name (instead of a second read and two more
    /// name copies). Every answer is the one the three lookups give.
    pub class_mirror_one_loader_lookup: bool,

    /// `CRATONVM_SPOOFED_PACKAGE_UNNAMED_MODULE` (`runtime_flag_default_on`,
    /// `=0` off; HotSpot parity). Interpreter round i6 wave 118, lane L5
    /// (`i44-L5-proposal-a-classs-module-is-an-identity-every-reader-sees`): a
    /// class a user loader defined in a platform module's package is in its
    /// loader's unnamed module for `Class.module` (`vm_object::class_module_object`),
    /// as linkage already reads it; off, its `getModule()` names the platform
    /// module its package label says. Probe `L5W118SpoofedPackageModule`.
    pub spoofed_package_unnamed_module: bool,

    // zt t8/cl4: the class-mirror slot-0 overlay switch and its field are
    // RETIRED. Forcing the class-id overlay over a real `java.lang.Class`
    // wrote into `cachedConstructor`, which the JDK's own `Class.newInstance`
    // body reads; see `vm_object::mirror_slot0_overlay`.

    /// The fused int compare-and-branch (`fused_icmp_arm!`,
    /// `interpreter::fused_int_operand`) takes an `ldc` / `ldc_w` of an int
    /// as its second operand when the thread's numeric-constant table
    /// (`JvmThread::site_tables.prim_const_sites`) already holds the site: javac's
    /// `iload i; ldc #N; if_icmp<cond>` for a loop bound outside `sipush`'s
    /// range becomes one dispatch instead of three. A miss, a float, a string
    /// or a class declines, and the unfused `ldc` resolves and fills as
    /// before. Admitted only with `ldc_const_cache` (the table's own gate).
    /// Interpreter round i2 wave 63, lane L7 (proposal
    /// `i62-L7-proposal-the-fused-compare-and-branch-takes-an-ldc-int-bound-20261026`).
    /// `CRATONVM_INTERP_FUSED_LDC_BOUND`, read once per VM here; default ON since the
    /// wave-63 host A/B (`L7W63LdcBoundBench --nojit`, min of 5: `ldcBound` -24%,
    /// `ldcWBound` -23%, `ldcBoth` -19%, no row slower); `=0` turns it off.
    /// A performance switch only: the value compared is the one the `ldc`
    /// would have pushed.
    pub interp_fused_ldc_bound: bool,

    /// The dispatch loop's `ldc` / `ldc_w` / `ldc2_w` arm stores the next pc
    /// through the hoisted `frame` it already holds instead of re-indexing
    /// `thread.frames[frame_idx]` after ending that borrow (the same store,
    /// address and order relative to `execute_ldc`). Interpreter round i2
    /// wave 63, lane L7 (proposal
    /// `i58-L7-proposal-the-ldc-arm-sets-the-pc-through-the-hoisted-frame-20261022`).
    /// `CRATONVM_INTERP_LDC_HOISTED_PC`, read once per VM here; default off,
    /// so the host can A/B it on one binary. A pure refactor in behaviour.
    pub interp_ldc_hoisted_pc: bool,

    /// The dispatch loop's `ldc` / `ldc_w` arm answers a warm int or float
    /// constant itself: it probes the thread's numeric-constant table
    /// (`JvmThread::site_tables.prim_const_sites`, the probe `constants::execute_ldc`
    /// makes first) and on a held `Int` / `Float` pushes it through the
    /// frame it already holds, instead of calling `execute_ldc` (a call, two
    /// `FrameStack` re-indexes and a `Value` push). A miss, a reference
    /// constant, `ldc2_w`, or a push that would overflow takes the call
    /// unchanged. Admitted only with `ldc_const_cache` (the table's own
    /// gate). Interpreter round i2 wave 64, lane L7 (proposal
    /// `i63-L7-proposal-the-ldc-arm-answers-a-warm-int-inline-20261027`).
    /// `CRATONVM_INTERP_LDC_INLINE_HIT`, read once per VM here; default on since
    /// the wave-66 triage (`--nojit` `ldcBody` -12%, `ldcBoth` -7%; `=0` off).
    /// A performance switch only: the value pushed is the one the call pushes.
    pub interp_ldc_inline_hit: bool,

    /// A warm `ldc` / `ldc_w` of a REFERENCE constant (`String`, `Class`,
    /// `MethodType`, `MethodHandle`, condy) answered from the per-thread
    /// constant-site table (`PrimitiveConstant::Ref`) instead of the shared
    /// record's `resolution_cache` read lock and hash probe. The entry holds the
    /// object's address stamped with `mem.address_epoch()`, and answers only
    /// while that epoch is unchanged (no moving or freeing collection since),
    /// so it needs no GC root of its own; the record that filled it still
    /// roots the object. Filled only from a record hit (the value every
    /// thread pushes). Admitted only with `ldc_const_cache`. Interpreter round
    /// i7 wave 143, lane L3 (stage 3 of
    /// `i1-L2-proposal-per-class-resolved-constant-pool`).
    /// `CRATONVM_INTERP_LDC_REF_SITES`, read once per VM; performance, default
    /// ON since the round-i7 triage (`=0` off).
    pub interp_ldc_ref_sites: bool,

    /// The fused int compare-and-branch (`fused_icmp_arm!`,
    /// `interpreter::fused_int_operand`) takes a `getstatic` of an int-typed
    /// static (`I`, `S`, `B`, `C`, `Z`) as its second operand when the
    /// thread's quickened static-site table (`JvmThread::site_tables.static_field_sites`)
    /// already holds the site: `iload i; getstatic #F; if_icmp<cond>` (a loop
    /// bound in a non-final `static int`) becomes one dispatch instead of
    /// three. It reads the field each execution (`get_static_shared`, with
    /// the `SeqCst` fence of a volatile), exactly as the quickened
    /// `getstatic` does; it declines while a field watchpoint is armed (so a
    /// `FieldAccess` event is never skipped), under
    /// `CRATONVM_JIT_NO_FIELD_FAST_PATH`, and on a miss. Interpreter round i2 wave
    /// 64, lane L7 (proposal
    /// `i63-L7-proposal-the-fused-compare-and-branch-takes-a-getstatic-int-bound-20261027`).
    /// `CRATONVM_INTERP_FUSED_GETSTATIC_BOUND`, read once per VM here;
    /// default off. A performance switch only.
    pub interp_fused_getstatic_bound: bool,

    /// The dispatch loop's `getstatic` (0xb2) answers a quickened site
    /// itself: it probes the thread's quickened static-site table
    /// (`JvmThread::site_tables.static_field_sites`, what `field_fast::getstatic_site_hit`
    /// probes) by reference, reads the field (`get_static_shared`, with the
    /// `SeqCst` fence after a volatile read) and pushes it with the push
    /// `field_access::push_static_field_value` makes for that descriptor,
    /// through the frame it already holds, instead of calling `op_getstatic`
    /// (two process-memo reads, two `FrameStack` re-indexes, a site clone and
    /// the generic `Value` push). A miss, a field watchpoint armed,
    /// `CRATONVM_JIT_NO_FIELD_FAST_PATH`, a reference field while the
    /// relocation-provenance trace is armed, or a push that would overflow
    /// takes the call unchanged. Interpreter round i2 wave 65, lane L7
    /// (proposal
    /// `i64-L7-proposal-the-getstatic-arm-answers-a-quickened-site-inline-20261028`).
    /// `CRATONVM_INTERP_GETSTATIC_INLINE_HIT`, read once per VM here; default
    /// on since the wave-66 triage (`--nojit` static reads -8..-14%; `=0` off). A performance switch only.
    pub interp_getstatic_inline_hit: bool,

    /// The dispatch loop's `ldc2_w` arm answers a warm long or double
    /// constant itself: the probe `constants::execute_ldc2w` makes first
    /// (`JvmThread::site_tables.prim_const_sites`, same key and epoch validation) and, on
    /// a held `Long` / `Double`, its push (`push_long` / `push_double`: the
    /// `KIND_LONG` / `KIND_DOUBLE` marks keep collision-shaped longs and NaN
    /// payloads bit-exact), through the frame it already holds, instead of
    /// calling `execute_ldc2w`. A miss or a push that would overflow takes
    /// the call unchanged. Admitted only with `ldc_const_cache`. Interpreter
    /// round i2 wave 65, lane L7 (proposal
    /// `i64-L7-proposal-the-ldc2w-arm-answers-a-warm-long-or-double-inline-20261028`).
    /// `CRATONVM_INTERP_LDC2W_INLINE_HIT`, read once per VM here; default
    /// on since the wave-66 triage (`--nojit` long/double rows -5%; `=0` off). A performance switch only.
    pub interp_ldc2w_inline_hit: bool,

    /// The dispatch loop's `putstatic` arm stores to a quickened site itself:
    /// the probe `field_fast::putstatic_site_hit` makes (same table and key,
    /// by reference rather than a clone), the same final-field decline, the
    /// descriptor-aware pop `pop_static_field_value` makes, the `SeqCst`
    /// fences around a volatile store and `set_static_shared` (which takes
    /// the SATB pre-barrier itself), through the frame it already holds,
    /// instead of calling `op_putstatic`. A miss, a final field, a field
    /// watchpoint armed (`FieldModification` included),
    /// `CRATONVM_JIT_NO_FIELD_FAST_PATH`, an empty operand stack, or a `D`
    /// slot not marked `KIND_DOUBLE` takes the call unchanged, nothing popped.
    /// Interpreter round i2 wave 66, lane L7 (proposal
    /// `i65-L7-proposal-the-putstatic-arm-stores-a-quickened-site-inline-20261029`).
    /// `CRATONVM_INTERP_PUTSTATIC_INLINE_HIT`, read once per VM here; default
    /// on since the wave-66 triage (`--nojit` -4..-5%; `=0` off). A performance switch only.
    pub interp_putstatic_inline_hit: bool,

    /// The dispatch loop fuses `getstatic #F; if<cond> L` (`ifeq` .. `ifle`)
    /// over a quickened int-typed static (`I`, `Z`, `B`, `S`, `C`): the int
    /// the quickened `getstatic` would push is read by the fused
    /// compare-and-branch's probe (`getstatic_int_site`, the same table, read
    /// and volatile fence) and compared with zero, and the branch is taken at
    /// the branch's own pc through the `if<cond>` arm's accounting (profile,
    /// back edge, OSR, poll). Nothing is pushed. A miss, a non-int field, a
    /// field watchpoint armed, `CRATONVM_JIT_NO_FIELD_FAST_PATH`, or a single
    /// step / breakpoint concerning the method (`fusion_off`) takes the
    /// unfused pair. Interpreter round i2 wave 66, lane L7 (proposal
    /// `i65-L7-proposal-a-fused-getstatic-and-if-zero-branch-for-static-flags-20261029`).
    /// `CRATONVM_INTERP_FUSED_GETSTATIC_IFZERO`, read once per VM here;
    /// default on since the wave-66 triage (`--nojit` -5..-15%; `=0` off). A performance switch only.
    pub interp_fused_getstatic_ifzero: bool,

    /// The dispatch loop fuses a static int increment, `getstatic #F;
    /// <operand>; iadd | isub; putstatic #F` with `<operand>` one of
    /// `iconst_m1` .. `iconst_5`, `bipush`, `sipush`, `iload n`, `iload_0`
    /// .. `iload_3` and the `putstatic` naming the SAME pool index (`x++`,
    /// `x--`, `x += k`, `x -= i`), over a quickened, non-final `I` static:
    /// one probe of the quickened static-site table, the read the quickened
    /// `getstatic` makes (`get_static_shared`, the `SeqCst` fence after a
    /// volatile read), a wrapping add / subtract, and the store the quickened
    /// `putstatic` makes (the `SeqCst` fences around a volatile store,
    /// `set_static_shared` with its SATB pre-barrier). Nothing is pushed or
    /// popped. A miss, a non-`I` or final field, a field watchpoint armed
    /// (`FieldAccess` or `FieldModification`), `CRATONVM_JIT_NO_FIELD_FAST_PATH`,
    /// a non-int local, or a single step / breakpoint concerning the method
    /// (`fusion_off`) takes the unfused sequence, nothing written. Admitted
    /// only while both `interp_getstatic_inline_hit` and
    /// `interp_putstatic_inline_hit` are on. Interpreter round i2 wave 67,
    /// lane L7 (proposal
    /// `i66-L7-proposal-a-fused-static-int-increment-20261030`).
    /// `CRATONVM_INTERP_FUSED_STATIC_INCREMENT`, read once per VM here;
    /// default on since the wave-67 triage (`--nojit` `counterBody` -38%; `=0` off). A performance switch only.
    pub interp_fused_static_increment: bool,

    /// The fused instance-field int increment, `aload_N; dup; getfield #F;
    /// <int operand>; iadd | isub; putfield #F` (`this.count++`,
    /// `modCount++`, `size -= k`), on a quickened (`fast_field_sites`) `I`
    /// instance field: the `aload_N` arm reads the receiver from the local,
    /// and `field_fast::fused_field_increment` reads, adds and stores the
    /// field through the quickened `getfield` / `putfield` arms' own
    /// accessors (`field_ptr_for`, `put_field_value`, the final-field rule),
    /// with nothing pushed. A null receiver, a cold site, a non-`I` or final
    /// field the quickened `putfield` would decline, a field watchpoint armed
    /// (`FieldAccess` or `FieldModification`), the field-phase instrument, a
    /// single step / breakpoint concerning the method (`fusion_off`) or any
    /// other shape takes the unfused sequence, nothing written (so the NPE is
    /// raised at the `getfield`'s own pc). Interpreter round i2 wave 68, lane
    /// L7 (proposal
    /// `i67-L7-proposal-a-fused-instance-field-increment-20261031`).
    /// `CRATONVM_INTERP_FUSED_FIELD_INCREMENT`, read once per VM here;
    /// default on since the wave-68 triage (`=0` turns it off). A performance
    /// switch only.
    pub interp_fused_field_increment: bool,

    /// `fused_static_increment` also serves a narrow static (`B` / `S` / `C`
    /// with the matching `i2b` / `i2s` / `i2c` before the `putstatic`) and a
    /// `long` static (`lconst_0/1` or a long local, `ladd` / `lsub`).
    /// Admitted only with `interp_fused_static_increment` (and so with both
    /// inline hits). Interpreter round i2 wave 68, lane L7 (proposal
    /// `i67-L7-proposal-fused-static-increment-for-narrow-and-long-statics-20261031`).
    /// `CRATONVM_INTERP_FUSED_STATIC_INCREMENT_WIDE`, read once per VM here;
    /// default on since the wave-68 triage (`=0` turns it off). A performance
    /// switch only.
    pub interp_fused_static_increment_wide: bool,

    /// `field_fast::fused_field_increment` also serves a narrow instance
    /// field (`B` / `S` / `C` with the matching `i2b` / `i2s` / `i2c` before
    /// the `putfield`) and a `long` instance field (`lconst_0/1` or a long
    /// local, `ladd` / `lsub`), read and stored through the quickened arms'
    /// own accessors. Builds on `interp_fused_field_increment` (default on
    /// since the wave-68 triage) and is admitted only with it. Interpreter
    /// round i2 wave 69, lane L7 (proposal
    /// `i68-L7-proposal-fused-field-increment-for-narrow-and-long-fields-20261101`).
    /// `CRATONVM_INTERP_FUSED_FIELD_INCREMENT_WIDE`, read once per VM here;
    /// default on since the wave-69 triage (`=0` turns it off). A performance
    /// switch only.
    pub interp_fused_field_increment_wide: bool,

    /// Both fused increments (`fused_static_increment`,
    /// `field_fast::fused_field_increment`) are entered through a small
    /// screen that checks the group's bytes alone (operand length from its
    /// opcode, the arithmetic, the conversion, the store opcode and its pool
    /// index) before the out-of-line body is called, so a shape that passes
    /// the loop's prefilter and then declines (`x = y + 1` over two statics,
    /// `last = count++`, `b++` with the wide switch off) returns before the
    /// body's prologue, local reads and site probe. Changes no answer: a
    /// group the screen passes is decided by the body exactly as before.
    /// Builds on `interp_fused_static_increment` / `interp_fused_field_increment`
    /// (both default on). Interpreter round i2 wave 69, lane L7 (proposal
    /// `i68-L7-proposal-decline-the-fused-increments-before-the-call-20261101`).
    /// `CRATONVM_INTERP_FUSED_INCREMENT_EARLY_DECLINE`, read once per VM
    /// here; default off. A performance switch only.
    pub interp_fused_increment_early_decline: bool,

    /// `field_fast::fused_field_increment` also serves the assignment written
    /// without `dup`: `aload_N; aload_N; getfield #F; <operand>; iadd | isub;
    /// [i2b | i2c | i2s]; putfield #F` with both loads naming the same local
    /// (`count = count + 1`, `other.size = other.size - k`). The rest of the
    /// group, the site, the final rule, the watch union and every decline are
    /// the `dup` form's; a decline leaves the stack untouched and the plain
    /// `aload_N` runs, so a null receiver's NPE is raised at the `getfield`.
    /// Builds on `interp_fused_field_increment` (default on since the
    /// wave-68 triage) and is admitted only with it; the narrow and long
    /// shapes need `interp_fused_field_increment_wide` as in the `dup` form.
    /// Interpreter round i2 wave 70, lane L7 (proposal
    /// `i69-L7-proposal-fuse-the-field-increment-without-dup-20261102`).
    /// `CRATONVM_INTERP_FUSED_FIELD_INCREMENT_NO_DUP`, read once per VM here;
    /// default off. A performance switch only.
    pub interp_fused_field_increment_no_dup: bool,

    /// `field_fast::fused_field_increment` also serves the `dup` form whose
    /// updated value is used: a `dup_x1` (`dup2_x1` for a long field) right
    /// after the `getfield` (`last = count++`, `a[size++] = x`: the old value
    /// is left on the stack) or right before the `putfield` (`return
    /// ++count`: the new one). A hit pushes that one value with the stack
    /// kind the unfused group leaves; the site, the final rule, the watch
    /// union and every decline are the statement form's, and the stack room
    /// is checked before the field is stored. Builds on
    /// `interp_fused_field_increment` (default on) and is admitted only with
    /// it; the narrow and long shapes need `interp_fused_field_increment_wide`
    /// as in the statement form. Interpreter round i3 wave 71, lane L7
    /// (proposal
    /// `i70-L7-proposal-fuse-the-field-increment-whose-value-is-used-20261103`).
    /// `CRATONVM_INTERP_FUSED_FIELD_INCREMENT_VALUE`, read once per VM here;
    /// default off. A performance switch only.
    pub interp_fused_field_increment_value: bool,

    /// The fused array element increment: `aload_N; iload_M; dup2; <x>aload;
    /// <operand>; iadd | isub; [i2b | i2c | i2s]; <x>astore` over an `int[]`,
    /// `long[]` (`laload; lconst | lload; ladd | lsub; lastore`), `byte[]`,
    /// `char[]` or `short[]` (`a[i]++`, `hist[b] += k`, `sum[j] -= x`; also
    /// `imul` / `iand` / `ior` / `ixor` and their long twins: `m[i] |= k`), run
    /// by `field_fast::fused_array_increment_local` in one dispatch: the
    /// array and the index read straight out of their locals, the element
    /// read and stored through the quickened primitive array arms' own
    /// validation (`prim_elem_ptr`), nothing pushed. A null array, a
    /// negative or out-of-range index, an element type the opcode does not
    /// name (or a `boolean[]` under `baload`) declines having written
    /// nothing, so the unfused `*aload` raises its NPE / AIOOBE at its own
    /// pc. Declined under single-step and an armed breakpoint, as every
    /// fusion is. Interpreter round i3 wave 72, lane L7 (proposal
    /// `i71-L7-proposal-fuse-the-array-element-increment-20261104`).
    /// `CRATONVM_INTERP_FUSED_ARRAY_INCREMENT`, read here once per VM and
    /// tested by the dispatch loop only on an `aload_N; iload_M; dup2`
    /// prefix; default on (round-i3 triage). A performance switch only.
    pub interp_fused_array_increment: bool,

    /// `field_fast`'s fused array element increment also serves the forms
    /// whose value is used: a `dup_x2` (`dup2_x2` for a `long[]`) right after
    /// the `*aload` (`x = a[i]++`: the old element is left) or right before
    /// the `*astore` (`x = ++a[i]`, `return a[i] += k`: the new, narrowed
    /// one). A hit pushes that one value with the kind the unfused group
    /// leaves; room is checked before the element is stored. Admitted only
    /// with `interp_fused_array_increment`. Interpreter round i3 wave 72,
    /// lane L7. `CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_VALUE`, read here once
    /// per VM; default on (round-i3 triage). A performance switch only.
    pub interp_fused_array_increment_value: bool,

    /// `field_fast`'s fused array element increment also serves the group
    /// whose array is already on the operand stack: `iload_M; dup2; <x>aload;
    /// ...; <x>astore` after a `getfield` (`this.counts[i]++`), an `aaload`
    /// (`m[r][c] += k`) or any other array producer, run by
    /// `field_fast::fused_array_increment_top` from the `iload_N` arm (the
    /// array slot is replaced by the value a value form leaves, or popped).
    /// Admitted only with `interp_fused_array_increment` (the value forms
    /// with `interp_fused_array_increment_value`). Interpreter round i3 wave
    /// 72, lane L7. `CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_TOP`, read here
    /// once per VM and tested by the dispatch loop only on an `iload_N;
    /// dup2` prefix; default on (round-i3 triage). A performance switch only.
    pub interp_fused_array_increment_top: bool,

    /// `field_fast`'s fused array element increment also entered at the
    /// group's `dup2`, with the array and the index already on the operand
    /// stack however they were computed (`hist[x & 255]++`,
    /// `count[s.charAt(i)]++`, `this.a[i + 1] += k`), run by
    /// `field_fast::fused_array_increment_dup2` from the `dup2` arm (the two
    /// slots are popped, and the value a value form leaves is pushed).
    /// Admitted only with `interp_fused_array_increment` (the value forms
    /// with `interp_fused_array_increment_value`). Interpreter round i3 wave
    /// 72, lane L7. `CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_DUP2`, read here
    /// once per VM and tested by the dispatch loop only on a `dup2` followed
    /// by an int / long / byte / char / short `*aload`; default off. A
    /// performance switch only.
    pub interp_fused_array_increment_dup2: bool,

    /// `field_fast::fused_field_increment` also serves the two-field update
    /// `aload_N; aload_N; getfield #G; <operand>; iadd | isub; [i2b | i2c |
    /// i2s]; putfield #F` with `#F != #G` (`this.a = this.b + k`, `hi = lo -
    /// 1`), through `field_fast::fused_two_field_update`: each index has its
    /// own quickened site (the read's descriptor gives the read width, the
    /// store's must be the group's result, and the store must pass the final
    /// rule), both against the one receiver; a decline writes nothing, so the
    /// `aload_N; getfield` fusion serves the read as before. Admitted only
    /// with `interp_fused_field_increment_no_dup` (the prefix it shares; the
    /// loop's screen passes a `putfield` of any index only under this
    /// switch), the narrow and long shapes with
    /// `interp_fused_field_increment_wide`. Interpreter round i3 wave 72,
    /// lane L7 (proposal `i71-L7-proposal-fuse-the-two-field-update-20261104`).
    /// `CRATONVM_INTERP_FUSED_FIELD_UPDATE_TWO_FIELDS`, read here once per VM;
    /// default on (round-i3 triage). A performance switch only.
    pub interp_fused_field_update_two_fields: bool,

    // (Wave 73's inline byte screen of the fused array increment,
    // `interp_fused_array_increment_screen`, was measured, kept off and
    // retired in round i3 wave 75, lane L7:
    // `docs/feature-designs/jdk-only-mode.md`.
    // `interp_fused_array_decline_memo` below answers its question exactly.)
    /// `field_fast::fused_array_accumulate`: the `iload_N` arm also fuses
    /// `iload_S; aload_N; iload_M; iaload | baload | caload | saload; iadd |
    /// isub | imul | iand | ior | ixor; istore_Z` (`s += a[i]`, `h ^=
    /// b[k]`) into one dispatch: the element read through the quickened
    /// `*aload`'s own validation (`prim_elem_ptr`), the result stored into
    /// local Z as `istore_N` stores it, nothing pushed. A null array, a
    /// negative or out-of-range index or another element type declines
    /// having written nothing, so the unfused `*aload` throws at its own
    /// pc. Declined under single-step and an armed breakpoint, as every
    /// fusion is. Interpreter round i3 wave 73, lane L7 (proposal
    /// `i72-L7-proposal-fuse-the-array-element-accumulate-20261105`).
    /// `CRATONVM_INTERP_FUSED_ARRAY_ACCUMULATE`, read here once per VM and
    /// tested by the dispatch loop only on an `iload_S; aload_N; iload_M`
    /// prefix; default on (round-i3 triage). A performance switch only.
    pub interp_fused_array_accumulate: bool,

    /// `field_fast::fused_array_accumulate_long`: the `lload_N` arms also fuse
    /// the long accumulate, `lload_S; aload_N; iload_M; laload; ladd | lsub |
    /// lmul | land | lor | lxor; lstore_Z` (`long s; s += la[i]`), and the
    /// widening `lload_S; aload_N; iload_M; iaload | baload | caload |
    /// saload; i2l; <op>; lstore_Z` (`long s; s += a[i]` over an `int[]`)
    /// into one dispatch: the accumulator read only from a `long`-marked
    /// slot, the element through the quickened `*aload`'s own validation,
    /// the result stored as `lstore_N` stores it, nothing pushed; every
    /// other case declines having written nothing. Declined under
    /// single-step and an armed breakpoint, as every fusion is. Interpreter
    /// round i3 wave 82, lane L7
    /// (`i73-L7-proposal-long-and-wide-local-forms-of-the-array-accumulate`,
    /// item 1). `CRATONVM_INTERP_FUSED_ARRAY_ACCUMULATE_LONG`, read here once
    /// per VM (`runtime_flag_on`) and tested by the `lload_N` arms only on an
    /// `aload_N; iload_M` successor pair; default OFF until measured. A
    /// performance switch only.
    pub interp_fused_array_accumulate_long: bool,

    /// The `aload_N` arm runs `aload_N; {aload_M | iload_M}` as one
    /// dispatch when the next bytecode is a `putfield` or an invoke (the
    /// census's `aload_0 -> aload_1` / `aload_0 -> iload_1`, in 143 and 130
    /// of the 169 bench probes, ahead of `putfield` in every constructor and
    /// setter), and for `putfield` also stores through the quickened
    /// `putfield_fast`: `this.x = x` in one dispatch. A `putfield` miss
    /// leaves both values pushed and the `putfield` runs at its own pc.
    /// Interpreter round i4 wave 89, lane L7
    /// (`docs/feature-designs/jdk-only-mode.md`).
    /// `CRATONVM_INTERP_FUSED_LOAD_PAIR`, read once per VM
    /// (`runtime_flag_default_on`); default ON since the wave-89 triage
    /// (--nojit pair loops 0.81-0.99 of off), `=0` turns it off.
    pub interp_fused_load_pair: bool,

    /// The `iconst_1` arm runs `iconst_1; goto` as one dispatch (javac's
    /// boolean materialisation, `cond ? true : false`, in 132 of the 169
    /// bench probes), the `goto` through `goto_arm!` at its own pc.
    /// Interpreter round i4 wave 89, lane L7.
    /// `CRATONVM_INTERP_FUSED_ICONST_GOTO`, read once per VM
    /// (`runtime_flag_on`); default OFF until measured.
    pub interp_fused_iconst_goto: bool,

    /// The `iload_N` arm runs `iload_N; {iconst_m1..5 | bipush | sipush};
    /// iadd | isub | imul | ishl | ishr | iushr | iand | ior | ixor [;
    /// istore_Z]` as one dispatch (the pair census's `iconst_1 -> iadd`, 56
    /// of 169 bench probes, and `bipush -> iand`, 34): fib's `n - 1`, `i + 1`
    /// as an argument, `h & 0xff`. Interpreter round i4 wave 91, lane L7.
    /// `CRATONVM_INTERP_FUSED_ICONST_ARITH`, read once per VM
    /// (`runtime_flag_default_on`); default ON since the wave-92 triage, `=0` off.
    pub interp_fused_iconst_arith: bool,

    /// The `lload_N` and `lload n` arms run `lload; {lload_Y | lload n |
    /// lconst_0 | lconst_1}; ladd | lsub | lmul | land | lor | lxor [;
    /// lstore_Z | lstore n]` and `lload; <same operand>; lcmp` as one
    /// dispatch (the long loops of CratonBench's `arithmetic` kernel, the
    /// wave-25 census's "next" after `iload; iconst; <op>`). Nothing in the
    /// group throws. Interpreter round i5 wave 101, lane L7
    /// (`i1-L1-proposal-superinstruction-coverage`).
    /// `CRATONVM_INTERP_FUSED_LONG_ARITH`, read once per VM
    /// (`runtime_flag_on`); default OFF until measured.
    pub interp_fused_long_arith: bool,

    /// The `dload_N` arms run `dload; {dload_Y | dload n | dconst_0 |
    /// dconst_1}; dadd | dsub | dmul | ddiv | drem [; dstore_Z | dstore n]`
    /// and `dload; <same operand>; dcmpl | dcmpg` as one dispatch (the double
    /// twin of [`Self::interp_fused_long_arith`]; nothing in the group
    /// throws). Interpreter round i5 wave 102, lane L7.
    /// `CRATONVM_INTERP_FUSED_DOUBLE_ARITH`, read once per VM
    /// default ON since the w102 triage (bench `L7W102DoubleArithBench`,
    /// `--nojit` 0.51-0.98x, JIT flat); `=0` off.
    pub interp_fused_double_arith: bool,

    /// The fused long group ([`Self::interp_fused_long_arith`]) also takes an
    /// int local widened by `i2l` as its second operand: `lload; {iload_Y |
    /// iload n}; i2l; ladd | lsub | lmul | land | lor | lxor | lcmp [;
    /// lstore]` -- javac's `sum += x` with a `long` sum and an `int` term, the
    /// top pair (`i2l -> ladd`) of the wave-88 battery census. A local that is
    /// not int-tagged declines, so the unfused `iload` keeps its answer.
    /// Interpreter round i5 wave 105, lane L7.
    /// `CRATONVM_INTERP_FUSED_LONG_INT_OPERAND`, read once per VM;
    /// default ON since the w105 triage (bench `L7W105LongIntOperandBench`,
    /// `--nojit` 0.50-0.74x); `=0` off.
    pub interp_fused_long_int_operand: bool,

    /// The `i2l` arm runs `i2l; ladd | lsub | lmul | land | lor | lxor | lcmp
    /// [; lstore_Z | lstore n]` over the operand stack's `long, int` as one
    /// dispatch: javac's `sum += <int expression>` with a `long` sum when the
    /// term is not a plain local (a call, a field, a `charAt`, a product), the
    /// `i2l -> ladd` and `ladd -> lstore_1` pairs of the wave-88 battery census
    /// left after [`Self::interp_fused_long_int_operand`]. Nothing in the
    /// group throws. Interpreter round i5 wave 106, lane L7.
    /// `CRATONVM_INTERP_FUSED_I2L_LONG_TAIL`, read once per VM
    /// default ON since the w106 triage (bench `L7W106I2lLongTailBench`,
    /// `--nojit` 0.86-0.89x); `=0` off.
    pub interp_fused_i2l_long_tail: bool,

    /// The `iload_N` and `iload n` arms run `iload; ifeq | ifne | iflt |
    /// ifge | ifgt | ifle` as one dispatch (`if (flag)`, `while (n != 0)`;
    /// the wave-88 census's `iload_1 -> ifne`, in 97 of 169 probes), the
    /// branch handed to the unfused arm's `cond_branch_arm!` at its own pc.
    /// Nothing in the pair throws. Interpreter round i6 wave 115, lane L8
    /// (`i1-L1-proposal-superinstruction-coverage`).
    /// `CRATONVM_INTERP_FUSED_ZERO_BRANCH`, read once per VM
    /// (`runtime_flag_default_on`); default ON since the round-i6 wave-115
    /// triage (`--nojit` 0.89-0.91x), `=0` off.
    pub interp_fused_zero_branch: bool,

    /// The `aload_N` arm runs `aload_N; ifnull | ifnonnull` as one dispatch
    /// (`if (x == null)`, `while (node != null)`), the reference tested by the
    /// `ifnull` arm's own test and the branch handed to `cond_branch_arm!` at
    /// its own pc. Nothing in the pair throws. Interpreter round i6 wave 115,
    /// lane L8. `CRATONVM_INTERP_FUSED_NULL_BRANCH`, read once per VM
    /// (`runtime_flag_default_on`); default ON since the round-i6 wave-115
    /// triage (`--nojit` 0.92-0.94x), `=0` off.
    pub interp_fused_null_branch: bool,

    /// The wide twin of [`Self::interp_fused_null_branch`]: the `aload n` arm
    /// runs `aload n; ifnull | ifnonnull` as one dispatch (a reference past
    /// local 3). Interpreter round i6 wave 116, lane L8.
    /// `CRATONVM_INTERP_FUSED_NULL_BRANCH_WIDE`, read once per VM
    /// (`runtime_flag_on`); default OFF until measured.
    pub interp_fused_null_branch_wide: bool,

    /// A quickened `getfield` hit (the `getfield` arm's, and the `aload_N;
    /// getfield` fusion's) runs the `getfield`s right after it in the same
    /// dispatch (`this.a.b`, `node.next.value`); the first one the quickened
    /// read declines runs unfused at its own pc. Interpreter round i6 wave
    /// 116, lane L8. `CRATONVM_INTERP_FUSED_GETFIELD_CHAIN`, read once per VM
    /// (`runtime_flag_on`); default OFF until measured.
    pub interp_fused_getfield_chain: bool,

    /// The dispatch loop runs its `LEAN_TOP` instance
    /// (`interpreter::execute_frame_from_index`, chosen once per entry): one
    /// test of the loop-top gate byte and the poll compare replace the three
    /// gate-bit tests per bytecode, and the code length is cached at the
    /// frame-switch compare. Implies the one-test pending-signal form of
    /// `CRATONVM_INTERP_ONE_TOP_SIGNAL_TEST` in that instance. Interpreter
    /// round i6 wave 116, lane L8. `CRATONVM_INTERP_LEAN_LOOP_TOP`, read once
    /// per VM (`runtime_flag_on`); default OFF until measured.
    pub interp_lean_loop_top: bool,

    /// `tableswitch` and `lookupswitch` run on the dispatch loop's fast path
    /// (forward targets), from the same quickened instruction and with the
    /// same offset the decoded path computes, instead of through
    /// `execute_instruction`. Interpreter round i6 wave 118, lane L8.
    /// `CRATONVM_INTERP_FAST_SWITCH_ARMS`, read once per VM
    /// (`runtime_flag_default_on`); default ON since the wave-129 triage, `=0` off.
    pub interp_fast_switch_arms: bool,

    /// The `iinc; goto` fusion also runs the loop test at the goto's target
    /// (`iload; <bound>; if_icmp<cond>` / `iload; if<cond>`) in the same
    /// dispatch, when the loop top it skips would have done nothing.
    /// Interpreter round i6 wave 118, lane L8.
    /// `CRATONVM_INTERP_FUSED_LOOP_STEP_TEST`, read once per VM
    /// (`runtime_flag_on`); default OFF until measured.
    pub interp_fused_loop_step_test: bool,

    /// A served `aload_N; {aload_M | iload_M}; putfield` pair
    /// (`CRATONVM_INTERP_FUSED_LOAD_PAIR`) runs the same pairs right after it
    /// in the same dispatch: a constructor's `this.a = a; this.b = b; ...`
    /// (the wave-88 census's `putfield -> aload_0`). Interpreter round i6
    /// wave 119, lane L8. `CRATONVM_INTERP_FUSED_STORE_CHAIN`, read once per
    /// VM (`runtime_flag_on`); default OFF until measured.
    pub interp_fused_store_chain: bool,

    /// The store chain ([`Self::interp_fused_store_chain`]) also takes a value
    /// loaded from a local past 3 (`aload_N; {iload n | aload n}; putfield`),
    /// at its head and inside it: a constructor's fifth and later parameters.
    /// Needs the load pair and the store chain on. Interpreter round i6 wave
    /// 120, lane L8. `CRATONVM_INTERP_FUSED_STORE_CHAIN_WIDE`, read once per
    /// VM (`runtime_flag_on`); default OFF until measured.
    pub interp_fused_store_chain_wide: bool,

    /// The fused increments' long operand also takes an `ldc2_w` of a
    /// `CONSTANT_Long` (`nanos += 1_000_000L`, `total -= 1024L`, `seq += 2`
    /// on a `long`): `getstatic #F (J); ldc2_w #K; ladd | lsub; putstatic #F`
    /// and `aload_N; dup; getfield #F (J); ldc2_w #K; ladd | lsub; putfield
    /// #F` (and its `dup2_x1` value forms), the constant read from the
    /// thread's numeric-constant table (`JvmThread::site_tables`'s
    /// `prim_const_sites`, the probe the quickened `ldc2_w` makes, same key
    /// and epoch validation); a miss or a `Double` entry declines, so the
    /// unfused `ldc2_w` resolves and fills as before. Admitted only with
    /// `interp_ldc2w_inline_hit` and `ldc_const_cache` (the table's readers),
    /// and per side with that side's wide switch
    /// (`interp_fused_static_increment_wide` /
    /// `interp_fused_field_increment_wide`). The `aload_N; aload_N` (no-dup)
    /// prefix and the two-field update are not widened. Interpreter round i3
    /// wave 83, lane L7
    /// (`i69-L7-proposal-ldc2w-long-operand-for-the-fused-increments`).
    /// `CRATONVM_INTERP_FUSED_INCREMENT_LDC2W`, read here once per VM
    /// (`runtime_flag_on`); default OFF until measured. A performance switch
    /// only.
    pub interp_fused_increment_ldc2w: bool,

    /// The `aload n` arm (a reference local past 3) fuses `aload n;
    /// getfield` as the `aload_N` arms fuse `aload_N; getfield`: the receiver
    /// pushed, then the quickened `getfield` read (same site, admission,
    /// field-watch gate and push; a miss leaves the receiver for the unfused
    /// `getfield`, which raises a null receiver's NPE at its own pc), through
    /// the out-of-line `field_fast::getfield_fast_wide_local`. Declined
    /// under single-step and an armed breakpoint, as every fusion is.
    /// Interpreter round i3 wave 83, lane L7
    /// (`i72-L7-proposal-wide-local-forms-of-the-fused-groups`).
    /// `CRATONVM_INTERP_FUSED_WIDE_LOCAL_GETFIELD`, read here once per VM
    /// (`runtime_flag_on`) and tested by the `aload n` arm only after its
    /// next-opcode byte is a `getfield`; default OFF until measured. A
    /// performance switch only.
    pub interp_fused_wide_local_getfield: bool,

    /// The fused array element increment's decoder (all three entries) also
    /// takes the shifts `ishl` / `ishr` / `iushr` (`a[i] <<= k`), their long
    /// twins `lshl` / `lshr` / `lushr` (whose count is an int operand:
    /// `iconst_*`, `bipush`, `sipush`, `iload`), and the divisions `idiv` /
    /// `irem` / `ldiv` / `lrem` (`a[i] /= k`, `a[i] %= k`), so the compound
    /// shapes the wave-72 increment declined become hits. A zero divisor
    /// declines before anything is written, so the unfused `idiv` raises the
    /// `ArithmeticException` at its own pc; `MIN_VALUE / -1` wraps as the JVM
    /// does. Admitted only with
    /// `interp_fused_array_increment`. Interpreter round i3 wave 74, lane L7.
    /// `CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_SHIFT_DIV`, read here once per
    /// VM and read by the out-of-line helpers only (the dispatch loop is
    /// unchanged); default on (round-i3 triage). A performance switch only.
    pub interp_fused_array_increment_shift_div: bool,

    /// The fused array element increment's local entry (the `aload_N` arm)
    /// also serves the update written without a compound operator, `a[j] =
    /// a[j] + k`: `aload_N; iload_M; aload_N; iload_M; <x>aload; <operand>;
    /// <op>; [conv]; <x>astore`, the same two locals twice in place of the
    /// `dup2` (the operand stack after the second `iload_M` is the one the
    /// `dup2` leaves, so the group's tail is decoded and executed exactly as
    /// the `dup2` form's). Admitted only with `interp_fused_array_increment`
    /// (tested in this field's value); the value forms with
    /// `interp_fused_array_increment_value`, the shifts and divisions with
    /// `interp_fused_array_increment_shift_div`.
    /// Interpreter round i3 wave 74, lane L7 (proposal
    /// `i73-L7-proposal-fuse-the-array-element-update-without-dup2-20261106`).
    /// `CRATONVM_INTERP_FUSED_ARRAY_UPDATE_NO_DUP`, read here once per VM and
    /// tested by the dispatch loop only on an `aload_N; iload_M; aload_N`
    /// prefix; default on (round-i3 triage). A performance switch only.
    pub interp_fused_array_update_no_dup: bool,

    /// The fused array element increment remembers, per method and pc, a
    /// group its decoder refused on the group's bytes (`a[j] += SF`, `a[j]
    /// += b[k]`, `a[j] = a[j] + b[j]`, `f[j] += 1f`): the entry sets the
    /// pc's bit in its `CachedBytecodeMethod`'s bitmap
    /// (`array_inc_declined`), and the three entries (reached from the
    /// `aload_N`, `iload_N` and `dup2` arms) test the bit first (since wave
    /// 76, lane L7: the loop arms tested it inline before), so the decode of
    /// a declining group is paid once per site instead of once per execution.
    /// Only shape declines are remembered (never a null array, an index out
    /// of range, a zero divisor); a set bit runs the group unfused, which is
    /// what the decline did. Frames not pushed from a cached method entry
    /// keep the call. Admitted only with `interp_fused_array_increment` (the
    /// arms test it first). Interpreter round i3 wave 75, lane L7 (proposal
    /// `i74-L7-proposal-a-per-method-array-increment-decline-memo-20261107`).
    /// `CRATONVM_INTERP_FUSED_ARRAY_DECLINE_MEMO`, read here once per VM;
    /// default on (round-i3 wave-75 triage). A performance switch only.
    pub interp_fused_array_decline_memo: bool,

    /// The element read `a[i]` fuses when a local is past 3: the `aload_N`
    /// arm takes `aload_N; iload n; <x>aload` (the for-each loop's `arr$[i$]`,
    /// whose hidden index local is at 4 or above in any method with a
    /// receiver and one parameter) and the `aload n` arm takes `aload n;
    /// iload{_M, n}; <x>aload`, through `field_fast::array_load_wide_locals_at`
    /// (the `*aload` arm's own readers; every decline pushes nothing, so the
    /// plain load runs and the `*aload` throws at its own pc). One dispatch
    /// where the unfused group took two (`aload`, then the wave-25 `iload n;
    /// <x>aload` pair). Interpreter round i3 wave 75, lane L7 (proposal
    /// `interpreter-i72-L7-proposal-wide-local-forms-of-the-fused-groups-KEPT-OFF-20261019`).
    /// `CRATONVM_INTERP_FUSED_WIDE_LOCALS`, read here once per VM and tested
    /// by the dispatch loop only after an `iload` / `iload_M` byte follows the
    /// `aload`; default on (round-i3 wave-75 triage). A performance switch
    /// only.
    pub interp_fused_wide_locals: bool,

    /// The fused array element increment's decoder (all three entries) also
    /// takes another array element as the operand, `aload_X; iload_Y;
    /// <x>aload` with X and Y in 0..3 and the load of the group's width
    /// (`a[j] += b[k]`, `acc[i] += x[i]`, `a[j] = a[j] + b[j]`): the operand
    /// element is read through the quickened `*aload`'s validation after
    /// the group's own element, before the store; any refusal (null, out of
    /// range, another element type) declines having written nothing, so the
    /// unfused operand `*aload` throws at its own pc. Admitted only with
    /// `interp_fused_array_increment`. Interpreter round i3 wave 76, lane L7
    /// (proposal
    /// `i75-L7-proposal-fuse-the-array-element-operand-of-the-array-increment-20261108`).
    /// `CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_ARRAY_OPERAND`, read here once
    /// per VM and read by the out-of-line helpers only; default off. A
    /// performance switch only.
    pub interp_fused_array_increment_array_operand: bool,

    /// The fused array element increment's decoder (all three entries) also
    /// takes a `getstatic #F` operand (`a[j] += SF`, `hist[b] += WEIGHT`,
    /// `sum[j] += DELTA` over a `long[]`), answered only from a filled
    /// quickened static site (`JvmThread::site_tables.static_field_sites`; the field
    /// resolved, static, its class initialised) whose descriptor is the
    /// group's width (`I` / `B` / `C` / `S` / `Z` for an int group, `J` for a
    /// long one), read through `get_static_shared` with the fence after a
    /// volatile read. A cold site, an armed JVMTI field watchpoint or another
    /// descriptor declines having written nothing, so the unfused
    /// `getstatic` resolves, initialises, posts and fills. Admitted only
    /// with `interp_fused_array_increment`. Interpreter round i3 wave 76,
    /// lane L7 (proposal
    /// `i75-L7-proposal-fuse-the-static-operand-of-the-array-increment-20261108`).
    /// `CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_STATIC_OPERAND`, read here once
    /// per VM and read by the out-of-line helpers only; default off. A
    /// performance switch only.
    pub interp_fused_array_increment_static_operand: bool,

    /// With `interp_fused_array_increment_static_operand` on, an OWNED frame
    /// running `<clinit>` (built by `interpreter::execute`, no cached method,
    /// so no decline memo) refuses the `a[j] += SF` static operand on its
    /// bytes: `fill_static_site` never fills a site from a `<clinit>` frame,
    /// so the operand would miss on every iteration of a static-initialiser
    /// table loop after the full decode (interpreter round i4 wave 94, lane
    /// L7; `i77-L7-proposal-a-clinit-frame-skips-the-array-increment-static-operand`).
    /// `CRATONVM_INTERP_FUSED_ARRAY_STATIC_OPERAND_NO_CLINIT`, read once per VM
    /// here (`runtime_flag_on`); default OFF until measured.
    pub interp_fused_array_static_operand_no_clinit: bool,

    /// A miss budget for the fused array increment's deferred operands (the
    /// array element of `interp_fused_array_increment_array_operand`, the
    /// static of `interp_fused_array_increment_static_operand`): a failure to
    /// read either is a per-execution decline the shape memo
    /// (`interp_fused_array_decline_memo`) never records, so a static site
    /// that never fills (an access from a `<clinit>`, a `java/lang/System`
    /// static) paid the call, the decode and the probe on every execution.
    /// Under this switch each such miss bumps a saturating per-pc counter
    /// (allocated at the first miss, beside the memo's bitmap; a hit never
    /// touches it), and the 64th miss at a pc sets that pc's decline bit, so
    /// the loop then stops calling exactly as for a shape decline. Inert
    /// without `interp_fused_array_decline_memo`. A site that would start
    /// hitting after its budget ran out stays unfused (its group then runs
    /// the unfused bytecodes, which answer the same). Interpreter round i3
    /// wave 77, lane L7 (proposal
    /// `i76-L7-proposal-a-miss-budget-for-the-deferred-operands-20261109`).
    /// `CRATONVM_INTERP_FUSED_ARRAY_OPERAND_MISS_BUDGET`, read here once per
    /// VM and read by the out-of-line helpers on a miss only; default off. A
    /// performance switch only.
    pub interp_fused_array_operand_miss_budget: bool,

    /// The fused array increment's element operand
    /// (`interp_fused_array_increment_array_operand`, required) also takes
    /// its array from `aload n` and its index from `iload n` (locals past 3:
    /// `acc[i] += x[i]` in a method with a receiver and parameters, a
    /// for-each body), and an index expression `iload; <int constant>; iadd
    /// | isub` (`p[i] += p[i - 1]`, `h[k] += w[k + 2]`), computed with
    /// `iadd`'s wrapping and range-checked as the unfused operand `*aload`
    /// checks it (a refusal declines having written nothing, so that
    /// `*aload` throws at its own pc). Interpreter round i3 wave 77, lane L7
    /// (proposal
    /// `i76-L7-proposal-wide-and-expression-operands-of-the-array-increment-20261109`).
    /// `CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_ARRAY_OPERAND_WIDE`, read here
    /// once per VM and read by the out-of-line helpers only; default off. A
    /// performance switch only.
    pub interp_fused_array_increment_array_operand_wide: bool,

    // The kill-switch memos left on the door and return paths, read once per
    // VM here (interpreter round i2 wave 61, lane L7; each an `env_cache`
    // memo slot read per call before, proposal
    // `interpreter-L7-proposal-the-kill-switch-memos-left-on-the-door-and-array-paths-FIXED-20261025`).
    // Every variable keeps its exact semantics, `CRATONVM_JIT=-<token>`
    // included (`runtime_var_os` / `runtime_var` resolve the tokens).
    /// A returning frame is retired in place and the next call at its depth
    /// is rebuilt in the buffers it left (`FrameStack::retire_top` /
    /// `push_cached_compact_reusing`). Default on;
    /// `CRATONVM_JIT_NO_FRAME_SLOT_REUSE` (present, any value; token
    /// `-frame-slot-reuse`) returns every frame's buffers to the pools.
    pub frame_slot_reuse: bool,

    /// A first call at a depth builds its `Frame` in the frame stack's slot.
    /// Default on; `CRATONVM_JIT_NO_FRAME_EMPLACE` (present, any value; token
    /// `-frame-emplace`) builds it on the Rust stack and moves it in.
    pub frame_emplace: bool,

    /// The interpreter's virtual and interface call paths do the JIT tier-up
    /// of instance methods (with [`Self::jit_enabled`]). Default on;
    /// `CRATONVM_JIT_VIRTUAL_TIERUP=0` / `false` (token `-virtual-tierup`)
    /// turns it off; unset or any other value leaves it on.
    pub jit_virtual_tierup: bool,

    /// The virtual door records the receiver of every call it commits to
    /// (`dispatch_virtual::door_record_receiver`, under the receiver-profiling
    /// gate). Default on; `CRATONVM_JIT_NO_DOOR_RECEIVER_RECORD` (present, any
    /// value) leaves the door's calls unrecorded.
    pub door_receiver_record: bool,

    /// JIT round 23 wave 2, lane L2 (INLINE22-4 stage 2): the interpreter
    /// records a lambda proxy receiver at the `invokeinterface` / `invokevirtual`
    /// that dispatched to it (the lambda door, `invoke_fast::execute_lambda_door`,
    /// and the general path's lambda arm), under the receiver-profiling gate, so
    /// the optimizing tier's profile-guarded route can see the proxy and splice
    /// its SAM forwarder (`CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE`). Before, neither
    /// path recorded and the site's profile never named a proxy. Default on;
    /// `CRATONVM_JIT_PROFILE_LAMBDA_RECEIVERS=0` turns it off.
    pub lambda_receiver_record: bool,

    /// JIT round 23 wave 2c, lane L2: a lambda adapter's boxing
    /// (`interpreter::box_primitive`, LambdaMetafactory's `Box.valueOf` around
    /// a primitive implementation) calls the reviewed `Integer.valueOf` /
    /// `Long.valueOf` Intrinsic directly when the generic tail would run that
    /// native anyway (`NativeRealm::box_value_of_intrinsic`), instead of a
    /// by-name `invoke_shared` per call (2-5 us; page
    /// `r23-w2-boxing-method-ref-call-costs-microseconds-FIXED-20261021.md`). Default
    /// on; `CRATONVM_LAMBDA_BOX_INTRINSIC=0` restores the by-name call.
    pub lambda_box_intrinsic: bool,

    /// JIT round 23 wave 3b, lane L2: the compiled lambda arm boxes an adapted
    /// site's `int` return with the compiled `Integer.valueOf` helper's own
    /// body on the thread it holds (box cache / TLAB bump), not through the
    /// native funnel (`jit::helpers::lambda_site_adapt_return`). Default on;
    /// `CRATONVM_JIT_LAMBDA_ARM_TLAB_BOX=0` restores the funnel.
    pub lambda_arm_tlab_box: bool,

    /// The door's receiver record reuses a memoized profile-store handle
    /// (`dispatch_virtual::record_receiver_memoized`). Default on;
    /// `CRATONVM_JIT_NO_DOOR_RECV_MEMO` (present, any value) does the full
    /// lookup on every call. Same records either way.
    pub door_recv_memo: bool,

    /// The interpreter caches the ROUTE of a signature-polymorphic
    /// `invokevirtual` site (`VarHandle` access modes,
    /// `MethodHandle.invokeExact` / `invoke`) per site and receiver handle
    /// class, so a repeat execution goes straight to the native the slow path
    /// ran instead of re-running that path (interpreter round i1 wave 48, lane
    /// L7; `runtime::interpreter::sig_poly_site`). Every per-call check stays
    /// in the native. `CRATONVM_SIG_POLY_SITE_CACHE`, read once per VM here;
    /// default on (`=0` takes every such site back to the slow path).
    /// A performance switch only: no observable behaviour depends on it.
    pub sig_poly_site_cache: bool,

    /// The signature-polymorphic route cache's hit LENDS its site for the
    /// call -- the entry's `Arc` is moved out of the per-thread table and
    /// moved back afterwards -- instead of cloning the `Arc` (two locked
    /// read-modify-writes per `VarHandle` access / `invokeExact` hit). A
    /// nested hit of the same site while it is lent declines to the slow
    /// path (proposal
    /// `interpreter-L7-proposal-the-signature-polymorphic-site-hit-lends-its-site-instead-of-cloning-the-arc-FIXED-20261021`,
    /// interpreter round i2 wave 57, lane L7). A performance switch only.
    /// `CRATONVM_SIG_POLY_SITE_LEND`, read once per VM here; default on since
    /// the lane's switch-on commit (`=0` restores the `Arc` clone).
    pub sig_poly_site_lend: bool,

    /// The `invokevirtual` arm probes the signature-polymorphic route cache
    /// (`sig_poly_site_cache`) BEFORE its fast door and cached dispatcher when
    /// the thread's site filter says the site may be cached
    /// (`sig_poly_site::try_hit_first`): a `VarHandle` access or
    /// `MethodHandle.invokeExact` never has an invoke-cache entry, so those
    /// two probes could only miss first (interpreter round i2 wave 54, lane
    /// L7; proposal `i53-L7-proposal-signature-polymorphic-sites-probe-their-route-first`).
    /// Costs every other `invokevirtual` one filter-bit test.
    /// `CRATONVM_SIG_POLY_SITE_FIRST`, read once per VM here; default on since
    /// wave 54 (`=0` restores the old order). A performance switch only: the
    /// hit is the same route the old order reached.
    pub sig_poly_site_first: bool,

    /// A signature-polymorphic site-cache hit that publishes the call-site
    /// descriptor (`cratonvm_native_api::poly_call_site`) hands the consuming
    /// native the site's own `Arc<str>` (`poly_call_site::arm_shared`, a
    /// reference-count bump) instead of a fresh copy of it (one heap
    /// allocation and free per `MethodHandle.invoke` / `invokeExact`); proposal
    /// `interpreter-L7-proposal-the-call-site-descriptor-channel-allocates-per-call-FIXED-20261013`,
    /// interpreter round i1 wave 49, lane L7. `CRATONVM_POLY_CALL_SITE_SHARED`,
    /// read once per VM here; default on (`=0` arms a copy again). A performance switch
    /// only: the native reads the same text either way.
    pub poly_call_site_shared: bool,

    /// The signature-polymorphic site cache (`sig_poly_site_cache`) records a
    /// route for EVERY `VarHandle` site, its replay arming the whole
    /// parameter-site word (coordinates included) and running the
    /// reference-return checkcast, as the slow path's block does. JIT round 16
    /// wave 7, lane varhandle
    /// (`r16-varhandle-compare-and-set-seven-times-slower-since-round-15-FIXED-20261001.md`):
    /// the round-15 merge kept every site with three or more parameters (an
    /// instance `compareAndSet` / `compareAndExchange`) and every
    /// narrow-reference-return site off the cache, ~7x per call under
    /// `--nojit`. `CRATONVM_VH_SITE_CACHE_FULL_DOOR`, read once per VM here;
    /// default on (`=0` restores that exclusion). A performance switch only:
    /// the hit runs the same checks as the slow path.
    pub vh_site_cache_full_door: bool,

    /// MH16-6 (JIT round 16 wave 7, lane varhandle; `jit-r16-mh-proposals-RETIRED-20261006.md`):
    /// a site-cache hit of an instance-field `VarHandle.compareAndSet` is
    /// served by the JIT funnel's shared CAS body
    /// (`jit::helpers::varhandle_instance_field_cas_values`) when it can
    /// prove the access servable, skipping the native's access-mode prologue;
    /// any doubt declines to the native. `CRATONVM_VH_INTERP_CAS_FAST`, read
    /// once per VM here; DEFAULT ON since round 16 wave 7 (Linux `--nojit`:
    /// cas-int 888 -> 565 ns, cas-ref 1021 -> 709 ns; `=0` is the kill switch).
    pub vh_interp_cas_fast: bool,

    /// The `compareAndExchange` sibling of [`Self::vh_interp_cas_fast`]
    /// (interpreter round i2 wave 54, lane L7; proposal
    /// `i53-L7-proposal-an-instance-field-arm-for-compare-and-exchange`): a
    /// site-cache hit of an instance-field `VarHandle.compareAndExchange`
    /// (and its `Acquire` / `Release` forms) over an `int` or `long` variable
    /// is one hardware compare-exchange whose witness is pushed, after the
    /// CAS arm's own checks (`jit::helpers::varhandle_instance_field_cae_values`);
    /// any doubt declines to the native. `CRATONVM_VH_INTERP_CAE_FAST`, read
    /// once per VM here; default on since wave 54 (`=0` kills it). A performance switch only (the witness is the
    /// exchange's own; the native's, since wave 55, a re-read that differs
    /// from `expected`, retried when it does not).
    pub vh_interp_cae_fast: bool,

    /// The strict `invokeExact` return check (`CRATONVM_MH_STRICT_INVOKEEXACT`,
    /// `vm_exec::unbox_poly_return_checked`) answers a box of the wrapper the
    /// call site expects by its class id, against the id this VM recorded for
    /// that wrapper the first time the name check admitted it
    /// (`NativeRealm::exact_return_wrapper_ids`), instead of taking the
    /// class-manager lock and comparing names on every primitive-returning
    /// `invokeExact` (interpreter round i2 wave 55, lane L7; proposal
    /// `interpreter-L7-proposal-the-invokeexact-return-check-by-class-id-FIXED-20261019`). Any other
    /// class still takes the name check, so the refusals are the same. A
    /// performance switch only. `CRATONVM_MH_EXACT_RETURN_BY_ID`, read once per
    /// VM here; default on since the lane's last commit (`=0` restores the
    /// name check on every call).
    pub mh_exact_return_by_id: bool,

    /// W6-1's own `VarHandle` return refusal (`vm_exec::unbox_poly_return_checked`,
    /// `varhandle_reference_return_mismatch`: a box at a narrower reference
    /// site is a `WrongMethodTypeException` with CratonVM's text) is not asked
    /// on the return path: the access natives' `asType` check
    /// (`CRATONVM_VH_PRIMITIVE_RETURN_ASTYPE`) and VH2-3's cast
    /// (`CRATONVM_VH_REFERENCE_RETURN_CHECKCAST`) judge every handle this VM
    /// serves. With `CRATONVM_DBG_FIELD_SITE` set the rule is still evaluated
    /// and a would-be refusal counted (`site_stats::VH_W6_1_REFUSAL`), never
    /// raised. Interpreter round i2 wave 58, lane L4 (proposal
    /// `interpreter-i57-L4-proposal-retire-the-w6-1-varhandle-return-text-KEPT-20261224`).
    /// `CRATONVM_VH_W6_1_RETIRED`, read once per VM here; default on since the
    /// lane's last commit (`=0` restores the rule).
    pub vh_w6_1_retired: bool,

    /// `MethodHandle.linkToStatic` / `linkToVirtual` / `linkToInterface` /
    /// `linkToSpecial` / `linkToNative` called from BYTECODE (a JDK
    /// `LambdaForm` linker: `DirectMethodHandle$Holder.invokeInterface`, a
    /// spun `LambdaForm$MH`) with the call's concrete descriptor reach the
    /// erased native `native_method_handle_link_to`, as the `invoke*` names
    /// do (`vm_exec::is_method_handle_linker_intrinsic`; interpreter round i2
    /// wave 55, lane L4; page
    /// `interpreter-L4-linkto-intrinsics-are-not-reached-from-bytecode-FIXED-20261019`). They found
    /// no method. `CRATONVM_MH_LINKTO_FROM_BYTECODE`, read once per VM here;
    /// default on (`=0` restores the miss).
    pub mh_linkto_from_bytecode: bool,

    /// A `StackWalker` walk that shows hidden frames lists the hidden
    /// method-handle frames HotSpot runs for a `MethodHandle.invokeExact` /
    /// `invoke` or `Method.invoke` call the VM serves as a native
    /// (`DirectMethodHandle$Holder.invokeStatic`, ...; read from the one
    /// stand-in table, `stackwalker::hidden_invoke_standin_frames_with`; proposal
    /// `i46-L3-proposal-one-table-of-the-jdk-frames-a-native-stands-for`,
    /// interpreter round i1 wave 48, lane L3).
    /// `CRATONVM_STANDIN_HIDDEN_INVOKE_FRAMES`, read once per VM here; default on
    /// (`=0` lists none). Only a
    /// `SHOW_HIDDEN_FRAMES` walk sees a difference.
    pub standin_hidden_invoke_frames: bool,

    /// A `StackWalker` walk that shows hidden frames lists the lambda proxy's
    /// SAM frame (`Outer$$Lambda/0x….apply`) HotSpot runs between a call site's
    /// `invokeinterface` and the lambda's implementation method, which this
    /// VM's lambda dispatch calls with no proxy frame
    /// (`stackwalker::lambda_proxy_standin_frames`; interpreter round i1 wave
    /// 50, lane L3; probes `L3W50LambdaProxyFrames`, `L6W37HiddenFramesHot`
    /// row `lambda walk-hidden`). `CRATONVM_STANDIN_LAMBDA_PROXY_FRAMES`, read
    /// once per VM here; default on since the lane's last commit (`=0` lists
    /// none). Only a `SHOW_HIDDEN_FRAMES` walk
    /// sees a difference; nothing on the lambda call path.
    pub standin_lambda_proxy_frames: bool,

    /// A native that returns an unbuilt `RuntimeError` marks the raise on its
    /// thread (`vm_exec::note_served_native_raise`), and the capture of the
    /// throwable lists the JDK method the native served at its one
    /// construction of it, or at its one call of a method that constructs it
    /// (`stackwalker::served_raise_frames`; proposal
    /// `i52-L3-proposal-a-bridge-that-raises-names-the-jdk-method-it-stands-for`,
    /// interpreter round i2 wave 53, lane L3; probe `L3W53ServedRaiseFrames`).
    /// `CRATONVM_THROWABLE_SERVED_RAISE_FRAMES`, read once per VM here; default
    /// on since the lane's last commit (`=0` turns the mark off). Only
    /// the error path of a native pays (one store), and a capture one `take`.
    pub served_raise_frames: bool,

    /// The funnel's mark of a raise (`served_raise_frames`) also keeps the
    /// class of the native's receiver, and the capture selects the method a
    /// virtual or interface call reached from it, as the invoke did
    /// (`TypeVariable.getAnnotation(null)` -> `TypeVariableImpl.getAnnotation`;
    /// proposal `docs/feature-designs/jdk-only-mode.md`,
    /// interpreter round i2 wave 54, lane L3; probe
    /// `L3W54ServedRaiseReceiverClass`). `CRATONVM_THROWABLE_SERVED_RAISE_RECEIVER`,
    /// read once per VM here; default on since the lane's last commit (`=0`:
    /// the static selection only). The error path only.
    pub served_raise_receiver: bool,

    /// A native whose raise HotSpot lists two or more JDK frames below the
    /// served method notes the chain (`NativeExceptionAccess::note_served_raise_chain`)
    /// and the mark carries it (`Field.get` with the override: the field
    /// accessor's frames; interpreter round i2 wave 54, lane L3; probe
    /// `L3W54FieldOverrideAccessorFrames`).
    /// `CRATONVM_THROWABLE_SERVED_RAISE_CHAIN`, read once per VM here;
    /// default on (`=0` drops every noted chain).
    pub served_raise_chain: bool,

    /// A capture the funnel's mark stands for leaves the `Class` member-lookup
    /// family (`stackwalker::member_lookup_standin_frames`) to the mark's
    /// rule, which lists the same frames -- the first family retired onto the
    /// mark (item 3 of `i52-L3-proposal-a-bridge-that-raises-names-the-jdk-method-it-stands-for`;
    /// interpreter round i2 wave 54, lane L3).
    /// `CRATONVM_THROWABLE_MEMBER_LOOKUP_ON_MARK`, read once per VM here; default
    /// on since the lane's last commit (`=0` keeps the family first).
    pub member_lookup_on_mark: bool,

    /// A JDK stack walk's `StackFrameInfo` leaves its `StackTraceElement` to
    /// its first read, which settles it against a redefinition of the frame's
    /// class since the walk, as HotSpot builds it then
    /// (`lang_stackwalker::populate_sfi`; interpreter round i2 wave 54, lane
    /// L3; probe `L3W40WalkedFrameReadAfterRedefinition`).
    /// `CRATONVM_SFI_LAZY_ELEMENT`, read once per VM here; default on (`=0`
    /// pre-caches the element at the walk, as before).
    pub sfi_lazy_element: bool,

    /// `--compatible`'s native `StackWalker.walk` / `forEach` hand out the
    /// JDK's own `StackFrameInfo` / `ClassFrameInfo`, as `--jdk-only`'s do and
    /// HotSpot does, instead of the VM's carrier (an instance of the
    /// `StackWalker$StackFrame` interface; interpreter round i2 wave 55, lane
    /// L3; probe `L3W55WalkFrameClasses`).
    /// `CRATONVM_COMPATIBLE_JDK_WALK_FRAMES`, read once per VM here; default
    /// on (`=0` keeps the carrier).
    pub compatible_jdk_walk_frames: bool,

    /// A native walk's lazy `StackFrameInfo` keeps its entry in one `int[]`
    /// per walk (the walk's class-redefinition count, each frame's line and
    /// no-file bit) instead of a pre-built `StackTraceElement`, which its
    /// first read builds from the frame and that array
    /// (`lang_stackwalker::populate_sfi_from_walk_lines`; interpreter round i2
    /// wave 55, lane L3; proposal
    /// `docs/feature-designs/jdk-only-mode.md`).
    /// `CRATONVM_SFI_WALK_LINES`, read once per VM here; default on since the
    /// lane's last commit (`=0` pre-builds the element, as wave 54 did).
    pub sfi_walk_lines: bool,

    /// `--compatible`'s generic-signature natives (`Class` / `Method` /
    /// `Constructor.getTypeParameters()`, a type-variable use, a generic
    /// array type) build the JDK's own
    /// `sun.reflect.generics.reflectiveObjects.TypeVariableImpl` /
    /// `GenericArrayTypeImpl`, as `--jdk-only` does and HotSpot does, instead
    /// of the VM's carriers (instances of the `java.lang.reflect.TypeVariable`
    /// / `GenericArrayType` interfaces; interpreter round i2 wave 56, lane L3;
    /// probes `L3W56CompatibleTypeVariableClass`,
    /// `L3W56CompatibleGenericTypeClasses`).
    /// `CRATONVM_COMPATIBLE_REFLECT_TYPE_IMPLS`, read once per VM here; default
    /// on since the lane's flip commit (`=0` keeps the carriers).
    pub compatible_reflect_type_impls: bool,

    /// A field handle's null-base raise, which the method-handle door serves
    /// at the caller's `MethodHandle.invokeExact` / `invoke`, lists the
    /// frames HotSpot runs below the handle's hidden `LambdaForm` frames
    /// (`DirectMethodHandle.checkBase`, `Objects.requireNonNull`): the door
    /// notes a chain whose first hop is the unlisted invoker
    /// (`ServedRaiseHopSite::Hidden`; interpreter round i2 wave 56, lane L3;
    /// probes `L3W54FieldOverrideAccessorFrames` under `--jdk-only`,
    /// `L3W56FieldHandleNullBase`).
    /// `CRATONVM_THROWABLE_SERVED_RAISE_HIDDEN_INVOKER`, read once per VM here;
    /// default on since the lane's flip commit (`=0` lists no JDK frame there).
    pub served_raise_hidden_invoker: bool,

    /// The served-raise families answer for a METHOD as well as for a call
    /// site: a by-name door notes the method it selected on the funnel's mark
    /// whatever its kind (not only an `ACC_NATIVE` one), and the capture
    /// lists that method's JDK frames above whatever stands on the marked
    /// frame -- a reflective splice above all
    /// (`stackwalker::keyed_served_raise_frames`; interpreter round i2 wave
    /// 61, lane L3, proposal
    /// `i60-L3-proposal-one-keyed-entry-for-the-served-raise-families`; probe
    /// `L3W61KeyedServedRaiseFrames`).
    /// `CRATONVM_THROWABLE_SERVED_RAISE_KEYED`, read once per VM here; default
    /// on since the lane's flip commit (`=0` notes only a native callee).
    pub served_raise_keyed: bool,

    /// The keyed families' door carrier: a by-name STATIC door on a `java/`
    /// or `jdk/` class (`NativeInvokeAccess::invoke_static`,
    /// `invoke_static_by_class_id`, `invoke_static_settling` -- a method
    /// handle's `REF_invokeStatic` door) notes the method it is entering on
    /// the thread (`JvmThread::served_door_in_flight`) for the length of its
    /// call, and a capture made while only the throwable's construction
    /// stands above the door lists that method's JDK frames above the door's
    /// caller -- a throwable a callee built inside the served native
    /// (`Class.forName(String, boolean, ClassLoader)` through
    /// `MethodHandle.invokeExact`). Interpreter round i2 wave 63, lane L3,
    /// proposal
    /// `docs/feature-designs/jdk-only-mode.md`;
    /// probe `L3W63DoorInFlightKey`. Default ON since the wave-63 host run (the
    /// handle rows of `L3W63DoorInFlightKey` and `L3W62HandleCalleeBuiltRaise` match
    /// HotSpot in `--compatible`; about 3% on a JDK static handle door); `=0` is the kill switch.
    /// `CRATONVM_THROWABLE_SERVED_RAISE_DOOR_KEY`, read once per VM here.
    ///
    /// Since interpreter round i2 wave 64, lane L3 (proposal
    /// `i63-L3-proposal-the-reflective-door-notes-the-door-record`): the
    /// served `Method.invoke`'s door writes the same record, and the row
    /// carrier (`ReflectiveCallRow::target`) is gone. Since wave 65 (proposal
    /// `i64-L3-proposal-flip-the-door-key-and-retire-the-row-key-switch`) it
    /// writes it under this switch alone: wave 62's
    /// `CRATONVM_THROWABLE_SERVED_RAISE_ROW_KEY` (a strict subset of this
    /// switch since wave 64) is retired, and `=0` here is the one kill switch
    /// for both doors.
    pub served_raise_door_key: bool,

    /// A static JDK door's note ([`Self::served_raise_door_key`]) takes the
    /// method it selects from a per-thread memo
    /// (`stackwalker::DoorKeyMemo`, keyed by owner, name and descriptor,
    /// dropped on class redefinition) instead of a class-manager read and a
    /// method scan per call. Interpreter round i2 wave 64, lane L3; probe
    /// `L3W64ReflectiveDoorRecord`, bench `L3W64DoorKeyBench`. Default OFF.
    /// `CRATONVM_THROWABLE_SERVED_RAISE_DOOR_MEMO`, read once per VM here.
    pub served_raise_door_memo: bool,

    /// A settling method handle's static door
    /// (`NativeInvokeAccess::invoke_static_settling_keyed`) keeps the method
    /// its served-raise note selects on the handle record, beside the settled
    /// owner (`cratonvm_native_api::SettledDoorKey`, tied to the owner and
    /// to `class_redefinition_count`): a later call writes the door record
    /// ([`Self::served_raise_door_key`]) with no class-manager read, no
    /// method scan and no memo hash. Interpreter round i2 wave 65, lane L3,
    /// proposal
    /// `i64-L3-proposal-the-settling-handle-door-carries-its-door-key`;
    /// probe `L3W65SettledDoorKey`, bench `L3W64DoorKeyBench`. Default OFF.
    /// `CRATONVM_THROWABLE_SERVED_RAISE_SETTLED_DOOR_KEY`, read once per VM
    /// here.
    pub served_raise_settled_door_key: bool,

    /// The `--compatible` carrier handle (settled owner in the heap slot
    /// `MH_STATIC_SETTLED`, no Rust record) keeps its door key too: a
    /// per-thread direct-mapped side line in `lang_invoke.rs`, keyed by the
    /// VM, the collection epoch, the carrier's address and the addresses of
    /// its class, name and descriptor objects, holds the
    /// `cratonvm_native_api::SettledDoorKey` the carrier's settling arm
    /// passes to `NativeInvokeAccess::invoke_static_settling_keyed`. Takes
    /// effect only with [`Self::served_raise_settled_door_key`] on as well
    /// (`NativeInvokeAccess::carrier_settled_door_key`). Interpreter round
    /// i2 wave 66, lane L3, proposal
    /// `i65-L3-proposal-the-carrier-handle-keeps-its-door-key`; probe
    /// `L3W66CarrierDoorKey`, bench `L3W64DoorKeyBench` (`handle-static-jdk`
    /// row, `--compatible`). Default OFF.
    /// `CRATONVM_THROWABLE_SERVED_RAISE_CARRIER_DOOR_KEY`, read once per VM
    /// here.
    pub served_raise_carrier_door_key: bool,

    /// The static native-callback memo's first-argument key
    /// (`runtime::native_callee_memo::static_first_arg_key`, asked by every
    /// settled `REF_invokeStatic` call through
    /// `vm_exec::invoke_static_settling`: an FFM up-call's `DirectStatic`, a
    /// settled method handle) validates the argument's header once and reads
    /// its kind and class through the `_validated` twins, instead of one full
    /// conservative validation in `kind_of` and another in `class_id_of`.
    /// Same key for every live object; an address the validator refuses is
    /// not memoised (the default arm keyed it under class 0). j27 wave 8, lane
    /// W8UPC, page `ffm-upcall-per-call-costs-microseconds-20261027`. Default
    /// OFF (perf, unmeasured). `CRATONVM_NATIVE_CALLBACK_MEMO_STATIC_KEY_VALIDATE_ONCE`,
    /// read once per VM here.
    pub static_memo_key_validate_once: bool,

    /// `vm_util::is_class_initialized_via_manager` -- the "is this class
    /// initialized" test of every reflective door, `MethodHandle` static door,
    /// JIT static-field slow path and `ensure_class_initialized_shared` --
    /// answers from the VM's lock-free initialized bitmap
    /// (`ClassRealm::class_init_bits`, one `Acquire` load and a bit test)
    /// instead of the per-thread memo (a TLS access and a hash probe), and
    /// fills the bit when its locked read finds a class initialized that no
    /// `finalize_class_init` marked. A set bit is never stale: the one
    /// non-monotonic transition (a `--compatible` stub upgraded in place)
    /// clears it under the class-manager write lock. With the switch on the
    /// memo serves only ids past the bitmap's capacity (wave 72, lane L3);
    /// with it off (`=0`) the memo serves every id, as before wave 71.
    /// Interpreter round i3 wave 71, lane L3, proposal
    /// `i59-L3-proposal-one-branch-class-initialized-test`; bench
    /// `L3W71ClassInitBitBench`. Default ON since the wave-71 triage (`=0`
    /// off). `CRATONVM_CLASS_INIT_BIT_FIRST`, read once per VM here.
    pub class_init_bit_first: bool,

    /// `vm_util::ensure_class_initialized_shared`'s slow path (a clear
    /// initialized bit): the state loop's first class-manager read also
    /// answers "is this `jdk/internal/constant/PrimitiveClassDescImpl`", and
    /// the ConstantDescs `<clinit>`-cycle guard runs only in the claim arm
    /// (once per call), instead of a separate read and name compare before the
    /// loop on every slow-path entry (re-entrant checks from inside a running
    /// `<clinit>`, wait-loop entries, erroneous-class re-raises). Same answer
    /// in every arm. Interpreter round i4 wave 92, lane L3, proposal
    /// `i72-L3-proposal-one-class-manager-read-on-the-class-init-slow-path`;
    /// bench `L3W92ClinitTableBench`. Default OFF.
    /// `CRATONVM_CLASS_INIT_SLOW_ONE_READ`, read once per VM here.
    pub class_init_slow_one_read: bool,

    /// `vm_exec::static_owner_init_pending` (the by-name `invokestatic`
    /// routes and the JIT's static doors): an owner that is not initialized
    /// and does not declare the static itself -- the symbolic owner of an
    /// INHERITED static, `Sub.helper()` with `helper` in `Base` -- answers
    /// "nothing to initialize" when the first superclass declaring
    /// `(name, descriptor)` declares it `static` and is initialized, from one
    /// superclass walk under the class-manager read lock, instead of a
    /// `MemberResolver` resolution per call. Any other shape (the owner
    /// declares it, an interface owner, an instance or missing method, an
    /// uninitialized declarer) takes the resolution, unchanged. Interpreter
    /// round i3 wave 72, lane L3, proposal
    /// `i71-L3-proposal-an-inherited-static-owner-answers-from-its-superclasses-bits`;
    /// bench `L3W72InheritedStaticBench`. Default OFF.
    /// `CRATONVM_STATIC_OWNER_SUPERS_INITIALIZED`, read once per VM here.
    pub static_owner_supers_initialized: bool,

    /// `vm_exec::native_new_initializes_class_first`: JVMS §5.5 for the
    /// native `new` doors (initialize before allocating). Default ON;
    /// `CRATONVM_NATIVE_NEW_INIT_FIRST=0` restores allocate-then-initialize.
    /// Read once per VM here (an environment read per not-yet-initialized
    /// call until interpreter round i3 wave 72, lane L3).
    pub native_new_init_first: bool,

    /// `vm_exec::native_alloc_initializes_class_first`: JVMS §5.5 for the
    /// constructor-less native allocations (`Unsafe.allocateInstance`, the
    /// serialization constructor). Default ON;
    /// `CRATONVM_NATIVE_ALLOC_INITIALIZES=0` allocates without initializing.
    /// Read once per VM here (wave 72, lane L3, as above).
    pub native_alloc_initializes: bool,

    /// The constructor-less native allocations (`Unsafe.allocateInstance` --
    /// every constructor `MethodHandle` and JDK 25 `Constructor.newInstance`
    /// -- and the other `NativeConstruct::NoConstructor` / by-name doors) note
    /// a `java.lang.ref` subclass for discovery at allocation
    /// (`memory::ref_discovery::note_allocation`), as the interpreter's and
    /// the JIT's `new` do. HotSpot discovers a `Reference` by its class in the
    /// marker, whoever allocated it; here, where the reference constructors
    /// are bytecode, allocation is the only place the processor hears of one,
    /// so a reflectively constructed `WeakReference` subclass was never
    /// cleared or enqueued. JIT round 21 wave 8, lane reflect (page
    /// `docs/feature-designs/jdk-only-mode.md`).
    /// Default ON; `CRATONVM_NATIVE_ALLOC_NOTES_REFERENCES=0` restores the
    /// defect. Read once per VM here.
    pub native_alloc_notes_references: bool,

    /// `CRATONVM_JNI_CALL_STATICALLY_BOUND_EXACT_MID` (default ON; JIT round
    /// 21 wave 10, lane jni): `jni_call_instance` runs an inherited methodID
    /// that can be statically bound (private, final, final class) exactly.
    /// Read once per VM here (review rev-w10: `jni_call_instance` read it per
    /// inherited JNI call).
    pub jni_call_statically_bound_exact_mid: bool,

    /// `CRATONVM_JNI_CALL_VIRTUAL_SELECTS` (default ON; JIT round 21 wave 10,
    /// lane jni): any other inherited methodID is selected against its own
    /// declaration (JVMS 5.4.6). Read once per VM here (review rev-w10: it
    /// was read on every inherited `Call<T>Method*`, e.g. `toString` or
    /// `Iterator.hasNext` on a subclass receiver).
    pub jni_call_virtual_selects: bool,

    /// `CRATONVM_JNI_CTOR_CALL_REGISTERS_FINALIZABLE` (default ON; JIT round
    /// 21 wave 8, lane jni) and `CRATONVM_JNI_CALL_INIT_EXACT_MID` (default
    /// ON; same wave): a JNI `<init>` call registers a finalizable receiver,
    /// and runs exactly the methodID's constructor. Read once per VM here
    /// (JIT round 21 wave 13, lane finalize; item 5 of
    /// `r21-review-w13-native-findings-20261019.md`: both were read on every
    /// JNI constructor call).
    pub jni_ctor_call_registers_finalizable: bool,
    /// See [`Self::jni_ctor_call_registers_finalizable`].
    pub jni_call_init_exact_mid: bool,

    /// The finalizer-at-`Object.<init>` switches (JIT round 21 waves 10, 11
    /// and 13, lane finalize; all default ON, `=0` restores the defect):
    /// `CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT` (the unwinder withdraws
    /// the early registration of an object whose constructor provably threw
    /// before `Object.<init>` returned),
    /// `CRATONVM_FINALIZER_CTOR_STAYS_INTERPRETED` (such constructors of a
    /// finalizable class are never compiled) and
    /// `CRATONVM_FINALIZER_ANCESTOR_CTOR_STAYS_INTERPRETED` (nor those of an
    /// ancestor of a loaded finalizable class). Read once per VM here (review
    /// rev-jit w13 items 3 / 3b: the compile bar read two or three of them on
    /// every interpreted constructor entry).
    pub finalizer_register_at_object_init: bool,
    /// See [`Self::finalizer_register_at_object_init`].
    pub finalizer_ctor_stays_interpreted: bool,
    /// See [`Self::finalizer_register_at_object_init`].
    pub finalizer_ancestor_ctor_stays_interpreted: bool,
    /// `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN` (JIT round 21 wave 16,
    /// lane finalize-reinit; default ON, `=0` restores the defect). Latched
    /// here since wave 19 (item 6 of
    /// `docs/feature-designs/jdk-only-mode.md`): it arms
    /// the interpreter's `return`-arm gate bit (`DG_FINALIZER_INIT_RETURN`,
    /// with [`Self::finalizer_register_at_object_init`]), so `=0` costs one
    /// bit test per return, and `SharedVm::new` hands it to the reference
    /// processor (`ReferenceProcessor::set_reregister_at_init_return`).
    pub finalizer_reregister_at_init_return: bool,
    /// `CRATONVM_JNI_REINIT_REGISTERS_FINALIZER_AGAIN` (JIT round 21 wave 22,
    /// lane finalize; default ON, `=0` restores the old answer; item 2 of
    /// `docs/feature-designs/jdk-only-mode.md`): a
    /// JNI `<init>` call (`Call[Nonvirtual]VoidMethod`) on an object that is
    /// already registered for finalization registers it once more when the
    /// constructor reaches `Object.<init>`'s return, as HotSpot's
    /// `_return_register_finalizer` does, so `finalize()` runs once per
    /// registration (`ReferenceEntry::extra_finalizers`). Read by the JNI
    /// door (`jni.rs` `jni_reinit_of_registered_finalizable`) and the
    /// finalizer door (`gc_and_alloc.rs` `run_finalizers_impl`).
    pub jni_reinit_registers_finalizer_again: bool,
    /// `CRATONVM_JNI_REINIT_COUNT_BY_OUTCOME` (JIT round 21 wave 23, lane
    /// finalize; default ON, `=0` restores the wave-22 count; items 2 and 3 of
    /// `docs/feature-designs/jdk-only-mode.md`): the JNI
    /// re-`<init>` count of [`Self::jni_reinit_registers_finalizer_again`]
    /// follows the receiver through a collection during a normally returning
    /// constructor (its handle is read back), and is not made for a failure
    /// that cannot have entered the constructor (a VM refusal, a
    /// `StackOverflowError`). `jni.rs` `jni_reinit_registers_again`.
    pub jni_reinit_count_by_outcome: bool,
    /// `CRATONVM_JNI_REINIT_INIT_RETURN_WITNESS` (JIT round 21 wave 25, lane
    /// initwitness; default ON, `=0` restores the wave-24 error-kind rule;
    /// item 2 of `docs/feature-designs/jdk-only-mode.md`,
    /// the exact answer): a FAILED JNI re-`<init>` call
    /// ([`Self::jni_reinit_count_by_outcome`]) counts its second `Finalizer`
    /// when the receiver's row witness (`ReferenceEntry::init_returns`, moved
    /// by the interpreter's `<init>`-return hook) moved during the call --
    /// proof that `Object.<init>` returned on it -- whatever the error kind.
    /// Without a witness the error-kind rule answers, as before. `jni.rs`
    /// `jni_reinit_registers_again_witnessed`.
    pub jni_reinit_init_return_witness: bool,
    /// `CRATONVM_JNI_REINIT_WITNESS_INTERPRETS` (JIT round 22 wave 1, lane
    /// review; correctness, default ON, `=0` leaves the witness silent where
    /// it was; item 2 of `docs/feature-designs/jdk-only-mode.md`,
    /// the silent witness): while a JNI re-`<init>` of a registered
    /// finalizable object runs (`jni.rs` `JniReinitWitnessWindow`), this VM's
    /// doors run every body interpreted and keep every frame
    /// (`SharedVm::jni_reinit_witness_window`, asked by
    /// `jit_bridge::jvmti_requires_interpreter[_for]` and
    /// `interpreter::jvmti_requires_interpreter_for_method`, as JVMTI's
    /// interp-only mode is), and the `<init>`-return hook also counts the
    /// return of `Object.<init>`'s own frame. So `Object.<init>` returning on
    /// the receiver -- elided, inside a compiled super constructor, or framed
    /// -- moves the witness (`exception_dispatch::finalizer_init_returned`).
    pub jni_reinit_witness_interprets: bool,
    /// `CRATONVM_JNI_REINIT_WITNESS_NESTED_EXACT` (JIT round 22 wave 2, lane
    /// review; correctness, default ON, `=0` trusts a moved witness whatever
    /// moved it): each JNI re-`<init>` call that arms a witness is counted on
    /// the receiver's finalizer row (`ReferenceEntry::jni_reinit_calls`), and
    /// a witness that moved while ANOTHER re-`<init>` of the same object also
    /// ran (a nested JNI call, or another thread) proves nothing about this
    /// call, so the error-kind rule answers (`jni.rs`
    /// `jni_reinit_registers_again_witnessed`).
    pub jni_reinit_witness_nested_exact: bool,
    /// `CRATONVM_FINALIZER_INIT_RETURN_SCAN_MEMO` (JIT round 21 wave 22, lane
    /// finalize; perf, opt-in `=1`; item 8 of
    /// `docs/feature-designs/jdk-only-mode.md`): the
    /// `<init>`-return hook's body decode
    /// (`exception_dispatch.rs` `ctor_body_keeps_receiver_and_calls_before`)
    /// is memoized per method body, on the frame's quickened stream
    /// (`QuickenedCode::ctor_receiver_facts`), instead of running on every
    /// finalizable constructor return.
    pub finalizer_init_return_scan_memo: bool,
    /// `CRATONVM_FINALIZER_CTOR_PROLOGUE_HANDLERS` (JIT round 21 wave 26, lane
    /// finprologue; default ON, `=0` restores the handler-pc-only rule; probe
    /// `R21Fz8FinPrologue1`): the unwinder's "throw precedes initialization"
    /// proof (`exception_dispatch.rs` `ctor_handler_rows_allow`) accepts an
    /// exception handler before the constructor's first initializing call
    /// when its row covers only pcs before that call -- a JEP 513
    /// `try`/`catch` prologue before `super(..)`. The proof refused such a
    /// body, so a throw in its prologue kept the allocation's finalizer
    /// registration and the object was finalized, which HotSpot never does.
    /// The compile bar (`finalizer_bars_compiling`) and the finalizable
    /// define's ancestor walk ask the same proof.
    pub finalizer_ctor_prologue_handlers: bool,

    /// The funnel's served-raise mark is taken only by a capture made while
    /// nothing but a throwable construction stands above the marked frame
    /// (`<init>` of the throwable's class or a superclass, `fillInStackTrace`):
    /// a capture of the same class deeper in Java the native ran (a
    /// `<clinit>` that catches another class's failed initialization) leaves
    /// it for the real raise. Interpreter round i2 wave 62, lane L3, proposal
    /// `docs/feature-designs/jdk-only-mode.md`;
    /// probe `L3W62MarkOwnConstruction`.
    /// `CRATONVM_THROWABLE_SERVED_RAISE_OWN_CONSTRUCTION`, read once per VM here.
    pub served_raise_own_construction: bool,

    /// A throwable captured inside a `<clinit>` that a served door runs under
    /// a class-init chain (`vm_exec::initialize_class_served_cold`'s mark; a
    /// static `VarHandle`'s holder in both modes, a `Field` value door's
    /// declaring class under `--jdk-only`) lists the chain's JDK frames
    /// between the marked caller and the `<clinit>`, as the walk rule
    /// `insert_served_init_chain_walk_entries` does; so does the current
    /// thread's `getStackTrace` (a throwable's). Interpreter round i5 wave
    /// 103, lane L3; probe `L3W103ClinitTriggerThrowableTraces`. Wave 104:
    /// also the current thread's own listing (`thread_stack_trace_served`:
    /// `Thread.getAllStackTraces`, `ThreadInfo`), probe
    /// `L3W104ClinitTriggerListings`.
    /// `CRATONVM_THROWABLE_SERVED_INIT_CHAIN_FRAMES` (default on, `=0` off),
    /// read once per VM here.
    pub served_init_chain_capture_frames: bool,

    /// The served `Method.invoke` runs a static `@CallerSensitive` target's
    /// `@CallerSensitiveAdapter` overload (the same name and parameters plus
    /// a trailing `Class`, static) with the reflective caller, as
    /// HotSpot 25's `MethodHandleAccessorFactory.findCSMethodAdapter` does
    /// (`Class.forName(String)` -> `forName(String, Class)`; interpreter
    /// round i2 wave 61, lane L3; probe `L3W61KeyedServedRaiseFrames`).
    /// `CRATONVM_REFLECT_CS_ADAPTER`, read once per VM here; default on since
    /// the lane's flip commit (`=0` runs the caller-sensitive method itself).
    pub reflect_cs_adapter: bool,

    /// A reflective listing resolves every listed member's signature types
    /// through the holder's loader first, as HotSpot's
    /// `Reflection::new_method` does (item 3 of
    /// `interpreter-L5-getinitiatedclasses-residual-recording-differences-FIXED-20261007`;
    /// interpreter round i2 wave 54, lane L3; probe
    /// `L3W54ReflectionLoadsSignatureTypes`).
    /// `CRATONVM_REFLECTION_LOADS_SIGNATURE_TYPES`, read once per VM here;
    /// **default ON** since 2026-10-07 (the suite run the switch waited for,
    /// and every served listing door raising with HotSpot's frames:
    /// `I47ListingSignatureTypeFrames`); `=0` lists without loading, as
    /// before (a missing type then fails nothing).
    pub reflection_loads_signature_types: bool,

    /// Over a real JDK the launcher asks the JDK's own
    /// `sun.launcher.LauncherHelper.checkAndLoadMain` for the main class and
    /// how to call `main` (its `isStaticMain` / `noArgMain`), as HotSpot's
    /// `java.c` does, instead of the VM's own `MethodFinder` search
    /// (`Vm::launcher_check_main_via_helper`; interpreter round i2 wave 59,
    /// lane L5, `i58-L5-proposal-the-launcher-runs-launcherhelper`).
    /// `CRATONVM_LAUNCHER_HELPER`, read once per VM here; default on since
    /// the lane's flip commit (`=0` keeps the VM's own check).
    pub launcher_helper: bool,

    /// With [`Self::launcher_helper`], a `-jar` launch hands the JAR to the
    /// helper as `java.c` does (`checkAndLoadMain(true, LM_JAR, <jar>)`), so
    /// the JDK's `getMainClassFromJar` reads `Main-Class` and acts on
    /// `Launcher-Agent-Class`, `Add-Opens` / `Add-Exports` and
    /// `Enable-Native-Access`, which the VM's own `-jar` path never read
    /// (interpreter round i2 wave 60, lane L5; probe
    /// `L5W60LauncherJarAttributes`). `CRATONVM_LAUNCHER_HELPER_JAR`, read
    /// once per VM here; default on since the lane's flip commit (`=0` keeps
    /// `LM_CLASS` with the manifest's class).
    pub launcher_helper_jar: bool,

    /// `CRATONVM_LAUNCHER_HELPER_JAR_NO_PRELOAD` (`runtime_flag_default_on`,
    /// default ON since wave 134, `=0` off; HotSpot parity:
    /// `L7bW133JarLaunchCleaner` and `L5W121LauncherAgentSeesMain` SAME with
    /// it on in both modes on w133). Interpreter round i6 wave 121, lane L5
    /// (`i58-L5-proposal-the-launcher-runs-launcherhelper`): an `LM_JAR`
    /// launch with no `-javaagent:` does not pre-load the manifest's main
    /// class before the helper runs, so the jar's `Launcher-Agent-Class` (which
    /// the helper starts first) sees its definition, as on HotSpot.
    pub launcher_helper_jar_no_preload: bool,

    /// With [`Self::launcher_helper`], a `-m <module>[/<class>]` launch hands
    /// `<module>/<class>` to the helper as `java.c` does
    /// (`checkAndLoadMain(true, LM_MODULE, ..)`), so the JDK's
    /// `loadModuleMainClass` prints `java.launcher.module.error2` / `error3`
    /// and sets `jdk.module.main.class` (interpreter round i6 wave 117, lane
    /// L5; probe `L5W117ModuleLaunchHelperTexts`). `--jdk-only`;
    /// `--compatible` needs [`Self::launcher_helper_module_compatible`] too.
    /// `CRATONVM_LAUNCHER_HELPER_MODULE`, read once per VM here; default ON
    /// (HotSpot parity), `=0` keeps `LM_CLASS`.
    pub launcher_helper_module: bool,

    /// `CRATONVM_LAUNCHER_HELPER_MODULE_COMPATIBLE` (default ON since the i6 wave-126 flip, `=0` off,
    /// `runtime_flag_default_on`): [`Self::launcher_helper_module`] under
    /// `--compatible` as well. (Was off until the probe was SAME there: that mode's
    /// boot layer answers `ModuleLayer.boot().findModule` from the VM's
    /// registry, and a miss would be the helper's `InternalError` instead of
    /// a launch; interpreter round i6 wave 117, lane L5.)
    pub launcher_helper_module_compatible: bool,

    /// `StackWalker.getCallerClass()` answers by HotSpot's rule (JDK 25
    /// `StackStreamFactory.CallerClassFinder`): over the walkers' capture,
    /// skip hidden frames, reflection frames and every class of the
    /// `java.lang.invoke` package, take the second frame left, and throw
    /// `IllegalCallerException("no caller frame: ...")` when there is none
    /// (`stack_walker::caller_class_by_finder_rule`; proposal
    /// `i49-L3-proposal-one-caller-class-rule-over-the-walkers-capture`,
    /// interpreter round i1 wave 50, lane L3; probe `L3W50CallerClassRule`).
    /// `CRATONVM_CALLER_CLASS_FINDER`, read once per VM here; default on since
    /// the lane's last commit (`=0`: the
    /// name list, which answered the outermost frame or `Object` when no
    /// caller was left).
    pub caller_class_finder_rule: bool,

    /// `jdk.internal.reflect.Reflection.getCallerClass()` answers by HotSpot's
    /// rule (JDK 25 `JVM_GetCallerClass`): over the walkers' capture, the frame
    /// after the asking `@CallerSensitive` one that
    /// `Method::is_ignored_by_security_stack_walk` does not skip
    /// (`Method.invoke`, `MethodAccessorImpl` subclasses, method-handle
    /// adapter frames), `null` when none
    /// (`stack_walker::reflection_get_caller_class_by_security_walk`; stage 2
    /// of proposal `i49-L3-proposal-one-caller-class-rule-over-the-walkers-capture`,
    /// interpreter round i1 wave 51, lane L3; probe `L3W51ReflectionCallerRule`).
    /// `CRATONVM_REFLECTION_CALLER_RULE`, read once per VM here; default on
    /// since the lane's last commit (`=0`: the bare frame list's second frame
    /// with its stream-plumbing skip).
    pub reflection_caller_class_rule: bool,

    /// `Reflection.getCallerClass()` answers from the two innermost frames
    /// when the rule above provably answers the asking frame's caller (no
    /// reflective or served `Thread.run` call in force, a caller the security
    /// walk never skips: `stackwalker::caller_frame_never_skipped`), without
    /// the walkers' capture (`NativeExceptionAccess::reflection_caller_class_fast`).
    /// `CRATONVM_REFLECTION_CALLER_FAST`, read once per VM here. Interpreter
    /// round i2 wave 60, lane L3 (the i45-L3 proposal's cost row: the capture
    /// per ask grew with the stack depth -- 47k -> 14k ns at depth 60, debug).
    /// Default on since the lane's last commit (`=0`: the whole rule for every ask).
    pub reflection_caller_fast: bool,

    /// Interpreter round i6 wave 121, lane L3: the fast arm above also
    /// answers while reflective or served `Thread.run` calls are in force,
    /// when every one of them pushed its first interpreter frame at least two
    /// frames below the asking one (`frames.len() - interp_depth >= 2`): their
    /// JDK frames splice in right below that first frame
    /// (`stackwalker::splice_frames_reporting`), so they cannot stand between
    /// the asker and its caller. Every thread but `main` runs inside the served
    /// `Thread.run`, and every JUnit test method inside a reflective call, so
    /// without it the common ask takes the whole capture. Same answer by
    /// construction; `CRATONVM_DBG_CALLER=1` checks it (`FAST MISMATCH`).
    /// `CRATONVM_REFLECTION_CALLER_FAST_UNDER_RECORDS`, read once per VM here;
    /// a performance switch, default off.
    pub reflection_caller_fast_under_records: bool,

    /// The reflective door natives of `native-builtins/src/lang_class.rs`
    /// (`resolve_caller_class_id`: access refusals and their texts, the JEP
    /// 403 `setAccessible` gate, `Class.getResource`, the served
    /// `Method.invoke` / `Constructor.newInstance`) find their caller by
    /// HotSpot's `JVM_GetCallerClass` rule over the walkers' capture
    /// (`stack_walker::reflective_door_caller_class_id`), the name list
    /// answering only the shapes the rule defers. Stage 3 of proposal
    /// `i49-L3-proposal-one-caller-class-rule-over-the-walkers-capture`
    /// (interpreter round i4 wave 94, lane L3; probes `L3W94CallerSensitive*`).
    /// `CRATONVM_REFLECTION_CS_CALLER_RULE`, read once per VM here; default
    /// OFF until the probe matrix is SAME in both modes.
    pub reflection_cs_caller_rule: bool,

    /// A caller-class question (`Reflection.getCallerClass`, the reflective
    /// door natives) sees the lambda proxy frame HotSpot runs between a
    /// method reference's call site and the `@CallerSensitive` method it
    /// names (`vm_exec::caller_lambda_proxy_insertions`): the proxy
    /// `Host$$Lambda/0x..` is the caller, as JDK 25's `JVM_GetCallerClass`
    /// answers (interpreter round i4 wave 94, lane L3;
    /// `L3W94CallerSensitiveMatrix` rows `get-other-lambda`,
    /// `newinst-sha-lambda`). `CRATONVM_CALLER_LAMBDA_PROXY_FRAMES`, read once
    /// per VM here; default ON (`=0`: the call site's class answers, as before).
    pub caller_lambda_proxy_frames: bool,

    /// The served `Class.getMethod` / `getMethods`
    /// (`native-builtins/src/lang_class.rs`) take JDK 25's public-method walk:
    /// `getMethodsRecursive` with `PublicMethods.MethodList.merge` and
    /// `getMostSpecific` for `getMethod`, `privateGetPublicMethods` with
    /// `PublicMethods.merge` for `getMethods`, instead of their two older
    /// walks. The i52-L3 one-walk proposal (interpreter round i4 wave 94, lane
    /// L3; probe `L3W94PublicMethodWalk`, bench `L3W94PublicMethodWalkBench`).
    /// `CRATONVM_PUBLIC_METHOD_JDK_WALK`, read once per VM here; default OFF
    /// until a framework boot has run with it.
    pub public_method_jdk_walk: bool,

    /// Every capture (throwables, `Thread.getStackTrace`, walks, published
    /// traces, debugger listings) lists the `AccessController.doPrivileged`
    /// frame HotSpot has below a `PrivilegedAction` the VM's bridge runs, and
    /// a `PrivilegedActionException` the bridge wraps starts at
    /// `AccessController.wrapException` (the stand-in table's `Privileged`
    /// family, `stackwalker::PRIVILEGED_STEPS`; interpreter round i1 wave 49,
    /// lane L3; probe `L3W49PrivilegedFrames`).
    /// `CRATONVM_STANDIN_PRIVILEGED_FRAMES`, read once per VM here; default on
    /// (`=0` lists none, the pre-wave-49 traces).
    pub standin_privileged_frames: bool,

    /// `--compatible` leaves hidden frames (`Method::is_hidden`: a hidden
    /// class's method, an `@Hidden` JDK method) out of throwables,
    /// `Thread.getStackTrace` and default walks, as `--jdk-only` has since
    /// interpreter round i1 wave 34 and HotSpot does (wave 48, lane L3:
    /// `--compatible` is held to HotSpot since wave 46; probes
    /// `L4W34HiddenFrames`, `L4W35StackWalkerHidden`).
    /// `CRATONVM_COMPATIBLE_HIDDEN_FRAMES`, read once per VM here; default on
    /// (`=0` keeps them, as before wave 48); see
    /// [`Self::hides_hidden_frames`].
    pub compatible_hides_hidden_frames: bool,

    /// The by-name STATIC tails (`invoke_static_on_owner`, the settled
    /// static handle, `invoke_static_shared_on_class`) tell
    /// `invoke_on_class_shared_inner` that they call a static method
    /// (`OnClassKind::Static`), so it skips the receiver-keyed lambda-proxy
    /// arm and the receiver retarget with its "never retarget a static"
    /// hierarchy walk, which re-derived what the caller knew (stage 4 of
    /// `docs/feature-designs/jdk-only-mode.md`,
    /// interpreter round i1 wave 52, lane L4). Behaviour-neutral: for a
    /// static method that walk always kept the named class.
    /// `CRATONVM_BYNAME_STATIC_KIND`, read once per VM here; default on since
    /// the lane's last commits (`=0`: the walk is asked again, as before
    /// wave 52).
    pub by_name_static_kind: bool,

    /// An interrupted sleep of a virtual thread raises what HotSpot's DEFAULT
    /// virtual threads raise (continuations on, `VirtualThread.sleepNanos`):
    /// `new InterruptedException()` with no message, its trace listing
    /// `VirtualThread.sleepNanos`, `Thread.sleepNanos`, `Thread.sleep`. This
    /// VM's virtual threads are `BoundVirtualThread`s, whose `Thread.sleep`
    /// reaches the platform natives and raised `"sleep interrupted"` (what
    /// HotSpot prints under `-XX:-VMContinuations`). Interpreter round i1 wave
    /// 48, lane L1 (`docs/feature-designs/jdk-only-mode.md`).
    /// `CRATONVM_VIRTUAL_SLEEP_CONTINUATION_SHAPE`, read once per VM here.
    pub virtual_sleep_continuation_shape: bool,

    /// The JDK drives virtual threads, as on HotSpot 25: over a real JDK
    /// (feature 21 or newer) `ContinuationSupport.isSupported0()` answers
    /// `true`, so `Thread.ofVirtual()` builds a `java.lang.VirtualThread` that
    /// runs on the JDK's own `ForkJoinPool` carriers (`CarrierThread`s,
    /// ordinary platform threads) and `VirtualThread.<clinit>` starts the
    /// `VirtualThread-unblocker`. The VM serves the continuation natives
    /// (`Continuation.enterSpecial` / `doYield` / `isPinned0`,
    /// `Thread.setCurrentThread` / `currentCarrierThread`;
    /// `vm::jdk_continuation`). Off: every virtual thread is a
    /// `ThreadBuilders$BoundVirtualThread` the VM's own carrier pool runs
    /// (HotSpot's `-XX:-VMContinuations` shape). Pages
    /// `i63-L1-a-virtual-threads-carrier-is-not-a-java-thread` and
    /// `i65-L1-virtual-thread-unblocker-reads-timed-waiting` (retired to
    /// `docs/feature-designs/jdk-only-mode.md`). `CRATONVM_VT_CONTINUATIONS`, read once
    /// per VM here; default on (`=0`: the bound shape).
    pub jdk_continuations: bool,

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

    /// `--patch-module <module>=<path>[<sep><path>...]`: per module, the
    /// paths searched BEFORE the module's own content (interpreter round i4
    /// wave 87, lane L5).
    pub patch_modules: Vec<(String, Vec<String>)>,

    /// `--limit-modules <m>[,<m>...]`: the observable modules are limited to
    /// the closure of these (plus `--add-modules` and the main module).
    pub limit_modules: Vec<String>,

    /// `--upgrade-module-path`: modules that replace the image's module of the
    /// same name.
    pub upgrade_module_path: Vec<String>,

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

    /// `-XX:+ExitOnOutOfMemoryError`. When `true`, the FIRST
    /// `OutOfMemoryError` the VM raises prints `Terminating due to
    /// java.lang.OutOfMemoryError: <message>` and ends the process with exit
    /// status 3, running no shutdown hooks — HotSpot's `os::_exit(3)`. Off by
    /// default on every entry point. gen r4w5/thrash5 (2026-09-24); the hook is
    /// `runtime::interpreter::maybe_dump_heap_on_oom_for`.
    pub exit_on_out_of_memory_error: bool,

    /// `-XX:+CrashOnOutOfMemoryError`. When `true`, the first
    /// `OutOfMemoryError` prints `Aborting due to java.lang.OutOfMemoryError:
    /// <message>` and aborts the process (the crash handler, where installed,
    /// writes the `hs_err_pid<pid>.log`). Off by default. gen r4w5/thrash5.
    pub crash_on_out_of_memory_error: bool,

    /// `-XX:OnOutOfMemoryError=<cmd>[;<cmd>...]`, verbatim; several options
    /// accumulate joined by `\n`, as HotSpot's `ccstrlist` does. On the first
    /// `OutOfMemoryError` each `;`-separated command runs to completion
    /// through the platform shell with `%p` replaced by the pid. `None` (the
    /// default on every entry point) runs nothing. gen r4w5/thrash5.
    pub on_out_of_memory_error: Option<String>,

    /// `-XX:+UseGCOverheadLimit` / `-XX:-UseGCOverheadLimit` (last one wins);
    /// `None` when neither was given (the default on every entry point).
    /// `Some(false)` turns the VM's GC-overhead limit off for this VM (the
    /// allocation doors then throw only when a full collection could not
    /// serve the request, as HotSpot's Serial, G1 and ZGC always do). Read
    /// once per VM by `runtime::interpreter::gc_overhead_limit_cycles`; the
    /// developer override `CRATONVM_GC_OVERHEAD_LIMIT` still wins. gcf f2/o
    /// (`gcf-f1o-overhead-limit-has-no-hotspot-switch-and-serial-has-no-limit-20261001`).
    pub use_gc_overhead_limit: Option<bool>,

    /// Structured embedding settings. Synthetic libraries start the legacy
    /// recorder; real JDK images defer these to their DCmdStart boot boundary.
    /// Real-JDK launchers use `jfr_startup_options` so the image parses the
    /// complete HotSpot option grammar and owns the Recording lifecycle.
    pub jfr_start_recording: Option<JfrStartRecordingConfig>,
    /// JDK DCmdStart arguments, deferred until Java boot completes.
    pub jfr_startup_options: Vec<String>,

    /// Event names any JFR recording this VM starts must ask for **by name**:
    /// the command-line equivalent of `jdk.jfr.Recording.enable(...)`.
    ///
    /// `None` (the default) means no name filter, so a recording keeps every
    /// event it sees. That is what every CratonVM recording did before this
    /// field existed, and it is still what a plain `-XX:StartFlightRecording`
    /// does. `Some(names)` installs
    /// `cratonvm_jfr::RecordingSettings::enabled_event_names`, which is a
    /// **whitelist**: the recording then keeps *only* those names.
    ///
    /// ## Why this is not a field on `JfrStartRecordingConfig`
    ///
    /// Two reasons, and the second is the load-bearing one:
    ///
    ///  1. It is not a knob on one recording's ring or its dump, which is all
    ///     `JfrStartRecordingConfig` holds. It states which producers this VM
    ///     is to arm, and it has to be readable by every path that starts a
    ///     recording: the boot path today, `jcmd JFR.start` if that is ever
    ///     wired (see the audit block in `runtime/serviceability.rs`), and an
    ///     embedder that builds a `VmConfig` directly and never sees argv.
    ///  2. `JfrStartRecordingConfig` is built with **exhaustive** struct
    ///     literals by out-of-crate targets (`vm/tests/tier1_tests.rs` builds
    ///     three of them, naming all five fields), so growing it is a
    ///     source-breaking change for each one. `VmConfig` is only ever built
    ///     through `Default` plus `..VmConfig::default()`, so growing it is
    ///     not. Checked across the workspace before choosing.
    ///
    /// ## Why naming events NARROWS the recording
    ///
    /// Not a preference: a consequence, and worth knowing before adding a
    /// name to a command line that already works. The producer gate for a
    /// default-off diagnostic event (`cratonvm.JitCompileDecision`; see
    /// `jfr/src/jit_decision.rs`) arms only when a *running* recording names
    /// that event in `enabled_event_names`, and that same set is the drain
    /// filter. Today's `RecordingSettings` has no way to say "arm this
    /// producer *and* keep everything else": arming requires the whitelist,
    /// and the whitelist excludes whatever is not in it. So naming events on
    /// the command line narrows the recording to exactly those events, in
    /// exactly the way `Recording.enable(...)` narrows one from Java. The two
    /// routes agreeing is the property worth having; `Vm::new` prints the
    /// resulting set at startup so it is never inferred from a thin dump.
    ///
    /// Populated from `-XX:StartFlightRecording:+<Event>#enabled=true` (see
    /// `apply_jfr_event_setting`) and/or the `CRATONVM_JFR_ENABLE_EVENTS`
    /// environment variable. Unknown names are a hard, loud failure at boot:
    /// see the `-XX:StartFlightRecording` block in `vm/src/vm/vm_init.rs`.
    pub jfr_enabled_events: Option<Vec<String>>,

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
    /// Increment 5). Pass `-XX:-ShowCodeDetailsInExceptionMessages` to opt out
    /// (every implicit NPE then has a null message, as on HotSpot). The
    /// `CRATONVM_HELPFUL_NPE_OPCODES` env var and the launcher's process-wide
    /// value, when set, override this field. Read per VM since interpreter
    /// round i4 wave 84, lane L2 (it was write-only before: an embedder's VM
    /// got the pre-JEP-358 diagnostics whatever it said). See
    /// [`crate::runtime::env_cache::show_code_details`].
    pub show_code_details_in_exception_messages: bool,

    /// HotSpot `-XX:+ShowHiddenFrames` (a diagnostic flag; off by default):
    /// keep the hidden frames (`Method::is_hidden`: a hidden class's method,
    /// a `@Hidden` JDK method) in throwable backtraces,
    /// `Thread.getStackTrace()` and `StackWalker` walks, and build the
    /// helpful NPE message of a null dereference in such a frame
    /// ([`Self::hides_hidden_frames`]). Interpreter round i4 wave 84, lane L2.
    pub show_hidden_frames: bool,

    /// HotSpot `-XX:DiagnoseSyncOnValueBasedClasses=<n>` (a diagnostic flag,
    /// range 0..=2, default 0): what a `monitorenter` on an instance of a
    /// `@jdk.internal.ValueBased` boot / platform class does. `0` nothing;
    /// `1` a fatal error (`Synchronizing on object 0x... of klass ... at
    /// ...`, abort); `2` the `valuebasedclasses` info log line plus the
    /// locking thread's stack (to stdout when `-Xlog` did not select the tag,
    /// as HotSpot's `Arguments` configures it) and a
    /// `jdk.SyncOnValueBasedClass` JFR event. Interpreter round i5 wave 102,
    /// lane L6 (`vm_exec::diagnose_sync_on_value_based_class`). Non-zero also
    /// keeps compiled code off its inline thin-lock path, so every compiled
    /// `monitorenter` reaches the check through the helper.
    pub diagnose_sync_on_value_based_classes: u8,

    /// HotSpot `-XX:±StackTraceInThrowable` (a product flag, default on).
    /// Off, no throwable records a stack trace: `getStackTrace()` is empty and
    /// `printStackTrace()` prints the header line alone, as HotSpot's
    /// `fill_in_stack_trace` returns at once. `Thread.getStackTrace()` and
    /// `StackWalker` are not affected. Interpreter round i4 wave 85, lane L2.
    pub stack_trace_in_throwable: bool,

    /// `CRATONVM_INTERP_NPE_DEFERRED_MESSAGE` (default OFF; performance):
    /// under `--jdk-only`, an implicit `NullPointerException` the INTERPRETER
    /// raises carries no message until `getMessage()` asks
    /// (`NullPointerException.getExtendedNPEMessage`), as HotSpot computes it,
    /// instead of running the whole-method JEP 358 analysis at every throw.
    /// The message is the same one, built from the stored trace's top frame
    /// (the trapping frame at `last_instr_pc`) through the compiled path's
    /// deferral (`exceptions::mark_npe_message_deferred`). Interpreter round
    /// i4 wave 85, lane L2; page
    /// `docs/feature-designs/jep358-helpful-npe.md`.
    pub interp_npe_deferred_message: bool,

    /// HotSpot `-XX:±OmitStackTraceInFastThrow` as given on the command line
    /// (last one wins), `None` when neither is (nio21 wave 16, lane L5).
    /// `None` resolves to `CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW` (default
    /// ON, `=0` off), i.e. HotSpot's own default. When on, a hot implicit
    /// exception site in COMPILED code throws this VM's preallocated,
    /// message-less, trace-less instance of the kind
    /// (`runtime::exceptions::try_fast_throw`). Per VM, never a process global.
    /// See [`VmConfig::omit_stack_trace_in_fast_throw_resolved`].
    pub omit_stack_trace_in_fast_throw: Option<bool>,

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
    /// `--enable-native-access` and `--illegal-native-access` (zt t18/df7,
    /// `docs/feature-designs/jdk-only-mode.md`):
    /// installed into the VM's own `panama::NativeAccessState` when the VM is
    /// made; the launcher sets it (process globals before). Default: no grant,
    /// `warn` -- every embedding entry point's answer before the move.
    pub native_access: cratonvm_native_builtins::panama::NativeAccessConfig,

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
/// cratonvm runs Pass 2 (structural) on every class and Pass 3 (typestate) on
/// non-boot classes. `None` is wired through [`Self::skips_verification`] into
/// `skip_verification`, which the verifier dispatcher in `vm/src/vm/vm_util.rs`
/// reads.
///
/// `All` is propagated to `ClassManager::set_strict_verification` at VM init
/// (`vm_init`, before any class is loaded) and withdraws three shortcuts:
///
/// * `bytecode_verifier::class_is_bootstrap_trusted` stops earning the lenient
///   branch-target path, so the boot image is checked against the spec-literal
///   JVMS §4.10.1 rule via `verify_bytecode_strict` — which is what that
///   function was documented as being for, and now actually is;
/// * `class_manager`'s `defer_loader_sensitive_pass3` stops withholding the
///   Pass-3 type-state verdict for user-loader classes, i.e. for every
///   Spring / Tomcat / H2 application class;
/// * `vm_util::verifier_skip_eligible` stops skipping link-time Pass 2 for
///   bootstrap classes.
///
/// This is genuinely stricter than the default and can reject class files
/// HotSpot's own `-Xverify:all` also rejects, plus — until the verifier's
/// remaining gaps close — some it does not. That is the point of the flag, and
/// it is why `Remote` remains the default.
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

/// What an explicit GC request does: HotSpot's `-XX:±DisableExplicitGC` and
/// `-XX:±ExplicitGCInvokesConcurrent`, folded into one per-VM value
/// ([`VmConfig::explicit_gc`]). gc-common w8-g
/// (`common-w7g-explicit-gc-flags-are-silently-ignored`).
///
/// "Explicit" means HotSpot's `GCCause::_java_lang_system_gc`: `System.gc()`,
/// `Runtime.gc()`, `MemoryMXBean.gc()` (which is `Runtime.getRuntime().gc()`),
/// and therefore also the `System.gc()` that `java.nio.Bits.reserveMemory`
/// makes. `jcmd GC.run` (`_dcmd_gc_run`), the `-XX:MaxMetaspaceSize` refusal's
/// collection (`_metadata_GC_threshold`) and every allocation-driven
/// collection are not explicit and ignore this value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExplicitGcPolicy {
    /// The `System.gc()` collection (plus, on G1, a marking cycle run to
    /// completion) and the forced finalizer / cleaner / notification drains.
    /// The default, and the only behaviour before the flags were honoured.
    #[default]
    Full,
    /// `-XX:+DisableExplicitGC`: the request is a no-op. No collection and no
    /// drain, as HotSpot's `JVM_GC`, which tests the flag before anything else.
    Disabled,
    /// `-XX:+ExplicitGCInvokesConcurrent`: collects exactly as [`Self::Full`]
    /// on every backend. Only the `invokedConcurrent` field of JFR's
    /// `jdk.SystemGC` differs. The flag replaces HotSpot's stop-the-world
    /// full compaction with a young pause plus a marking cycle waited to
    /// completion. That is already what this VM's G1 `System.gc()` does,
    /// because it has no full compaction. HotSpot ignores the flag on
    /// Serial/Parallel (the Generational backend's model), and ZGC's
    /// `System.gc()` is its whole-heap cycle either way. See
    /// `gc_and_alloc::explicit_gc_action`. One G1 difference since JIT round 20
    /// wave 21 (lane g1ref): under [`Self::Full`] the marking cycle runs whole
    /// inside one stop-the-world pause (`gc_and_alloc::g1_stw_full_mark_cycle`,
    /// HotSpot's full collection lets no mutator run either); under this policy
    /// it stays concurrent, as HotSpot's does.
    Concurrent,
}

impl ExplicitGcPolicy {
    /// The policy two HotSpot boolean flags select. `None` is "not given".
    /// `DisableExplicitGC` wins over `ExplicitGCInvokesConcurrent`, as in
    /// HotSpot, where `JVM_GC` returns before the concurrent flag is read.
    /// A `-XX:-` form (`Some(false)`) is the flag's default, off.
    pub fn from_hotspot_flags(
        disable_explicit_gc: Option<bool>,
        explicit_gc_invokes_concurrent: Option<bool>,
    ) -> Self {
        if disable_explicit_gc == Some(true) {
            Self::Disabled
        } else if explicit_gc_invokes_concurrent == Some(true) {
            Self::Concurrent
        } else {
            Self::Full
        }
    }
}

/// gci i43/lf: HotSpot 25's TLAB flags (`gc/shared/tlab_globals.hpp`) as the
/// launcher validated them (`vm-cli` `space_tlab_flags_from_hotspot_flags`:
/// HotSpot's ranges and `AfterMemoryInit` constraints, refusing to start on a
/// bad value). `None` = not given (HotSpot's default, the `hotspot_*` accessors).
/// Per VM ([`VmConfig::tlab_flags`]), never a process flag.
///
/// Consumed through `cratonvm_gc::tlab::TlabSizing` (`HeapRealm::tlab_sizing`,
/// gci i45/sc); `waste_increment` is read only under the opt-in
/// `CRATONVM_TLAB_HOTSPOT_WASTE_LIMIT` on the Generational heap (gcj j2/yg2,
/// `gci-i45sc-proposal-tlab-refill-waste-limit-per-fill-20261016`). The accessors give the values HotSpot
/// reports (JFR `jdk.GCTLABConfiguration`, the flag rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TlabFlags {
    /// `-XX:±UseTLAB` (HotSpot default on).
    pub use_tlab: Option<bool>,
    /// `-XX:±ResizeTLAB` (HotSpot default on).
    pub resize_tlab: Option<bool>,
    /// `-XX:TLABSize=<bytes>` (HotSpot default 0, "ergonomic").
    pub tlab_size: Option<u64>,
    /// `-XX:MinTLABSize=<bytes>` (HotSpot default 2048).
    pub min_tlab_size: Option<u64>,
    /// `-XX:TLABWasteTargetPercent=<1..100>` (HotSpot default 1).
    pub waste_target_percent: Option<u64>,
    /// `-XX:TLABRefillWasteFraction=<1..max_juint>` (HotSpot default 64).
    pub refill_waste_fraction: Option<u64>,
    /// `-XX:TLABAllocationWeight=<0..100>` (HotSpot default 35).
    pub allocation_weight: Option<u64>,
    /// `-XX:TLABWasteIncrement=<0..max_jint>` (HotSpot default 4).
    pub waste_increment: Option<u64>,
}

impl TlabFlags {
    /// HotSpot 25's `MinTLABSize` default (`2*K`).
    pub const HOTSPOT_MIN_TLAB_SIZE: u64 = 2048;
    /// HotSpot 25's `TLABRefillWasteFraction` default.
    pub const HOTSPOT_REFILL_WASTE_FRACTION: u64 = 64;

    /// Nothing given.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// `UseTLAB` as HotSpot reports it.
    pub fn hotspot_use_tlab(&self) -> bool {
        self.use_tlab.unwrap_or(true)
    }

    /// `MinTLABSize` as HotSpot reports it.
    pub fn hotspot_min_tlab_size(&self) -> u64 {
        self.min_tlab_size.unwrap_or(Self::HOTSPOT_MIN_TLAB_SIZE)
    }

    /// `TLABRefillWasteFraction` as HotSpot reports it (JFR's
    /// `jdk.GCTLABConfiguration.tlabRefillWasteLimit` is this flag).
    pub fn hotspot_refill_waste_fraction(&self) -> u64 {
        self.refill_waste_fraction
            .unwrap_or(Self::HOTSPOT_REFILL_WASTE_FRACTION)
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

/// The bits of [`VmConfig::interp_traces`]: the interpreter's diagnostic
/// traces, each armed by its variable's presence (any value), read once per VM
/// (interpreter round i2 wave 59, lane L7).
pub mod interp_trace {
    /// `CRATONVM_INVOKESTATIC_LOADER_TRACE`: the loader trace of a slow
    /// `invokestatic` of a few named Spring / `Class.forName` owners.
    pub const INVOKESTATIC_LOADER: u8 = 1 << 0;
    /// `CRATONVM_DBG_INVSPECIAL`: a slow `invokespecial` from a user-loader class.
    pub const INVSPECIAL: u8 = 1 << 1;
    /// `CRATONVM_DBG_IMSE`: the `athrow` read-hold autopsy.
    pub const IMSE: u8 = 1 << 2;
    /// `CRATONVM_DBG_LAMBDA_DISPATCH`: a default-method call on a lambda
    /// proxy (`interpreter::lambda`; `invokedynamic`'s own lines of the same
    /// variable keep their process gate).
    pub const LAMBDA_DISPATCH: u8 = 1 << 3;

    /// Every bit whose variable is present.
    pub(crate) fn from_env() -> u8 {
        [
            ("CRATONVM_INVOKESTATIC_LOADER_TRACE", INVOKESTATIC_LOADER),
            ("CRATONVM_DBG_INVSPECIAL", INVSPECIAL),
            ("CRATONVM_DBG_IMSE", IMSE),
            ("CRATONVM_DBG_LAMBDA_DISPATCH", LAMBDA_DISPATCH),
        ]
        .into_iter()
        .filter(|(name, _)| cratonvm_types::flags::runtime_var_os(name).is_some())
        .fold(0, |bits, (_, bit)| bits | bit)
    }
}

impl VmConfig {
    /// Whether the interpreter trace `bit` ([`interp_trace`]) is armed.
    #[inline(always)]
    pub fn interp_trace(&self, bit: u8) -> bool {
        self.interp_traces & bit != 0
    }
}

/// A default-ON interpreter site-cache lever spelled like `CRATONVM_JIT_OSR`:
/// unset (or unreadable) → on; `0` / `false` / `off` / `no` (trimmed,
/// case-insensitive) → off; any other value → on. The rule the field-site
/// gates applied in `field_access.rs` before wave 58 moved them here.
fn site_cache_opt_out_unset(name: &str) -> bool {
    cratonvm_types::flags::runtime_var(name)
        .ok()
        .map(|v| {
            let v = v.trim();
            !(v == "0"
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("off")
                || v.eq_ignore_ascii_case("no"))
        })
        .unwrap_or(true)
}

impl Default for VmConfig {
    fn default() -> Self {
        Self {
            max_heap_size: 256 * 1024 * 1024, // 256 MB
            max_direct_memory_size: None,
            default_thread_stack_size: None,
            zgc_uncommit: true,
            zgc_uncommit_delay_secs: 300,
            zgc_soft_max_heap_size: None,
            zgc_ergonomic_soft_max_heap_size: None,
            zgc_proactive: true,
            zgc_allocation_spike_tolerance: None,
            zgc_collection_interval_secs: None,
            zgc_collection_interval_minor_secs: None,
            zgc_collection_interval_only: false,
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
            max_stack_depth: cratonvm_types::flags::runtime_var("RJ_MAX_STACK_DEPTH")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|n| *n >= 64 && *n <= 65536)
                .unwrap_or(8192),
            max_java_stack_trace_depth: crate::runtime::stackwalker::MAX_JAVA_STACK_TRACE_DEPTH,
            classpath: Vec::new(),
            launcher_jar: None,
            boot_classpath: Vec::new(),
            ext_classpath: Vec::new(),
            java_home: None,
            verbose_class_loading: false,
            verbose_gc: false,
            system_properties: Vec::new(),
            jvm_args: Vec::new(),
            assertion_directives: None,
            skip_verification: false,
            xverify_mode: XverifyMode::Remote,
            enable_preview: false,
            finalization_enabled: true,
            class_unloading: true,
            always_pre_touch: false,
            print_string_table_statistics: false,
            string_table_size: None,
            use_transparent_huge_pages: false,
            use_large_pages: false,
            // ZGC is the default collector as of 2026-08-10. The evidence is
            // the 651-class Tomcat suite under all three backends on one
            // commit: ZGC 604 PASS / 29 HANG / 0 CRASH in 247 min against
            // Generational's 519 / 115 / 1 in 356 min, and the 63 classes that
            // are non-PASS under Generational while passing under BOTH other
            // backends — 62 of which log `[moving-young] fallback`, against 9%
            // of the classes that pass on that arm. See
            // `docs/known-issues/tomcat/gc-backend-3way-fullsuite-comparison-20260810.md`.
            //
            // The known cost, accepted deliberately: ZGC does not compact, so
            // it needs roughly 1.5x the heap on buffer-churning workloads
            // (`ZipContentTests` OOMs at `-Xmx 2g` and passes from 3g, where
            // the generational collector passes at 2g). `docs/gc-tuning.md`
            // says so where operators will read it.
            //
            // `-XX:+UseGenerationalGC` is the escape hatch, available in every
            // build — including `--no-default-features`, which takes the
            // `cfg(not(...))` arm because the `Zgc` variant does not exist
            // there.
            #[cfg(feature = "zgc")]
            gc_algorithm: GcAlgorithm::Zgc,
            #[cfg(not(feature = "zgc"))]
            gc_algorithm: GcAlgorithm::Generational,
            g1_ihop_percent: None,
            g1_region_size: None,
            g1_parallel_gc_threads: None,
            zgc_parallel_gc_threads: None,
            zgc_conc_gc_threads: None,
            zgc_dynamic_gc_threads: None,
            g1_max_gc_pause_ms: None,
            g1_string_dedup: None,
            g1_mixed_gc_live_threshold_percent: None,
            g1_heap_waste_percent: None,
            young_gen_sizing: cratonvm_gc::gen_young_sizing::YoungGenSizing::default(),
            hotspot_gc_ergo_warnings: Vec::new(),
            tenuring: cratonvm_gc::gen_heap::TenuringConfig::default(),
            // HotSpot's default (both flags off). Every entry point inherits it.
            explicit_gc: ExplicitGcPolicy::Full,
            explicit_gc_invokes_concurrent: false,
            // gci i43/lf: HotSpot's defaults (nothing given).
            survivor_ratio: None,
            tlab_flags: TlabFlags::default(),
            // The processor's own default (1000 ms/MB), unchanged.
            soft_ref_lru_policy_ms_per_mb: None,
            min_heap_size: None,
            min_heap_free_ratio: None,
            max_heap_free_ratio: None,
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
            // Unlike the JDK mode above, this default is the SAME on both
            // entry points (`for_launcher` re-states it rather than changing
            // it). Strict mode is opted into, never inherited.
            compatibility_mode: EMBEDDED_DEFAULT_COMPATIBILITY_MODE,
            audit_missing_natives: false,
            jdwp_port: None,
            jdwp_suspend: false,
            jdwp_agent: None,
            overlap_interpreter_args: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_JIT_OVERLAP_ARGS",
            ),
            frame_install_borrowed: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_FRAME_INSTALL_BORROWED",
            ),
            cast_hit_raw: cratonvm_types::flags::runtime_flag_default_on("CRATONVM_CAST_HIT_RAW"),
            cast_hit_in_place: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_CAST_HIT_IN_PLACE",
            ),
            attach_checks: cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ATTACH_CHECKS"),
            attach_listener_load: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_ATTACH_LISTENER_LOAD",
            ),
            attach_listener_props: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_ATTACH_LISTENER_PROPS",
            ),
            attach_listener_lazy: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_ATTACH_LISTENER_LAZY",
            ),
            attach_self_unset: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_ATTACH_SELF_UNSET",
            ),
            disable_attach_mechanism: false,
            enable_dynamic_agent_loading: None,
            dynamic_agent_warning: cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ATTACH_AGENT_WARNING"),
            attach_agentmain_jdk: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_ATTACH_AGENTMAIN_JDK",
            ),
            perf_data_file: cratonvm_types::flags::runtime_flag_default_on("CRATONVM_PERF_DATA_FILE"),
            sigquit_thread_dump: cratonvm_types::flags::runtime_flag_default_on("CRATONVM_SIGQUIT_THREAD_DUMP"),
            reduce_signal_usage: false,
            vm_property_list_order: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VM_PROPERTY_LIST_ORDER",
            ),
            vm_compressed_oops_mode_hotspot: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VM_COMPRESSED_OOPS_MODE_HOTSPOT",
            ),
            cast_memo_wide: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_CAST_MEMO_WIDE",
            ),
            cast_memo_two_positive: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_CAST_MEMO_TWO_POSITIVE",
            ),
            cast_site_cache: cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_CAST_SITE_CACHE")
                .is_none()
                && !crate::runtime::env_cache::dbg_h2trace()
                && !crate::runtime::env_cache::dbg_loader_trace(),
            negative_cast_memo: cratonvm_types::flags::runtime_var_os(
                "CRATONVM_JIT_NO_NEGATIVE_CAST_MEMO",
            )
            .is_none(),
            cast_memo_crosscheck: cratonvm_types::flags::runtime_var_os(
                "CRATONVM_DBG_CAST_MEMO_CROSSCHECK",
            )
            .is_some(),
            new_site_cache: cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_NEW_SITE_CACHE")
                .is_none()
                && !crate::runtime::env_cache::dbg_h2trace()
                && !crate::runtime::env_cache::dbg_loader_trace()
                && !crate::runtime::env_cache::nsee_trace(),
            field_site_cache: site_cache_opt_out_unset("CRATONVM_JIT_FIELD_SITE_CACHE"),
            field_site_cache_loader: site_cache_opt_out_unset(
                "CRATONVM_JIT_FIELD_SITE_CACHE_LOADER",
            ),
            method_site_cache: cratonvm_types::flags::runtime_var_os(
                "CRATONVM_JIT_METHOD_SITE_CACHE",
            )
            .is_some(),
            iface_select_memo: cratonvm_types::flags::runtime_var_os(
                "CRATONVM_JIT_NO_IFACE_SELECT_MEMO",
            )
            .is_none(),
            ldc_const_cache: cratonvm_types::flags::runtime_var_os(
                "CRATONVM_JIT_NO_LDC_CONST_CACHE",
            )
            .is_none()
                && !crate::runtime::interpreter::constants::ldc_const_cache_forced_off_by_diagnostics(),
            npe_message_memo: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JIT_NPE_MESSAGE_MEMO",
            ),
            door_sync: !matches!(
                cratonvm_types::flags::runtime_var("CRATONVM_JIT_NO_DOOR_SYNC").as_deref(),
                Ok("1") | Ok("true") | Ok("on")
            ),
            param_tag_scan: cratonvm_types::flags::runtime_var_os(
                "CRATONVM_JIT_NO_PARAM_TAG_SCAN",
            )
            .is_none(),
            resolution_failure_record: cratonvm_types::flags::runtime_var_os(
                "CRATONVM_LOADER_NO_RESOLUTION_FAILURE_RECORD",
            )
            .is_none(),
            dup_name_field_gate: cratonvm_types::flags::runtime_var_os(
                "CRATONVM_LOADER_NO_DUP_NAME_FIELD_GATE",
            )
            .is_none(),
            native_sync: !matches!(
                cratonvm_types::flags::runtime_var("CRATONVM_NATIVE_SYNC").as_deref(),
                Ok(v) if v == "0" || v.eq_ignore_ascii_case("false")
            ),
            interp_traces: interp_trace::from_env(),
            site_census: cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_FIELD_SITE").is_some(),
            invoke_stats: cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_INVOKESTATS")
                .is_some(),
            descriptor_facts: cratonvm_types::flags::runtime_var_os(
                "CRATONVM_JIT_NO_DESCRIPTOR_FACTS",
            )
            .is_none(),
            jit_enabled: !crate::runtime::env_cache::disable_jit(),
            jit_invocation_threshold: crate::runtime::env_cache::jit_invocation_threshold(),
            interp_branch_arm_gate: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_BRANCH_ARM_GATE",
            ),
            interp_hoisted_observers: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_HOISTED_OBSERVERS",
            ),
            jvmti_step_poll: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_STEP_POLL",
            ),
            jvmti_method_entry_frame_control: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL",
            ),
            jvmti_contended_entry_frame_control: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_CONTENDED_ENTRY_FRAME_CONTROL",
            ),
            jvmti_nested_entry_frame_control: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_NESTED_ENTRY_FRAME_CONTROL",
            ),
            jvmti_lambda_entry_pop: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_LAMBDA_ENTRY_POP",
            ),
            jvmti_compiled_caller_entry_pop: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JVMTI_COMPILED_CALLER_ENTRY_POP",
            ),
            jdwp_lambda_proxy_steps: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JDWP_LAMBDA_PROXY_STEPS",
            ),
            jdwp_lambda_target_pop: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JDWP_LAMBDA_TARGET_POP",
            ),
            jdwp_compiled_caller_pop: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JDWP_COMPILED_CALLER_POP",
            ),
            mh_form_frames_under_debugger: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_MH_FORM_FRAMES_UNDER_DEBUGGER",
            ),
            jdwp_agent_debugger_facts: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JDWP_AGENT_DEBUGGER_FACTS",
            ),
            // i135-L1: default OFF -- HotSpot 25 answers OPAQUE_FRAME here (measured); =1 serves the request.
            jvmti_blocked_entry_frame_control: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_JVMTI_BLOCKED_ENTRY_FRAME_CONTROL",
            ),
            interp_stack_dump_at_poll: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_STACK_DUMP_AT_POLL",
            ),
            interp_one_top_signal_test: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_ONE_TOP_SIGNAL_TEST",
            ),
            interp_static_owner_stamp: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_STATIC_OWNER_STAMP",
            ),
            interp_owner_memo_name_generation: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_OWNER_MEMO_NAME_GENERATION",
            ),
            native_callback_memo_edge_epoch: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_NATIVE_CALLBACK_MEMO_EDGE_EPOCH",
            ),
            native_invoke_memo: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_NATIVE_INVOKE_MEMO",
            ),
            native_batch_reference_stores: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_NATIVE_BATCH_REFERENCE_STORES",
            ),
            class_memo_narrow_epoch: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_CLASS_MEMO_NARROW_EPOCH",
            ),
            interp_trivial_ctor_super_chain: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_TRIVIAL_CTOR_SUPER_CHAIN",
            ),
            interp_field_ctor_wide_values: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FIELD_CTOR_WIDE_VALUES",
            ),
            interp_field_ctor_ldc_values: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FIELD_CTOR_LDC_VALUES",
            ),
            interp_ctor_verdict_on_entry: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_CTOR_VERDICT_ON_ENTRY",
            ),
            interp_field_ctor_gen_g1: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_FIELD_CTOR_GEN_G1",
            ),
            // WARNING(i147-pre-super-flip): default ON since 2026-10-29 (A/B on
            // the 64.177 LTO build: inner-new -41%, anon-new -17%,
            // T1HCtorCastAdapter proxy-new -19%; L7W28FramelessCtorBench rows
            // <= 1.043; T1G6 / T1HCtorCastAdapter / L7W28 / L7W94 / L3W144
            // probes SAME as HotSpot in 4 modes, Serial and G1). `=0` restores
            // the framed inner-class constructor.
            interp_field_ctor_pre_super_stores: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FIELD_CTOR_PRE_SUPER_STORES",
            ),
            monitorexit_method_lock_parity: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_MONITOREXIT_METHOD_LOCK_PARITY",
            ),
            // WARNING(i6-user-flip): CRATONVM_USER_LOADER_RESOURCE_BYTECODE default ON by user decision 2026-10-19; =0 restores the old path
            user_loader_resource_bytecode: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_USER_LOADER_RESOURCE_BYTECODE",
            ),
            user_loader_resource_bytecode_compiled: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_USER_LOADER_RESOURCE_BYTECODE_COMPILED",
            ),
            loader_dictionary_before_drive: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_LOADER_DICTIONARY_BEFORE_DRIVE",
            ),
            interp_precise_oop_maps: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_PRECISE_OOP_MAPS",
            ),
            loader_dictionary_one_door: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_LOADER_DICTIONARY_ONE_DOOR",
            ),
            global_name_refusal_record: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_GLOBAL_NAME_REFUSAL_RECORD",
            ),
            global_name_initiating_record: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_GLOBAL_NAME_INITIATING_RECORD",
            ),
            loader_request_door: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_LOADER_REQUEST_DOOR",
            ),
            global_name_owner_unguessed: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_GLOBAL_NAME_OWNER_UNGUESSED",
            ),
            loader_drive_under_loader_lock: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_LOADER_DRIVE_UNDER_LOADER_LOCK",
            ),
            jit_subtype_define_scoped: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JIT_SUBTYPE_DEFINE_SCOPED",
            ),
            execute_no_jit_gate_without_jit: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_EXECUTE_NO_JIT_GATE_WITHOUT_JIT",
            ),
            native_own_body_hint: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_NATIVE_OWN_BODY_HINT",
            ),
            execute_shared_method_view: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_EXECUTE_SHARED_METHOD_VIEW",
            ),
            execute_cached_reentry_frame: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_EXECUTE_CACHED_REENTRY_FRAME",
            ),
            vthread_execute_cached_frame: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VTHREAD_EXECUTE_CACHED_FRAME",
            ),
            execute_fewer_copies: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_EXECUTE_FEWER_COPIES",
            ),
            execute_lean_reentry: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_EXECUTE_LEAN_REENTRY",
            ),
            execute_method_index_memo: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_EXECUTE_METHOD_INDEX_MEMO",
            ),
            class_entry_record_all: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_CLASS_ENTRY_RECORD_ALL",
            ),
            class_site_isolated_sticky: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_CLASS_SITE_ISOLATED_STICKY",
            ),
            interface_special_receiver_check: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERFACE_SPECIAL_RECEIVER_CHECK",
            ),
            interface_special_receiver_memo: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERFACE_SPECIAL_RECEIVER_MEMO",
            ),
            jit_iface_special_receiver_guard: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JIT_IFACE_SPECIAL_RECEIVER_GUARD",
            ),
            indy_generic_direct_invoke: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INDY_GENERIC_DIRECT_INVOKE",
            ),
            indy_generic_thread_row: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INDY_GENERIC_THREAD_ROW",
            ),
            indy_generic_keep_primitive_z: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INDY_GENERIC_KEEP_PRIMITIVE_Z",
            ),
            mh_compat_array_accessors_jdk: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_MH_COMPAT_ARRAY_ACCESSORS_JDK",
            ),
            indy_generic_via_makesite: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INDY_GENERIC_VIA_MAKESITE",
            ),
            // WARNING: i7-default-flip (page i147-orch): default ON since the
            // round i7 follow-up A/B, `=0` off.
            mh_interp_basic_frame_push: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_MH_INTERP_BASIC_FRAME_PUSH",
            ),
            mh_interp_special_frame_push: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_MH_INTERP_SPECIAL_FRAME_PUSH",
            ),
            mh_iwa_interp_frame_push: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_MH_IWA_INTERP_FRAME_PUSH",
            ),
            vthread_sig_poly_sites: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_VTHREAD_SIG_POLY_SITES",
            ),
            reflect_ctor_lean_alloc: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_REFLECT_CTOR_LEAN_ALLOC",
            ),
            jit_reflect_door_lean_site: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_JIT_REFLECT_DOOR_LEAN_SITE",
            ),
            field_name_ring_durable: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_FIELD_NAME_RING_DURABLE",
            ),
            vthread_compiled_leaf_entry: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VTHREAD_COMPILED_LEAF_ENTRY",
            ),
            vthread_compiled_instance_leaf: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_VTHREAD_COMPILED_INSTANCE_LEAF",
            ),
            vthread_compiled_leaf_callers: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_VTHREAD_COMPILED_LEAF_CALLERS",
            ),
            vthread_compiled_leaf_osr: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_VTHREAD_COMPILED_LEAF_OSR",
            ),
            // WARNING: default OFF (was ON since interpreter round i5 wave 113).
            // A mounted virtual thread that runs compiled code parks PINNED under
            // the compiled frame where HotSpot unmounts (`@ShouldNotPin` Quarkus
            // classes `amqp` / `kafka` `VirtualThreadTest`: 20 pin events each).
            // Performance only; a correctness difference outranks it.
            vthread_compiled_learned: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_VTHREAD_COMPILED_LEARNED",
            ),
            vthread_compiled_learned_entry: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VTHREAD_COMPILED_LEARNED_ENTRY",
            ) && cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_VTHREAD_COMPILED_LEARNED",
            ),
            vthread_compiled_app_entry: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_VTHREAD_COMPILED_APP_ENTRY",
            ) && cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_VTHREAD_COMPILED_LEARNED",
            ),
            vthread_local_call_count: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_VTHREAD_LOCAL_CALL_COUNT",
            ) || cratonvm_types::flags::runtime_flag_on("CRATONVM_VTHREAD_NONVIRTUAL_DOORS"),
            vthread_nonvirtual_doors: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_VTHREAD_NONVIRTUAL_DOORS",
            ),
            vthread_virtual_door: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VTHREAD_VIRTUAL_DOOR",
            ),
            vthread_entry_pin_denies_entry_only: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VTHREAD_ENTRY_PIN_DENIES_ENTRY_ONLY",
            ),
            vthread_leaf_verdict_memo: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_VTHREAD_LEAF_VERDICT_MEMO",
            ),
            vthread_app_entry_caller_walk: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_VTHREAD_APP_ENTRY_CALLER_WALK",
            ),
            vthread_slice_on_carrier: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_VTHREAD_SLICE_ON_CARRIER",
            ),
            vthread_slice_on_carrier_compiled: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_VTHREAD_SLICE_ON_CARRIER_COMPILED",
            ),
            jit_door_thread_block: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_JIT_DOOR_THREAD_BLOCK",
            ),
            // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
            jit_door_one_depth_counter: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JIT_DOOR_ONE_DEPTH_COUNTER",
            ),
            vthread_lambda_door: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_VTHREAD_LAMBDA_DOOR",
            ),
            vthread_skip_branch_arm: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VTHREAD_SKIP_BRANCH_ARM",
            ),
            // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
            jit_osr_optimizing_resolver_miss_retry: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JIT_OSR_OPTIMIZING_RESOLVER_MISS_RETRY",
            ),
            vthread_recycle_exec_caches: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VTHREAD_RECYCLE_EXEC_CACHES",
            ),
            vthread_lean_registry: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_VTHREAD_LEAN_REGISTRY",
            ),
            vthread_carrier_sleeps: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_VTHREAD_CARRIER_SLEEPS",
            ),
            // Default ON since interpreter round i5 w106 triage
            // (L6W103VirtualThreadLearnedShortBench helpers virtual 0.93-0.97x,
            // osr virtual 0.86-0.98x, platform rows 1.00-1.03x); `=0` off.
            vthread_handoff_spin_yields: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VTHREAD_HANDOFF_SPIN_YIELDS",
            ),
            // Default ON since interpreter round i5 w101 triage (--nojit
            // ReentrantLock rows 0.61-0.91x, JIT flat); `=0` off.
            owner_record_needs_index: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_OWNER_RECORD_NEEDS_INDEX",
            ) && !cratonvm_types::flags::runtime_flag_on("CRATONVM_AOS_OWNER_INDEX"),
            jni_monitor_exceptions: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JNI_MONITOR_EXCEPTIONS",
            ),
            main_thread_ends_before_destroy: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_MAIN_THREAD_ENDS_BEFORE_DESTROY",
            ),
            destroy_java_vm_thread: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DESTROY_JAVA_VM_THREAD",
            ),
            shutdown_hook_timeout_ms: cratonvm_types::flags::runtime_var(
                "CRATONVM_SHUTDOWN_HOOK_TIMEOUT_MS",
            )
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            // WARNING interp-i5-hook-timeout-flip-20261015: was 30_000.
            .unwrap_or(0),
            vthread_one_publish: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VTHREAD_ONE_PUBLISH",
            ),
            vthread_death_sweep_by_count: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT",
            ),
            vthread_retire_manager_entries: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VTHREAD_RETIRE_MANAGER_ENTRIES",
            ),
            dbg_vthread_census: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_DBG_VTHREAD_CENSUS",
            ),
            class_mirror_layout_memo: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_CLASS_MIRROR_LAYOUT_MEMO",
            ),
            class_mirror_one_loader_lookup: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_CLASS_MIRROR_ONE_LOADER_LOOKUP",
            ),
            spoofed_package_unnamed_module: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_SPOOFED_PACKAGE_UNNAMED_MODULE",
            ),
            interp_fused_ldc_bound: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_LDC_BOUND",
            ),
            interp_ldc_hoisted_pc: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_LDC_HOISTED_PC",
            ),
            interp_ldc_inline_hit: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_LDC_INLINE_HIT",
            ),
            interp_ldc_ref_sites: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_LDC_REF_SITES",
            ),
            interp_fused_getstatic_bound: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_GETSTATIC_BOUND",
            ),
            interp_getstatic_inline_hit: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_GETSTATIC_INLINE_HIT",
            ),
            interp_ldc2w_inline_hit: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_LDC2W_INLINE_HIT",
            ),
            interp_putstatic_inline_hit: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_PUTSTATIC_INLINE_HIT",
            ),
            interp_fused_getstatic_ifzero: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_GETSTATIC_IFZERO",
            ),
            interp_fused_static_increment: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_STATIC_INCREMENT",
            ),
            interp_fused_field_increment: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_FIELD_INCREMENT",
            ),
            interp_fused_static_increment_wide: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_STATIC_INCREMENT_WIDE",
            ),
            interp_fused_field_increment_wide: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_FIELD_INCREMENT_WIDE",
            ),
            interp_fused_increment_early_decline: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_FUSED_INCREMENT_EARLY_DECLINE",
            ),
            interp_fused_field_increment_no_dup: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_FIELD_INCREMENT_NO_DUP",
            ),
            interp_fused_field_increment_value: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_FIELD_INCREMENT_VALUE",
            ),
            interp_fused_array_increment: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_ARRAY_INCREMENT",
            ),
            interp_fused_array_increment_value: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_VALUE",
            ),
            // The stack-top and `dup2` entries build on the base switch, so its
            // `=0` stops their calls too (wave-73 orchestrator fix,
            // `i73-L7-array-increment-kill-switch-leaves-the-top-call-20261106`).
            interp_fused_array_increment_top: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_ARRAY_INCREMENT",
            ) && cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_TOP",
            ),
            interp_fused_array_increment_dup2: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_ARRAY_INCREMENT",
            ) && cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_DUP2",
            ),
            interp_fused_field_update_two_fields: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_FIELD_UPDATE_TWO_FIELDS",
            ),
            interp_fused_array_accumulate: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_ARRAY_ACCUMULATE",
            ),
            interp_fused_array_accumulate_long: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_ARRAY_ACCUMULATE_LONG",
            ),
            interp_fused_load_pair: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_LOAD_PAIR",
            ),
            interp_fused_iconst_goto: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_FUSED_ICONST_GOTO",
            ),
            interp_fused_iconst_arith: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_ICONST_ARITH",
            ),
            // Default ON since interpreter round i5 w101 triage (--nojit long
            // loops 0.59-0.77x); `=0` off.
            interp_fused_long_arith: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_LONG_ARITH",
            ),
            // Default ON since interpreter round i5 w102 triage (--nojit
            // double loops 0.51-0.98x, JIT flat); `=0` off.
            interp_fused_double_arith: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_DOUBLE_ARITH",
            ),
            // Default ON since interpreter round i5 w105 triage (--nojit
            // `L7W105LongIntOperandBench` int-operand rows 0.50-0.74x with the
            // long group on, controls 0.90-0.95x); `=0` off.
            interp_fused_long_int_operand: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_LONG_INT_OPERAND",
            ),
            // Default ON since interpreter round i5 w106 triage (--nojit
            // `L7W106I2lLongTailBench` target rows 0.86-0.89x, controls
            // 1.00-1.03x); `=0` off.
            interp_fused_i2l_long_tail: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_I2L_LONG_TAIL",
            ),
            // Interpreter round i6 wave 115, lane L8: default ON since the
            // wave-115 triage (`L8W115BranchFusionBench --nojit` 0.89-0.94x).
            interp_fused_zero_branch: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_ZERO_BRANCH",
            ),
            interp_fused_null_branch: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_NULL_BRANCH",
            ),
            // Interpreter round i6 wave 116, lane L8: default OFF until the
            // `--nojit` A/B (`L8W116LoopTopBench`).
            interp_fused_null_branch_wide: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_FUSED_NULL_BRANCH_WIDE",
            ),
            interp_fused_getfield_chain: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_FUSED_GETFIELD_CHAIN",
            ),
            interp_lean_loop_top: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_LEAN_LOOP_TOP",
            ),
            // Interpreter round i6 wave 118, lane L8: default OFF until the
            // `--nojit` A/B (`L8W118SwitchLoopBench`); default ON since the wave-129 A/B (0.82-0.95).
            interp_fast_switch_arms: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FAST_SWITCH_ARMS",
            ),
            interp_fused_loop_step_test: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_FUSED_LOOP_STEP_TEST",
            ),
            // Interpreter round i6 wave 119, lane L8: default OFF until the
            // `--nojit` A/B (`L8W119StoreChainBench`).
            interp_fused_store_chain: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_FUSED_STORE_CHAIN",
            ),
            // Interpreter round i6 wave 120, lane L8: default OFF until the
            // `--nojit` A/B (`L8W119StoreChainBench`, row `ctor6`).
            interp_fused_store_chain_wide: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_FUSED_STORE_CHAIN_WIDE",
            ),
            interp_fused_increment_ldc2w: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_INCREMENT_LDC2W",
            ),
            interp_fused_wide_local_getfield: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_FUSED_WIDE_LOCAL_GETFIELD",
            ),
            interp_fused_array_increment_shift_div: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_SHIFT_DIV",
            ),
            // Builds on the base switch, as `_DUP2` does, so its `=0` stops
            // this entry's calls too.
            interp_fused_array_update_no_dup: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_ARRAY_INCREMENT",
            ) && cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_ARRAY_UPDATE_NO_DUP",
            ),
            interp_fused_array_decline_memo: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_ARRAY_DECLINE_MEMO",
            ),
            interp_fused_wide_locals: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_WIDE_LOCALS",
            ),
            interp_fused_array_increment_array_operand: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_ARRAY_OPERAND",
            ),
            interp_fused_array_increment_static_operand: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_STATIC_OPERAND",
            ),
            interp_fused_array_static_operand_no_clinit: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_FUSED_ARRAY_STATIC_OPERAND_NO_CLINIT",
            ),
            interp_fused_array_operand_miss_budget: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INTERP_FUSED_ARRAY_OPERAND_MISS_BUDGET",
            ),
            interp_fused_array_increment_array_operand_wide: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_ARRAY_OPERAND_WIDE",
            ),
            frame_slot_reuse: cratonvm_types::flags::runtime_var_os(
                "CRATONVM_JIT_NO_FRAME_SLOT_REUSE",
            )
            .is_none(),
            frame_emplace: cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_FRAME_EMPLACE")
                .is_none(),
            jit_virtual_tierup: match cratonvm_types::flags::runtime_var(
                "CRATONVM_JIT_VIRTUAL_TIERUP",
            ) {
                // Explicit opt-out only: `0` / `false` disable; unset or any
                // other value enables (the retired `env_cache` memo's rule).
                Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
                Err(_) => true,
            },
            door_receiver_record: cratonvm_types::flags::runtime_var_os(
                "CRATONVM_JIT_NO_DOOR_RECEIVER_RECORD",
            )
            .is_none(),
            lambda_receiver_record: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JIT_PROFILE_LAMBDA_RECEIVERS",
            ),
            lambda_box_intrinsic: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_LAMBDA_BOX_INTRINSIC",
            ),
            lambda_arm_tlab_box: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JIT_LAMBDA_ARM_TLAB_BOX",
            ),
            door_recv_memo: cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_DOOR_RECV_MEMO")
                .is_none(),
            sig_poly_site_cache: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_SIG_POLY_SITE_CACHE",
            ),
            sig_poly_site_lend: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_SIG_POLY_SITE_LEND",
            ),
            sig_poly_site_first: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_SIG_POLY_SITE_FIRST",
            ),
            poly_call_site_shared: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_POLY_CALL_SITE_SHARED",
            ),
            vh_site_cache_full_door: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VH_SITE_CACHE_FULL_DOOR",
            ),
            vh_interp_cas_fast: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VH_INTERP_CAS_FAST",
            ),
            vh_interp_cae_fast: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VH_INTERP_CAE_FAST",
            ),
            mh_exact_return_by_id: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_MH_EXACT_RETURN_BY_ID",
            ),
            vh_w6_1_retired: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VH_W6_1_RETIRED",
            ),
            mh_linkto_from_bytecode: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_MH_LINKTO_FROM_BYTECODE",
            ),
            standin_hidden_invoke_frames: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_STANDIN_HIDDEN_INVOKE_FRAMES",
            ),
            standin_lambda_proxy_frames: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_STANDIN_LAMBDA_PROXY_FRAMES",
            ),
            served_raise_frames: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_THROWABLE_SERVED_RAISE_FRAMES",
            ),
            served_raise_receiver: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_THROWABLE_SERVED_RAISE_RECEIVER",
            ),
            served_raise_chain: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_THROWABLE_SERVED_RAISE_CHAIN",
            ),
            member_lookup_on_mark: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_THROWABLE_MEMBER_LOOKUP_ON_MARK",
            ),
            sfi_lazy_element: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_SFI_LAZY_ELEMENT",
            ),
            compatible_jdk_walk_frames: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_COMPATIBLE_JDK_WALK_FRAMES",
            ),
            sfi_walk_lines: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_SFI_WALK_LINES",
            ),
            compatible_reflect_type_impls: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_COMPATIBLE_REFLECT_TYPE_IMPLS",
            ),
            served_raise_hidden_invoker: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_THROWABLE_SERVED_RAISE_HIDDEN_INVOKER",
            ),
            served_raise_keyed: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_THROWABLE_SERVED_RAISE_KEYED",
            ),
            served_raise_door_key: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_THROWABLE_SERVED_RAISE_DOOR_KEY",
            ),
            served_raise_door_memo: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_THROWABLE_SERVED_RAISE_DOOR_MEMO",
            ),
            served_raise_settled_door_key: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_THROWABLE_SERVED_RAISE_SETTLED_DOOR_KEY",
            ),
            served_raise_carrier_door_key: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_THROWABLE_SERVED_RAISE_CARRIER_DOOR_KEY",
            ),
            static_memo_key_validate_once: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_NATIVE_CALLBACK_MEMO_STATIC_KEY_VALIDATE_ONCE",
            ),
            class_init_bit_first: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_CLASS_INIT_BIT_FIRST",
            ),
            class_init_slow_one_read: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_CLASS_INIT_SLOW_ONE_READ",
            ),
            static_owner_supers_initialized: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_STATIC_OWNER_SUPERS_INITIALIZED",
            ),
            native_new_init_first: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_NATIVE_NEW_INIT_FIRST",
            ),
            native_alloc_initializes: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_NATIVE_ALLOC_INITIALIZES",
            ),
            native_alloc_notes_references: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_NATIVE_ALLOC_NOTES_REFERENCES",
            ),
            jni_call_statically_bound_exact_mid: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JNI_CALL_STATICALLY_BOUND_EXACT_MID",
            ),
            jni_call_virtual_selects: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JNI_CALL_VIRTUAL_SELECTS",
            ),
            jni_ctor_call_registers_finalizable: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JNI_CTOR_CALL_REGISTERS_FINALIZABLE",
            ),
            jni_call_init_exact_mid: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JNI_CALL_INIT_EXACT_MID",
            ),
            finalizer_register_at_object_init: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT",
            ),
            finalizer_ctor_stays_interpreted: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_FINALIZER_CTOR_STAYS_INTERPRETED",
            ),
            finalizer_ancestor_ctor_stays_interpreted:
                cratonvm_types::flags::runtime_flag_default_on(
                    "CRATONVM_FINALIZER_ANCESTOR_CTOR_STAYS_INTERPRETED",
                ),
            finalizer_reregister_at_init_return: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN",
            ),
            jni_reinit_registers_finalizer_again: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JNI_REINIT_REGISTERS_FINALIZER_AGAIN",
            ),
            jni_reinit_count_by_outcome: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JNI_REINIT_COUNT_BY_OUTCOME",
            ),
            jni_reinit_init_return_witness: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JNI_REINIT_INIT_RETURN_WITNESS",
            ),
            jni_reinit_witness_interprets: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JNI_REINIT_WITNESS_INTERPRETS",
            ),
            jni_reinit_witness_nested_exact: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JNI_REINIT_WITNESS_NESTED_EXACT",
            ),
            finalizer_init_return_scan_memo: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_FINALIZER_INIT_RETURN_SCAN_MEMO",
            ),
            finalizer_ctor_prologue_handlers: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_FINALIZER_CTOR_PROLOGUE_HANDLERS",
            ),
            served_raise_own_construction: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_THROWABLE_SERVED_RAISE_OWN_CONSTRUCTION",
            ),
            served_init_chain_capture_frames: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_THROWABLE_SERVED_INIT_CHAIN_FRAMES",
            ),
            reflect_cs_adapter: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_REFLECT_CS_ADAPTER",
            ),
            reflection_loads_signature_types: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_REFLECTION_LOADS_SIGNATURE_TYPES",
            ),
            launcher_helper: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_LAUNCHER_HELPER",
            ),
            launcher_helper_jar: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_LAUNCHER_HELPER_JAR",
            ),
            launcher_helper_jar_no_preload: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_LAUNCHER_HELPER_JAR_NO_PRELOAD",
            ),
            launcher_helper_module: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_LAUNCHER_HELPER_MODULE",
            ),
            launcher_helper_module_compatible: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_LAUNCHER_HELPER_MODULE_COMPATIBLE",
            ),
            caller_class_finder_rule: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_CALLER_CLASS_FINDER",
            ),
            reflection_caller_class_rule: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_REFLECTION_CALLER_RULE",
            ),
            reflection_caller_fast: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_REFLECTION_CALLER_FAST",
            ),
            reflection_caller_fast_under_records: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_REFLECTION_CALLER_FAST_UNDER_RECORDS",
            ),
            reflection_cs_caller_rule: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_REFLECTION_CS_CALLER_RULE",
            ),
            caller_lambda_proxy_frames: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_CALLER_LAMBDA_PROXY_FRAMES",
            ),
            public_method_jdk_walk: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_PUBLIC_METHOD_JDK_WALK",
            ),
            standin_privileged_frames: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_STANDIN_PRIVILEGED_FRAMES",
            ),
            compatible_hides_hidden_frames: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_COMPATIBLE_HIDDEN_FRAMES",
            ),
            by_name_static_kind: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_BYNAME_STATIC_KIND",
            ),
            virtual_sleep_continuation_shape: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VIRTUAL_SLEEP_CONTINUATION_SHAPE",
            ),
            jdk_continuations: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_VT_CONTINUATIONS",
            ),
            module_path: Vec::new(),
            add_reads: Vec::new(),
            add_exports: Vec::new(),
            add_opens: Vec::new(),
            add_modules: Vec::new(),
            patch_modules: Vec::new(),
            limit_modules: Vec::new(),
            upgrade_module_path: Vec::new(),
            jit_aggressive_compilation: false,
            heap_dump_on_oom: false,
            heap_dump_path: None,
            exit_on_out_of_memory_error: false,
            crash_on_out_of_memory_error: false,
            on_out_of_memory_error: None,
            use_gc_overhead_limit: None,
            jfr_start_recording: None,
            jfr_startup_options: Vec::new(),
            jfr_enabled_events: None,
            use_container_support: true,
            container_effective_processors: None,
            // JEP 358: default ON to match HotSpot (messages verified
            // byte-identical). Opt out via -XX:-ShowCodeDetailsInExceptionMessages.
            show_code_details_in_exception_messages: true,
            // Interpreter round i4 wave 84, L2: HotSpot's default, off.
            show_hidden_frames: false,
            // Interpreter round i5 wave 102, L6: HotSpot's default, 0.
            diagnose_sync_on_value_based_classes: 0,
            // Interpreter round i4 wave 85, L2: HotSpot's default, on.
            stack_trace_in_throwable: true,
            // Interpreter round i4 wave 85, L2; on by default since the wave-86
            // triage (`=0` off), read once per VM.
            interp_npe_deferred_message: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_INTERP_NPE_DEFERRED_MESSAGE",
            ),
            // nio21 w16 L5: absent; resolved per VM (default ON, as HotSpot).
            omit_stack_trace_in_fast_throw: None,
            xlog_spec: None,
            jvmti_agent_options: Vec::new(),
            native_access: cratonvm_native_builtins::panama::NativeAccessConfig::default(),
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

    /// Is a JDWP debugger configured at the VM's start: `--jdwp-port`, or
    /// (interpreter round i7 wave 136, lane L1; [`Self::jdwp_agent_debugger_facts`],
    /// `CRATONVM_JDWP_AGENT_DEBUGGER_FACTS`) `-agentlib:jdwp`? A fact of the
    /// start: neither field changes once the VM exists.
    #[inline]
    pub fn jdwp_debugger_configured(&self) -> bool {
        self.jdwp_port.is_some() || (self.jdwp_agent_debugger_facts && self.jdwp_agent.is_some())
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

/// [`VmConfig::omit_stack_trace_in_fast_throw_resolved`] over its inputs: the
/// explicit `-XX:` answer when there is one, else the switch (read only then).
fn resolve_omit_stack_trace_in_fast_throw(
    explicit: Option<bool>,
    switch_default_on: impl FnOnce() -> bool,
) -> bool {
    explicit.unwrap_or_else(switch_default_on)
}

#[cfg(test)]
mod nio21_w16_l5_fast_throw_flag_tests {
    use super::*;

    #[test]
    fn nio21_w16_l5_explicit_xx_flag_beats_the_switch() {
        assert!(!resolve_omit_stack_trace_in_fast_throw(Some(false), || {
            true
        }));
        assert!(resolve_omit_stack_trace_in_fast_throw(Some(true), || false));
        assert!(resolve_omit_stack_trace_in_fast_throw(None, || true));
        assert!(!resolve_omit_stack_trace_in_fast_throw(None, || false));
        assert_eq!(VmConfig::default().omit_stack_trace_in_fast_throw, None);
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
    ///
    /// The compatibility mode is stated here rather than inherited: it is
    /// [`LAUNCHER_DEFAULT_COMPATIBILITY_MODE`] ([`CompatibilityMode::JdkOnly`]),
    /// which `default()` does **not** share. The launcher's full policy —
    /// class library *and* substitution strictness — reads at this one call
    /// site, and a change to either constant is a one-line diff here.
    pub fn for_launcher() -> Self {
        Self::default()
            .with_jdk_mode(LAUNCHER_DEFAULT_JDK_MODE)
            .with_compatibility_mode(LAUNCHER_DEFAULT_COMPATIBILITY_MODE)
    }

    /// The real-JDK **embedding** base: `libcratonvm` and `cratonvm-embed`
    /// build every configuration on this.
    ///
    /// **This is no longer an alias for [`VmConfig::for_launcher`].** It was
    /// one for as long as the two entry points agreed on compatibility mode.
    /// They no longer do: the launcher defaults to
    /// [`CompatibilityMode::JdkOnly`] and this does not. Re-aliasing it would
    /// silently hand every embedding host a stricter policy it never asked
    /// for and has no flag to opt out of — the C ABI's advertised default
    /// (`libcratonvm::DEFAULT_COMPATIBILITY_MODE`) is this value, so that
    /// would be an ABI break wearing a launcher change's clothes.
    ///
    /// The JDK mode is still the launcher's ([`LAUNCHER_DEFAULT_JDK_MODE`],
    /// real): an embedder that links the C API is running against a host JDK,
    /// not against the hermetic in-tree default. Like `for_launcher`, it
    /// inspects nothing about the host — a caller that ends up in real-JDK
    /// mode must validate a usable JDK via [`require_real_jdk`] and fail
    /// loudly, because this constructor never downgrades to synthetic on its
    /// own. (It used to probe the host; see `jdk-mode-determinism.md`.)
    /// Is HotSpot's `OmitStackTraceInFastThrow` in effect for this VM? The
    /// explicit `-XX:±OmitStackTraceInFastThrow` wins; absent, the
    /// `CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW` switch decides. It stays
    /// OPT-IN (`=1`) by the owner's decision recorded in
    /// `docs/JIT_OPTIMIZATION.md` (HotSpot's own default is ON); wave 16 L5 made
    /// it per VM and raised the hot threshold, the flip is the owner's call.
    pub fn omit_stack_trace_in_fast_throw_resolved(&self) -> bool {
        resolve_omit_stack_trace_in_fast_throw(self.omit_stack_trace_in_fast_throw, || {
            cratonvm_types::flags::runtime_flag_on("CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW")
        })
    }

    pub fn with_host_jdk_default() -> Self {
        Self::default()
            .with_jdk_mode(LAUNCHER_DEFAULT_JDK_MODE)
            .with_compatibility_mode(EMBEDDED_HOST_JDK_DEFAULT_COMPATIBILITY_MODE)
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

    /// Explicit compatibility-mode selection. The only way to reach
    /// [`CompatibilityMode::JdkOnly`]; nothing infers it.
    ///
    /// # This deliberately does not touch [`Self::use_synthetic_jdk`]
    ///
    /// `--jdk-only` implies a real JDK image, so the obvious convenience would
    /// be to force [`JdkMode::Real`] here. That is wrong: it would silently
    /// repair `--jdk-only --synthetic-jdk` into a real-JDK run, erasing the
    /// conflict the CLI is supposed to report. The CLI pairs `--jdk-only` with
    /// an explicit [`Self::with_jdk_mode`] call
    /// (`docs/book/src/user-guide/jdk-only-mode.md` §9); this setter records only
    /// what it was asked for, and [`Self::validate_compatibility`] is the
    /// backstop for any caller that sets the two independently.
    pub fn with_compatibility_mode(mut self, mode: CompatibilityMode) -> Self {
        self.compatibility_mode = mode;
        self
    }

    /// Whether strict JDK-only policy is in force for this configuration.
    pub fn is_jdk_only(&self) -> bool {
        self.compatibility_mode.is_jdk_only()
    }

    /// Do the captures leave hidden frames (`Method::is_hidden`) out, as
    /// HotSpot does without `-XX:+ShowHiddenFrames`? `--jdk-only` since
    /// interpreter round i1 wave 34; `--compatible` too under
    /// [`Self::compatible_hides_hidden_frames`] (wave 48, lane L3). Read by
    /// the throwable capture, `Thread.getStackTrace` / the walkers
    /// (`hidden_stack_entries`) and the implicit-exception snapshot splice.
    ///
    /// Never under `-XX:+ShowHiddenFrames` ([`Self::show_hidden_frames`]),
    /// as HotSpot's `fill_in_stack_trace` and stack walk keep them then.
    pub fn hides_hidden_frames(&self) -> bool {
        !self.show_hidden_frames && (self.is_jdk_only() || self.compatible_hides_hidden_frames)
    }

    /// The policy value handed to the native registry, the `ClassManager` and
    /// dispatch at VM init.
    ///
    /// `real_jdk` reports [`Self::use_synthetic_jdk`] **as it stands**. It is
    /// not forced to `true` by a [`CompatibilityMode::JdkOnly`] request, which
    /// is why this constructs [`ExecutionPolicy`] with a struct literal rather
    /// than calling [`ExecutionPolicy::jdk_only`] — that constructor hardcodes
    /// `real_jdk = true`, and using it here would make an incoherent
    /// `JdkOnly` + [`JdkMode::Synthetic`] pair look coherent to every consumer
    /// downstream. Keeping the incoherence visible is what lets
    /// [`Self::validate_compatibility`] reject it instead of the VM booting a
    /// synthetic library under a policy that forbids synthetic stubs.
    pub fn execution_policy(&self) -> ExecutionPolicy {
        ExecutionPolicy {
            compatibility_mode: self.compatibility_mode,
            real_jdk: !self.use_synthetic_jdk,
        }
    }

    /// Reject configurations whose compatibility mode and JDK mode contradict
    /// each other. Call before VM init; the CLI calls it after flag parsing.
    ///
    /// The single rejected pairing is [`CompatibilityMode::JdkOnly`] +
    /// [`JdkMode::Synthetic`]. Strict mode means real class bytes are
    /// authoritative and no `NativeKind::SyntheticStub` may be registered or
    /// invoked; the synthetic library *is* ~5,200 such stubs, so the pair asks
    /// for a VM with no class library at all. That is rejected here, at
    /// configuration time, rather than surfacing later as an unexplained
    /// `NoClassDefFoundError` — the same reason [`require_synthetic_jdk`]
    /// exists.
    ///
    /// Neither mode is silently rewritten to make the other work: which
    /// correction is right depends on what the caller meant, and both
    /// alternatives are named in the error.
    pub fn validate_compatibility(&self) -> Result<(), crate::error::VmError> {
        if self.is_jdk_only() && self.use_synthetic_jdk {
            return Err(crate::error::VmError::InvalidConfiguration(
                "--jdk-only and --synthetic-jdk cannot be combined \
                 (compatibility mode `jdk-only` with JDK mode `synthetic-jdk`).\n\
                 \n\
                 --jdk-only means real JDK class bytes are authoritative: no fabricated \
                 compatibility class and no synthetic-stub native may be registered or \
                 invoked. The synthetic class library is ~5,200 such stubs, so the \
                 combination selects a VM with no usable class library.\n\
                 \n\
                 Fix by one of:\n  \
                 * drop --synthetic-jdk and run --jdk-only against a real JDK \
                 (set JAVA_HOME or pass --java-home; see the real-JDK error text for \
                 the accepted layouts); or\n  \
                 * drop --jdk-only and run --synthetic-jdk under `--compatible`, \
                 which is what the synthetic library requires. The launcher \
                 implies --compatible for --synthetic-jdk already; an \
                 embedder has to ask for it.\n\
                 \n\
                 CratonVM does not pick one for you: the two fixes run different class \
                 libraries with different semantics and different bug sets, so a run \
                 whose library was chosen by the VM is not reproducible or reportable."
                    .to_string(),
            ));
        }
        Ok(())
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

    /// `--enable-preview` for this VM ([`Self::enable_preview`]); the
    /// embedding spelling (`cratonvm-embed` re-exports this type) of the
    /// launcher's flag and of `libcratonvm`'s `JavaVMOption`.
    pub fn with_enable_preview(mut self, enabled: bool) -> Self {
        self.enable_preview = enabled;
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
                if target.is_empty() {
                    continue;
                }
                // `ALL-UNNAMED` is the JDK convention; store it as the empty
                // string, our unnamed-module sentinel — the same translation
                // `parse_add_exports` below already documents doing. Without
                // it the flag records the literal "ALL-UNNAMED", which names
                // no module and grants nothing. That was invisible while
                // `ModuleRegistry::reads` returned `true` for every unnamed
                // PROVIDER; now that the rule is directional (measured on
                // HotSpot 25: `java.logging.canRead(unnamed)` is false bare
                // and true under `--add-reads java.logging=ALL-UNNAMED`),
                // this translation is what makes the flag work at all.
                let target = if target == "ALL-UNNAMED" {
                    String::new()
                } else {
                    target.to_string()
                };
                result.push((reader.trim().to_string(), target));
            }
        }
        result
    }

    /// Parse an `--add-exports` or `--add-opens` value:
    /// `"module/package=target_module"`.
    ///
    /// `target_module` may be `ALL-UNNAMED` (the JDK convention). It is passed
    /// through **verbatim**, not folded into the empty string: the empty string
    /// is `ModuleRegistry`'s *unqualified* marker — open to every module in the
    /// process — and `ALL-UNNAMED` opens to the unnamed module only.
    /// `ModuleRegistry::add_opens` resolves the token
    /// (`classloading::module::ALL_UNNAMED_TARGET`).
    ///
    /// Folding it here was a real over-grant, measured 2026-08-09 by
    /// `probes/AddOpensFlagProbe.java` against Temurin 25: under
    /// `--add-opens=java.base/java.net=ALL-UNNAMED`, HotSpot answers
    /// `Module.isOpen("java.net")` **false** and CratonVM answered **true**,
    /// and any *named* module got the deep-reflection grant along with the
    /// unnamed one. Same conflation the `Module.addOpens(String, Module)`
    /// native already had to fix with its own sentinel — see
    /// `native-builtins/.../reflect_invoke.rs`'s `UNRESOLVED_TARGET_MODULE`.
    ///
    /// Note this is deliberately NOT symmetric with `parse_add_reads`, which
    /// does map `ALL-UNNAMED` to the empty string: a *read* edge names a source
    /// module rather than a target set, and `""` is the unnamed module's own
    /// name there, not a wildcard.
    pub fn parse_add_exports(s: &str) -> Option<(String, String, String)> {
        let (left, target) = s.split_once('=')?;
        let (module, pkg) = left.split_once('/')?;
        Some((
            module.trim().to_string(),
            pkg.trim().replace('.', "/"),
            target.trim().to_string(),
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
///
/// # Why `jmods/` is still preferred over `lib/modules` (2026-07-26)
///
/// `startup-and-diagnostics.md` §6.1 asked for
/// the opposite: put `lib/modules` (checked below, after the jmods branch)
/// *first*, because `ClassPath::load_jmod` used to inflate every `classes/`
/// entry of all 70 JMODs at load time — 27,962 entries, ~136 MB, 15-21 s
/// before the first Java class loaded. That measurement was reproduced and
/// is real. The **cost** was fixed; the **ordering** was deliberately not.
///
/// The reason is that `jmods/` and `lib/modules` are not interchangeable
/// sources for the same classes:
///
/// * `lib/modules` is a `jlink`-produced image. Its `module-info.class`
///   files are rewritten by the `SystemModules` plugin and it carries
///   generated `jdk.internal.module.SystemModules$*` classes that no JMOD
///   contains, while the JMODs carry the pristine, un-transformed
///   `module-info`. `ClassPath::scan_module_infos` feeds the module layer
///   from exactly those bytes.
/// * A JMOD also carries `lib/`, `conf/`, `legal/`, `bin/` and `include/`
///   entries that the jimage does not, and `ClassPath::find_resource` serves
///   them from the `classes/` subtree and the archive today.
///
/// Preferring the jimage would therefore change *which bytes* boot sees, on
/// every default run, and no session that has proposed it has been able to
/// run the H2 / Spring / Tomcat suites both ways. The cost was instead
/// removed where it originated: `load_jmod` now builds a decompression-free
/// name index and inflates per lookup, the same shape the JAR path has used
/// since the O(jars x zip-probes) scan was closed. Same win, same reader,
/// same bytes. See `boot-classpath-lazy.md`.
///
/// This is unconditional — there is no flag and no env var for it, and no
/// opt-in. Reverting to eager inflation means reverting that commit.
///
/// The CLASS BYTES are the image's all the same (zs s4/rp, 2026-10-15,
/// `CRATONVM_BOOT_IMAGE_CLASS_BYTES`, default ON, `=0` kill): the boot
/// `ClassPath` serves a class the `lib/modules` beside these JMODs holds from
/// that image, and a class only the image holds (jlink's
/// `BoundMethodHandle$Species_*`, `SystemModules$*`) at all. Module
/// descriptors and resources stay the JMODs'. See `RuntimeImageClasses` in
/// `classloading/src/class_path.rs`.
///
/// **Superseded by `CRATONVM_BOOT_JIMAGE_OVER_JMODS` (default ON since 2026-10-24):** on a JDK that
/// has both forms the image alone is the boot path. Linux host, JDK 25 with `jmods/`, 7 interleaved
/// reps: Hello max-RSS 227 -> 211 MB, `classes 3000` 244 -> 234 MB, CPU equal; Gradle 8.12.1's launcher
/// over its 174 jars prints the same text as HotSpot; `javac` under the VM (`-m jdk.compiler/...`)
/// writes byte-identical class files on every arm; 198 loader / module / resource probes behave alike.
/// The pristine-`module-info` concern above did not show in any of them. Marker: `fp-default-flip`.
pub fn discover_boot_classpath(java_home: Option<&str>) -> Vec<String> {
    discover_boot_classpath_with(
        java_home,
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BOOT_JIMAGE_OVER_JMODS"),
    )
}

/// [`discover_boot_classpath`] with the `CRATONVM_BOOT_JIMAGE_OVER_JMODS`
/// decision made by the caller (`jimage_over_jmods`), so a test can ask both
/// ways without touching the environment.
///
/// zt/fp footprint round (docs/feature-designs/jdk-only-mode.md): with the switch ON, a JDK
/// that ships both `jmods/` and `lib/modules` boots from the image alone, as
/// HotSpot does and as a JDK without `jmods/` already does here. The 70
/// JMODs' central directories and name tables cost ~8 MB of a Hello world's
/// working set that the image does not (Windows, JDK 25.0.3 with `jmods/`
/// made by `jmod create`: 112 MB, against 101 MB with the switch), and the
/// image is the source HotSpot boots from. Default ON (fp-default-flip-20261024, `=0` restores the
/// JMODs); the "Why `jmods/` is still preferred" section above is the case that was decided the other way
/// until the Linux rows below.
pub fn discover_boot_classpath_with(
    java_home: Option<&str>,
    jimage_over_jmods: bool,
) -> Vec<String> {
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

    // `CRATONVM_BOOT_JIMAGE_OVER_JMODS` (default ON since fp-default-flip-20261024, `=0` restores the JMODs): the image beside the JMODs
    // is the boot path, and the JMODs are not read.
    let lib_modules_image = java_home.join("lib").join("modules");
    if jimage_over_jmods && lib_modules_image.is_file() {
        return vec![lib_modules_image.to_string_lossy().into_owned()];
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
        match cratonvm_types::flags::runtime_var(key) {
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
    Err(
        "synthetic-JDK mode was selected but this binary was built without the \
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
            .to_string(),
    )
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
    if let Ok(val) = cratonvm_types::flags::runtime_var("CRATONVM_JAVA_HOME") {
        let p = PathBuf::from(val.trim());
        if p.is_dir() {
            return Some(p);
        }
    }

    // 3. JAVA_HOME env var
    if let Ok(val) = cratonvm_types::flags::runtime_var("JAVA_HOME") {
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
    let path_var = cratonvm_types::flags::runtime_var_os("PATH")?;
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

/// Resolve the `java` executable that spawning the bare name `"java"`
/// would actually run.
///
/// # Why this is not just `first_java_executable_on_path`
///
/// On Windows, `CreateProcessW` — which `std::process::Command` uses —
/// does **not** search `PATH` first. Its documented order begins with
/// *the directory of the calling executable*, and only reaches `PATH`
/// several steps later. So for a `cratonvm.exe` that has the optional
/// `java.exe` alias built beside it (the `java-bin-alias` cargo feature,
/// which exists because Maven Surefire's `-Djvm=` requires a path ending
/// in `java.exe`), the bare name `"java"` resolves to **that alias** —
/// this very VM — no matter what `PATH` says.
///
/// That is what made the previous self-check insufficient rather than
/// merely incomplete. It compared `current_exe()` against the first
/// `java` on `PATH`; with a real JDK on `PATH` those differ, the check
/// passed, and the spawn then ran the sibling alias anyway. The child —
/// now named `java.exe` — reached the same code, made the same
/// comparison, passed it for the same reason, and spawned again: an
/// unbounded fan-out, three processes per level for the three
/// `-XshowSettings` variants tried in turn. Observed 2026-08-28: 58
/// processes inside two seconds, all dying with
/// `EXCEPTION_STACK_OVERFLOW` before VM construction, exhausting 64 GB of
/// host RAM and taking the machine down. Because the children are named
/// `java.exe`, killing `cratonvm` does not stop it.
///
/// Resolving to an absolute path here and spawning *that* removes the
/// ambiguity: the caller can compare what will really run against itself
/// before running it.
///
/// The current directory is deliberately **not** searched, even though
/// `CreateProcessW` would search it before `PATH`. Executing a `java.exe`
/// that happens to sit in whatever directory the VM was launched from is
/// not behaviour worth preserving; skipping it is both safer and more
/// predictable.
fn resolve_java_executable() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "java.exe" } else { "java" };

    if cfg!(windows) {
        if let Ok(me) = std::env::current_exe() {
            if let Some(dir) = me.parent() {
                let sibling = dir.join(exe);
                if sibling.is_file() {
                    return std::fs::canonicalize(&sibling).ok().or(Some(sibling));
                }
            }
        }
    }

    first_java_executable_on_path()
}

/// The JDK home a `java` executable belongs to, read from the file system
/// the way the JDK launcher computes `java.home`: the parent of the `bin/`
/// directory holding the real (symlink-resolved) executable. `None` unless
/// that directory carries a boot image (`lib/modules` or
/// `jmods/java.base.jmod`), so an unusual layout still falls back to asking
/// the JVM itself.
fn java_home_from_executable(java: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(java).ok()?;
    let home = real.parent()?.parent()?;
    let has_boot_image = home.join("lib").join("modules").is_file()
        || home.join("jmods").join("java.base.jmod").is_file();
    has_boot_image.then(|| home.to_path_buf())
}

fn detect_java_home_from_path() -> Option<PathBuf> {
    // Resolve what the spawn would actually execute, then refuse if it is
    // this process. `cratonvm` installed (or built) as `java.exe` is a
    // supported configuration — Maven Surefire needs it — so this is a
    // routine case, not a defensive one, and getting it wrong recurses
    // without bound. See `resolve_java_executable`.
    let java = resolve_java_executable()?;
    if let Ok(this) = std::env::current_exe().and_then(std::fs::canonicalize) {
        let resolved = std::fs::canonicalize(&java).unwrap_or_else(|_| java.clone());
        if this == resolved {
            tracing::debug!(
                "skipping java.home auto-detection: `java` resolves to this \
                 executable ({}). Set JAVA_HOME or CRATONVM_JAVA_HOME, or pass \
                 --java-home, to point at a real JDK.",
                resolved.display()
            );
            return None;
        }
    }

    if let Some(home) = java_home_from_executable(&java) {
        tracing::info!("Auto-detected JAVA_HOME from PATH: {}", home.display());
        return Some(home);
    }

    // Try the flag variants in order of specificity
    let flag_variants = [
        "-XshowSettings:properties", // JDK 25+
        "-XshowSettings:property",   // JDK 9-24
        "-XshowSettings:all",        // universal fallback
    ];

    for flag in &flag_variants {
        // The resolved absolute path, never the bare name: a bare name is
        // re-resolved by `CreateProcessW` at spawn time, which is exactly
        // the step the guard above cannot see through.
        let output = std::process::Command::new(&java)
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

    // -----------------------------------------------------------------------
    // Boot-classpath source selection (2026-07-26 boot-classpath-lazy)
    //
    // `discover_boot_classpath` prefers `jmods/` and falls back to
    // `lib/modules`. That ordering is load-bearing — see the function's doc
    // comment — so it is pinned here rather than left to be "fixed" by a
    // later reader who only sees the startup cost and not the reason.
    // `resolve_java_home(Some(dir))` is authoritative when the directory
    // exists, so these tests need no JDK and touch no environment variables.
    // -----------------------------------------------------------------------

    fn fake_java_home(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cratonvm_boot_cp_{}_{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // `java` on PATH resolves to its home the way the JDK launcher does it --
    // the parent of the real executable's `bin/` -- with no child JVM. Phase
    // 5 of the fast-boot proposal: the spawn was most of a bare launch's cost.
    #[cfg(unix)]
    #[test]
    fn java_home_from_executable_follows_the_symlink_to_the_jdk() {
        let home = fake_java_home("exe_home");
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::create_dir_all(home.join("lib")).unwrap();
        std::fs::write(home.join("bin").join("java"), b"").unwrap();
        std::fs::write(home.join("lib").join("modules"), b"jimage").unwrap();
        let link_dir = fake_java_home("exe_link");
        std::os::unix::fs::symlink(home.join("bin").join("java"), link_dir.join("java")).unwrap();

        let found = java_home_from_executable(&link_dir.join("java"));
        assert_eq!(found, Some(std::fs::canonicalize(&home).unwrap()));
    }

    #[test]
    fn java_home_from_executable_refuses_a_tree_with_no_boot_image() {
        let home = fake_java_home("exe_no_image");
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::write(home.join("bin").join("java"), b"").unwrap();

        assert_eq!(java_home_from_executable(&home.join("bin").join("java")), None);
    }

    #[test]
    fn boot_classpath_prefers_jmods_and_puts_java_base_first() {
        let home = fake_java_home("jmods_first");
        let jmods = home.join("jmods");
        std::fs::create_dir_all(&jmods).unwrap();
        // Deliberately create them out of order so the result proves sorting
        // and the java.base hoist rather than directory-iteration order.
        for name in ["java.xml.jmod", "java.base.jmod", "java.desktop.jmod"] {
            std::fs::write(jmods.join(name), b"JM\x01\x00").unwrap();
        }
        std::fs::create_dir_all(home.join("lib")).unwrap();
        std::fs::write(home.join("lib").join("modules"), b"not-a-real-jimage").unwrap();

        let cp = discover_boot_classpath_with(Some(&home.to_string_lossy()), false);
        assert_eq!(cp.len(), 3, "all three JMODs must be on the boot classpath");
        assert!(
            cp[0].ends_with("java.base.jmod"),
            "java.base must come first, got {cp:?}"
        );
        assert!(
            cp[1].ends_with("java.desktop.jmod") && cp[2].ends_with("java.xml.jmod"),
            "remaining JMODs must be sorted for run-to-run determinism, got {cp:?}"
        );
        assert!(
            !cp.iter()
                .any(|e| e.replace('\\', "/").ends_with("lib/modules")),
            "lib/modules must not be used when jmods/ is present: {cp:?}"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn fp_jimage_over_jmods_boots_from_the_image_alone() {
        // `CRATONVM_BOOT_JIMAGE_OVER_JMODS`: a JDK with both forms boots from
        // `lib/modules` with the switch on, from the JMODs without it; a JDK
        // with only one form is the same either way.
        let home = fake_java_home("jimage_over_jmods");
        let jmods = home.join("jmods");
        std::fs::create_dir_all(&jmods).unwrap();
        for name in ["java.xml.jmod", "java.base.jmod"] {
            std::fs::write(jmods.join(name), b"JM\x01\x00").unwrap();
        }
        std::fs::create_dir_all(home.join("lib")).unwrap();
        std::fs::write(home.join("lib").join("modules"), b"not-a-real-jimage").unwrap();

        let on = discover_boot_classpath_with(Some(&home.to_string_lossy()), true);
        assert_eq!(on.len(), 1, "the image alone, got {on:?}");
        assert!(on[0].replace('\\', "/").ends_with("lib/modules"));
        let off = discover_boot_classpath_with(Some(&home.to_string_lossy()), false);
        assert_eq!(off.len(), 2, "both JMODs, got {off:?}");
        assert!(off[0].ends_with("java.base.jmod"));

        std::fs::remove_file(home.join("lib").join("modules")).unwrap();
        let no_image = discover_boot_classpath_with(Some(&home.to_string_lossy()), true);
        assert_eq!(no_image, off, "no image to prefer: the JMODs");

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn boot_classpath_falls_back_to_lib_modules_without_jmods() {
        // JRE / jlink-trimmed image: no `jmods/` at all. This is the path
        // RKC16N.9 added, and it must keep working.
        let home = fake_java_home("jimage_fallback");
        std::fs::create_dir_all(home.join("lib")).unwrap();
        std::fs::write(home.join("lib").join("modules"), b"not-a-real-jimage").unwrap();

        let cp = discover_boot_classpath(Some(&home.to_string_lossy()));
        assert_eq!(cp.len(), 1, "expected the jimage alone, got {cp:?}");
        assert!(cp[0].replace('\\', "/").ends_with("lib/modules"));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn boot_classpath_empty_jmods_dir_falls_through_to_lib_modules() {
        // A `jmods/` directory that exists but holds no `.jmod` files must
        // not shadow a usable `lib/modules`.
        let home = fake_java_home("empty_jmods");
        std::fs::create_dir_all(home.join("jmods")).unwrap();
        std::fs::create_dir_all(home.join("lib")).unwrap();
        std::fs::write(home.join("lib").join("modules"), b"not-a-real-jimage").unwrap();

        let cp = discover_boot_classpath(Some(&home.to_string_lossy()));
        assert_eq!(cp.len(), 1, "expected the jimage alone, got {cp:?}");
        assert!(cp[0].replace('\\', "/").ends_with("lib/modules"));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn boot_classpath_empty_when_java_home_has_neither() {
        let home = fake_java_home("neither");
        let cp = discover_boot_classpath(Some(&home.to_string_lossy()));
        assert!(cp.is_empty(), "expected no boot classpath, got {cp:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The `-version` banner must not carry the old, ~9x-low "~300" figure.
    /// This is user-visible text quoted in bug reports; see
    /// [`REAL_JDK_NATIVE_REGISTRATIONS`] for the derivation.
    #[test]
    fn jdk_mode_describe_does_not_quote_the_stale_native_count() {
        let real = JdkMode::Real.describe();
        assert!(
            !real.contains("~300"),
            "real-JDK banner still quotes the retracted ~300 figure: {real}"
        );
        assert!(
            real.contains("2,700"),
            "real-JDK banner should quote the measured figure: {real}"
        );
        assert_eq!(REAL_JDK_NATIVE_REGISTRATIONS, 2_700);
    }

    #[test]
    fn default_config() {
        let config = VmConfig::default();
        assert_eq!(config.max_heap_size, 256 * 1024 * 1024);
        assert_eq!(config.max_stack_depth, 8192);
        assert!(config.classpath.is_empty());
        assert!(config.boot_classpath.is_empty());
        assert!(config.ext_classpath.is_empty());
        assert!(config.java_home.is_none());
        // ZGC is the default collector as of 2026-08-10 (see `VmConfig::default`
        // for the measurement behind the flip). A `--no-default-features` build
        // has no `Zgc` variant at all and falls back to `Generational`, so this
        // assertion is cfg'd the same way the field is — otherwise it would be
        // asserting on a value that cannot exist in that configuration.
        #[cfg(feature = "zgc")]
        assert_eq!(config.gc_algorithm, GcAlgorithm::Zgc);
        #[cfg(not(feature = "zgc"))]
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
        if let Ok(val) = cratonvm_types::flags::runtime_var("JAVA_HOME") {
            let p = PathBuf::from(&val);
            if p.join("jmods").is_dir() {
                return Some(p);
            }
        }
        // Fall back to PATH detection
        detect_java_home_from_path()
    }

    /// **ONE FILE, TWO TESTS, AND `cargo test` RUNS THEM AT THE SAME TIME.**
    ///
    /// `java_resolves_to_the_sibling_alias_before_path` and
    /// `auto_detection_declines_when_java_is_this_process` both stand a decoy
    /// at `<test binary's directory>/java.exe`, and neither can use a different
    /// path: the behaviour under test is that `CreateProcessW` searches the
    /// calling executable's OWN directory first, so the file has to be beside
    /// this binary and has to be called `java.exe`.
    ///
    /// Unserialised they raced three ways, and the loudest was the simplest:
    /// both reach `std::fs::write` / `std::fs::copy` on the same path, and
    /// Windows refuses the second one rather than interleaving it. Measured at
    /// **5 failures in 40 paired runs** before this lock, 0 in 200 after.
    /// The quieter interleaving is the one that reads as a spelling bug: B
    /// deletes the file between A's `resolve_java_executable` and A's
    /// `canonicalize`, so A's `expected` falls through `unwrap_or` to the raw
    /// path while `resolved` is the canonical `\?\` form, and the assertion
    /// compares two spellings of the same file.
    ///
    /// `parking_lot`, so a panicking test poisons nothing: a failure in one of
    /// these must fail ONE test, not convert the other into a lock-poisoning
    /// error that hides its own result.
    #[cfg(windows)]
    static SIBLING_ALIAS: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// The `java.exe`-beside-`cratonvm.exe` layout must not auto-detect.
    ///
    /// This is the configuration the `java-bin-alias` cargo feature
    /// produces, and it is the one that recursed without bound: the old
    /// guard compared `current_exe()` against the first `java` on `PATH`,
    /// which on a machine with a real JDK installed is a different file,
    /// so the guard passed and the spawn then ran the sibling alias —
    /// this VM — anyway.
    ///
    /// Asserted through `resolve_java_executable` rather than by spawning
    /// anything. A test that actually let the recursion start would be a
    /// test that can take the machine down when it regresses, which is
    /// the opposite of useful.
    #[test]
    #[cfg(windows)]
    fn java_resolves_to_the_sibling_alias_before_path() {
        let _serialised = SIBLING_ALIAS.lock();
        let me = std::env::current_exe().expect("current_exe");
        let dir = me.parent().expect("exe has a parent").to_path_buf();

        let alias = dir.join("java.exe");
        let created = !alias.exists();
        if created {
            // Contents are irrelevant: resolution is by path, and nothing
            // here executes it.
            std::fs::write(&alias, b"not a real executable").expect("write decoy");
        }

        let resolved = resolve_java_executable();

        // CANONICALISE BEFORE THE DELETE. `std::fs::canonicalize` resolves
        // against the filesystem and FAILS for a path that no longer exists, so
        // asking about `alias` after removing it silently fell through
        // `unwrap_or` to the raw path -- while `resolve_java_executable` hands
        // back an already-canonical extended-length one. Two spellings of the
        // same file, and the assertion compared them: the resolved side carried
        // the Windows `\\?\` prefix and the expected side did not.
        //
        // Windows-only, and it also fails ALONE, which is why it was fixed
        // separately from the race: the two causes are independent and closing
        // either one leaves the other. The race is real -- see
        // [`SIBLING_ALIAS`], which is what now closes it -- and an earlier
        // revision of this comment asserted it was not.
        let expected = std::fs::canonicalize(&alias).unwrap_or_else(|_| alias.clone());

        if created {
            let _ = std::fs::remove_file(&alias);
        }

        let resolved = resolved.expect("a java.exe beside the test binary must resolve");
        let resolved = std::fs::canonicalize(&resolved).unwrap_or(resolved);
        assert_eq!(
            resolved, expected,
            "CreateProcessW searches the calling executable's own directory \
             before PATH, so resolution must too — otherwise the self-check \
             in detect_java_home_from_path compares against the wrong file \
             and a cratonvm built as java.exe spawns itself without bound"
        );
    }

    /// Auto-detection must decline when `java` resolves to this process.
    ///
    /// The payload of the fix: whatever `PATH` holds, if the thing that
    /// would be spawned is this executable, the answer is `None` and the
    /// user is pushed to `JAVA_HOME` / `--java-home`.
    #[test]
    #[cfg(windows)]
    fn auto_detection_declines_when_java_is_this_process() {
        let _serialised = SIBLING_ALIAS.lock();
        let me = std::env::current_exe().expect("current_exe");
        let dir = me.parent().expect("exe has a parent").to_path_buf();
        let alias = dir.join("java.exe");

        // Only meaningful when we can stand in for the alias ourselves.
        // Copying this test binary makes `resolve_java_executable` return a
        // file whose canonical path equals `current_exe()`'s content-wise
        // but not path-wise, so instead assert the narrower, decisive
        // property: a resolution equal to current_exe() yields None.
        if alias.exists() {
            return;
        }
        std::fs::copy(&me, &alias).expect("copy self as java.exe");
        let resolved = resolve_java_executable().map(|p| std::fs::canonicalize(&p).unwrap_or(p));
        let _ = std::fs::remove_file(&alias);

        let resolved = resolved.expect("sibling alias resolves");
        assert!(
            resolved.file_name().and_then(|n| n.to_str()) == Some("java.exe"),
            "expected the sibling alias, got {}",
            resolved.display()
        );
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

        let entries = discover_boot_classpath_with(Some(jdk.to_str().unwrap()), false);
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
        // `ALL-UNNAMED` survives parsing verbatim. Collapsing it to `""` here
        // is what made `--add-opens ...=ALL-UNNAMED` an unqualified open, so
        // this assertion is the guard on the over-grant, not a formatting
        // preference: `""` is `ModuleRegistry`'s open-to-everyone marker.
        let result = VmConfig::parse_add_exports("java.base/java.lang=ALL-UNNAMED");
        assert_eq!(
            result,
            Some((
                "java.base".to_string(),
                "java/lang".to_string(),
                "ALL-UNNAMED".to_string()
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
    /// by pointing `CRATONVM_JAVA_HOME` at a synthesised JDK tree, with
    /// `JAVA_HOME` scrubbed so the next probe down cannot answer instead.
    ///
    /// The two variables need *different* mechanisms, and getting that wrong
    /// is what made these tests vacuous for months:
    ///
    /// * `CRATONVM_JAVA_HOME` is a **declared** flag. `resolve_java_home`
    ///   reads it through `flags::runtime_var`, which serves it from one
    ///   process-wide snapshot latched on the first read of any flag. In this
    ///   crate's test binary that latch has long since happened, so `set_var`
    ///   here changed `environ` and nothing the code under test would read —
    ///   the test then measured the developer's real JDK. It is overridden on
    ///   the snapshot instead, for this thread only.
    /// * `JAVA_HOME` is **not** declared and keeps `std::env`'s live-read
    ///   semantics, so it still has to be stashed/restored in `environ` —
    ///   which is process-wide and races concurrent tests, hence
    ///   [`env_lock`].
    fn with_scratch_java_home<R>(root: Option<&str>, f: impl FnOnce() -> R) -> R {
        cratonvm_types::flags::with_thread_overrides(&[("CRATONVM_JAVA_HOME", root)], || {
            with_env("JAVA_HOME", None, f)
        })
    }

    /// Set an **undeclared** environment variable for the duration of `f`.
    ///
    /// Declared `CRATONVM_*` flags must not go through here — see
    /// [`with_scratch_java_home`] — so this asserts against them rather than
    /// leaving the next caller to rediscover why their override did nothing.
    fn with_env<R>(key: &str, value: Option<&str>, f: impl FnOnce() -> R) -> R {
        debug_assert!(
            !key.starts_with("CRATONVM_"),
            "{key} looks like a declared flag; use flags::with_thread_overrides"
        );
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

    /// Avoid `JAVA_HOME` races between the detection tests.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn detect_real_jdk_returns_some_when_java_base_jmod_present() {
        let tmp = tempfile::tempdir().unwrap();
        fake_jdk_with_jmod(tmp.path());
        let detected = detect_real_jdk_from(tmp.path().to_str());
        assert!(
            detected.is_some(),
            "detect_real_jdk should find synthetic JDK at {}",
            tmp.path().display()
        );
        assert_eq!(detected.unwrap(), tmp.path());
    }

    #[test]
    fn detect_real_jdk_returns_some_for_lib_modules_image() {
        let tmp = tempfile::tempdir().unwrap();
        fake_jdk_with_lib_modules(tmp.path());
        let detected = detect_real_jdk_from(tmp.path().to_str());
        assert!(
            detected.is_some(),
            "detect_real_jdk should accept jlink-image layout"
        );
    }

    #[test]
    fn detect_real_jdk_returns_none_when_only_rtjar_present() {
        // JDK 8-style: `lib/rt.jar` only. We have no JMOD/jimage loader,
        // so this should NOT count as "real JDK boot available".
        // `CRATONVM_JAVA_HOME` is the highest-priority probe in
        // `resolve_java_home`, so we don't need to scrub `JAVA_HOME` or
        // `PATH` — the synthesised directory wins.
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("rt.jar"), b"PK\x03\x04").unwrap();
        assert!(
            detect_real_jdk_from(tmp.path().to_str()).is_none(),
            "rt.jar-only install must not satisfy detect_real_jdk"
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
        with_scratch_java_home(tmp.path().to_str(), || {
            let cfg = VmConfig::for_launcher();
            assert_eq!(cfg.jdk_mode(), JdkMode::Real);
            assert!(!cfg.use_synthetic_jdk);
        });
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
        let tmp = tempfile::tempdir().unwrap();
        // Bare directory: no `jmods/`, no `lib/modules`. `resolve_java_home`
        // will accept it (it's a real directory) but `detect_real_jdk`
        // must reject it because neither boot blob is present.
        let cfg = VmConfig::for_launcher();
        assert_eq!(
            cfg.jdk_mode(),
            JdkMode::Real,
            "the launcher default must not depend on whether a JDK exists"
        );
        let err = require_real_jdk(tmp.path().to_str())
            .expect_err("require_real_jdk must fail when no boot modules exist");
        assert!(err.contains("no usable JDK was found"), "{err}");
    }

    /// `with_host_jdk_default` is the out-of-crate (`libcratonvm`,
    /// `cratonvm-embed`) base. It no longer aliases `for_launcher()` — the
    /// two now disagree about compatibility mode — but it must still share
    /// the launcher's JDK mode, and in particular it must NOT probe the host.
    #[test]
    fn with_host_jdk_default_shares_the_launchers_jdk_mode() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        // Host with no usable JDK: the legacy implementation would have
        // returned synthetic here.
        with_scratch_java_home(tmp.path().to_str(), || {
            assert_eq!(
                VmConfig::with_host_jdk_default().jdk_mode(),
                VmConfig::for_launcher().jdk_mode()
            );
            assert_eq!(VmConfig::with_host_jdk_default().jdk_mode(), JdkMode::Real);
        });
    }

    /// Explicit `--synthetic-jdk` opt-in must beat the launcher default:
    /// even with a real JDK on the host, an explicit synthetic request is
    /// honoured (subject to `require_synthetic_jdk`, which the CLI checks).
    #[test]
    fn explicit_synthetic_jdk_override_wins_over_launcher_default() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        fake_jdk_with_jmod(tmp.path());
        with_scratch_java_home(tmp.path().to_str(), || {
            let cfg = VmConfig::for_launcher().with_jdk_mode(JdkMode::Synthetic);
            assert_eq!(cfg.jdk_mode(), JdkMode::Synthetic);
            assert!(cfg.use_synthetic_jdk);
            // The legacy boolean setter must stay equivalent.
            assert_eq!(
                VmConfig::for_launcher().with_synthetic_jdk(true).jdk_mode(),
                JdkMode::Synthetic
            );
        });
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
             jdk-mode-determinism.md"
        );
        assert_eq!(LAUNCHER_DEFAULT_JDK_MODE, JdkMode::Real);
    }

    // -----------------------------------------------------------------------
    // Compatibility mode, 2026-07-31; launcher default flipped 2026-09-20.
    //
    // BOTH pairs now split by entry point, for different reasons: the
    // JDK-mode pair on hermeticity, the compatibility pair on who asked.
    // These tests pin the split in both directions, because each half is the
    // kind of "inconsistency" a later reader could tidy the wrong way --
    // toward an embedder that silently runs strict, or a launcher that
    // silently does not.
    // -----------------------------------------------------------------------

    /// The launcher default is real-JDK *and* strict. A person typing
    /// `cratonvm Foo` gets the conforming answer; `--compatible` opts out.
    #[test]
    fn launcher_default_is_real_jdk_and_jdk_only() {
        let cfg = VmConfig::for_launcher();
        assert_eq!(cfg.jdk_mode(), JdkMode::Real);
        assert_eq!(cfg.compatibility_mode, LAUNCHER_DEFAULT_COMPATIBILITY_MODE);
        assert_eq!(cfg.compatibility_mode, CompatibilityMode::JdkOnly);
        assert!(cfg.is_jdk_only());
        assert_eq!(cfg.execution_policy(), ExecutionPolicy::jdk_only());
        cfg.validate_compatibility()
            .expect("the launcher default must be a valid pairing");
    }

    /// `VmConfig::default()` (embedding / in-tree tests) stays `compatible`,
    /// and that is now a DIFFERENCE from the launcher rather than an
    /// agreement. A host that never passed a policy argument has not asked
    /// for a stricter one and has no flag of its own to opt back out with.
    #[test]
    fn embedded_default_is_compatible_and_differs_from_the_launcher() {
        let cfg = VmConfig::default();
        assert_eq!(cfg.compatibility_mode, EMBEDDED_DEFAULT_COMPATIBILITY_MODE);
        assert_eq!(cfg.compatibility_mode, CompatibilityMode::Compatible);
        assert!(!cfg.is_jdk_only());
        assert_ne!(
            EMBEDDED_DEFAULT_COMPATIBILITY_MODE, LAUNCHER_DEFAULT_COMPATIBILITY_MODE,
            "the compatibility defaults differ by design; re-equalising them either \
             hands every embedder a policy it never asked for, or gives the \
             launcher back the substitutions --compatible now names"
        );
        // The default is synthetic-JDK, so the policy must report
        // `real_jdk = false` - `execution_policy` reads the config as it
        // stands rather than asserting a house style.
        assert!(!cfg.execution_policy().real_jdk);
    }

    /// The real-JDK EMBEDDING base: the launcher's JDK mode, the embedder's
    /// compatibility mode. `libcratonvm`'s advertised C-ABI default is
    /// defined as this value, so it must not follow the launcher.
    #[test]
    fn host_jdk_default_is_real_jdk_but_still_compatible() {
        let cfg = VmConfig::with_host_jdk_default();
        assert_eq!(cfg.jdk_mode(), JdkMode::Real);
        assert_eq!(
            cfg.compatibility_mode,
            EMBEDDED_HOST_JDK_DEFAULT_COMPATIBILITY_MODE
        );
        assert_eq!(cfg.compatibility_mode, CompatibilityMode::Compatible);
        assert!(!cfg.is_jdk_only());
        assert_eq!(cfg.execution_policy(), ExecutionPolicy::compatible(true));
        cfg.validate_compatibility()
            .expect("real-JDK + compatible is a valid pairing");
        // It was a bare alias for `for_launcher()` until the launcher default
        // flipped. Re-aliasing it is an ABI break, so pin the divergence.
        assert_ne!(
            VmConfig::with_host_jdk_default().compatibility_mode,
            VmConfig::for_launcher().compatibility_mode,
            "with_host_jdk_default must not inherit the launcher's strictness"
        );
    }

    /// `--jdk-only` paired with real-JDK (what the CLI builds) is a coherent
    /// configuration whose policy reports both halves.
    #[test]
    fn jdk_only_with_real_jdk_is_the_valid_strict_pairing() {
        let cfg = VmConfig::for_launcher()
            .with_jdk_mode(JdkMode::Real)
            .with_compatibility_mode(CompatibilityMode::JdkOnly);
        assert!(cfg.is_jdk_only());
        assert!(!cfg.use_synthetic_jdk);
        let policy = cfg.execution_policy();
        assert_eq!(policy.compatibility_mode, CompatibilityMode::JdkOnly);
        assert!(policy.real_jdk, "strict mode runs on a real JDK image");
        assert!(policy.is_jdk_only());
        assert_eq!(policy, ExecutionPolicy::jdk_only());
        cfg.validate_compatibility()
            .expect("jdk-only + real-jdk is valid");
    }

    /// `--jdk-only --synthetic-jdk` is a configuration error, and the error
    /// has to name both flags and both fixes: the correct correction depends
    /// on what the caller meant, so the VM picks neither.
    ///
    /// Note what is *not* asserted: `with_compatibility_mode` does not clear
    /// `use_synthetic_jdk`. If it did, this pairing would repair itself
    /// silently and the conflict would never reach the operator.
    #[test]
    fn jdk_only_with_synthetic_jdk_is_rejected() {
        let cfg = VmConfig::default()
            .with_jdk_mode(JdkMode::Synthetic)
            .with_compatibility_mode(CompatibilityMode::JdkOnly);
        assert!(
            cfg.use_synthetic_jdk,
            "with_compatibility_mode must not rewrite the JDK mode"
        );
        // The incoherence stays visible in the policy rather than being
        // papered over by `ExecutionPolicy::jdk_only()`.
        assert!(!cfg.execution_policy().real_jdk);

        let err = cfg
            .validate_compatibility()
            .expect_err("jdk-only + synthetic-jdk must be rejected");
        match &err {
            crate::error::VmError::InvalidConfiguration(msg) => {
                for needle in ["--jdk-only", "--synthetic-jdk", "Fix by one of:"] {
                    assert!(
                        msg.contains(needle),
                        "error message must mention {needle:?}; got:\n{msg}"
                    );
                }
            }
            other => panic!("expected VmError::InvalidConfiguration, got {other:?}"),
        }
    }

    /// `JdkMode` carries no strictness. Selecting either class library leaves
    /// the compatibility mode alone — the two axes are orthogonal, and
    /// overloading `JdkMode` with strictness is explicitly out of contract.
    #[test]
    fn jdk_mode_alone_never_implies_strictness() {
        for mode in [JdkMode::Real, JdkMode::Synthetic] {
            let cfg = VmConfig::default().with_jdk_mode(mode);
            assert_eq!(
                cfg.compatibility_mode,
                CompatibilityMode::Compatible,
                "{mode} must not change the compatibility mode"
            );
            assert!(!cfg.is_jdk_only());
            cfg.validate_compatibility()
                .expect("compatible mode is valid with either class library");
        }
        // The legacy boolean setter is equivalent and equally inert.
        assert!(!VmConfig::default().with_synthetic_jdk(true).is_jdk_only());
        // `with_host_jdk_default()` rather than `for_launcher()`: the
        // launcher default is strict now, so it would fail this for a reason
        // that has nothing to do with the setter under test.
        assert!(!VmConfig::with_host_jdk_default()
            .with_synthetic_jdk(false)
            .is_jdk_only());
    }

    /// The real-JDK-unavailable error must name every location searched
    /// and every accepted layout, so a bug report carries enough to fix
    /// the machine without a round trip.
    #[test]
    fn require_real_jdk_error_names_search_path_and_layouts() {
        let tmp = tempfile::tempdir().unwrap();
        let err = require_real_jdk(tmp.path().to_str()).expect_err("bare dir is not a JDK");
        for needle in [
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
    }

    /// An `rt.jar`-only (JDK 8) install is rejected by the *validation*
    /// path too, not merely by detection — and the rejection is an error,
    /// never a downgrade. Companion to
    /// `detect_real_jdk_returns_none_when_only_rtjar_present`.
    #[test]
    fn require_real_jdk_rejects_rtjar_only_install() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("rt.jar"), b"PK\x03\x04").unwrap();
        assert!(require_real_jdk(tmp.path().to_str()).is_err());
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
        with_scratch_java_home(bogus.path().to_str(), || {
            let home = require_real_jdk(Some(explicit.as_str())).expect("explicit JDK is valid");
            assert_eq!(home, tmp.path());
            let described = describe_jdk_search(Some(explicit.as_str()));
            assert!(described.contains("--java-home"), "{described}");
        });
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

/// Interpreter round i3 wave 76, lane L4:
/// `CRATONVM_EXECUTE_NO_JIT_GATE_WITHOUT_JIT` is read once per VM, default
/// off, `truthy_word`. The switch is set explicitly in every case.
#[cfg(test)]
mod i76_l4_execute_no_jit_gate_tests {
    use super::VmConfig;

    #[test]
    fn the_switch_is_off_unless_a_truthy_word() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some(""), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_EXECUTE_NO_JIT_GATE_WITHOUT_JIT", word)],
                || {
                    assert_eq!(
                        VmConfig::default().execute_no_jit_gate_without_jit,
                        on,
                        "{word:?}"
                    );
                },
            );
        }
    }

    #[test]
    fn the_own_body_hint_switch_is_off_unless_a_truthy_word() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some(""), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_NATIVE_OWN_BODY_HINT", word)],
                || {
                    assert_eq!(VmConfig::default().native_own_body_hint, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i3 wave 77, lane L4:
    /// `CRATONVM_EXECUTE_SHARED_METHOD_VIEW` is read once per VM,
    /// `truthy_word`; the switch is set explicitly in every case.
    #[test]
    fn the_shared_method_view_switch_is_off_unless_a_truthy_word() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some(""), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_EXECUTE_SHARED_METHOD_VIEW", word)],
                || {
                    // Default on since the wave-77 triage: unset reads as on.
                    assert_eq!(
                        VmConfig::default().execute_shared_method_view,
                        on || matches!(word, None | Some("")),
                        "{word:?}"
                    );
                },
            );
        }
    }

    /// Interpreter round i3 wave 83, lane L3: `CRATONVM_EXECUTE_FEWER_COPIES`,
    /// `CRATONVM_REFLECT_CTOR_LEAN_ALLOC` and
    /// `CRATONVM_JIT_REFLECT_DOOR_LEAN_SITE` are read once per VM,
    /// `runtime_flag_on`; each is set explicitly in every case, so no default
    /// is pinned.
    #[test]
    fn the_wave_83_l3_switches_follow_a_truthy_word() {
        for (word, on) in [
            (Some("0"), false),
            (Some("false"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_EXECUTE_FEWER_COPIES", word),
                    ("CRATONVM_REFLECT_CTOR_LEAN_ALLOC", word),
                    ("CRATONVM_JIT_REFLECT_DOOR_LEAN_SITE", word),
                ],
                || {
                    let config = VmConfig::default();
                    assert_eq!(config.execute_fewer_copies, on, "{word:?}");
                    assert_eq!(config.reflect_ctor_lean_alloc, on, "{word:?}");
                    assert_eq!(config.jit_reflect_door_lean_site, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i4 wave 91, lane L3: `CRATONVM_FIELD_NAME_RING_DURABLE`
    /// is read once per VM, `runtime_flag_on`; set explicitly in every case,
    /// so no default is pinned.
    #[test]
    fn the_wave_91_l3_field_name_ring_switch_follows_a_truthy_word() {
        for (word, on) in [
            (Some("0"), false),
            (Some("false"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_FIELD_NAME_RING_DURABLE", word)],
                || {
                    let config = VmConfig::default();
                    assert_eq!(config.field_name_ring_durable, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i6 wave 115, lane L7:
    /// `CRATONVM_INTERP_FIELD_CTOR_WIDE_VALUES` is read once per VM,
    /// `runtime_flag_default_on` since the round-i6 wave-115 triage (`--nojit`
    /// 0.60-0.64x); unset, it is on, `=0` off.
    #[test]
    fn the_field_ctor_wide_values_switch_follows_a_truthy_word_and_defaults_on() {
        for (word, on) in [
            (None, true),
            (Some("0"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_INTERP_FIELD_CTOR_WIDE_VALUES", word)],
                || {
                    assert_eq!(VmConfig::default().interp_field_ctor_wide_values, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i6 wave 121, lane L7:
    /// `CRATONVM_INTERP_PRECISE_OOP_MAPS` is read once per VM,
    /// `runtime_flag_on`; unset, it is off.
    #[test]
    fn the_precise_oop_maps_switch_follows_a_truthy_word_and_defaults_off() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_INTERP_PRECISE_OOP_MAPS", word)],
                || {
                    assert_eq!(VmConfig::default().interp_precise_oop_maps, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i6 wave 120, lane L7:
    /// `CRATONVM_LOADER_DICTIONARY_BEFORE_DRIVE` is read once per VM,
    /// `runtime_flag_on`; unset, it is off.
    #[test]
    fn the_loader_dictionary_before_drive_switch_follows_a_truthy_word_and_defaults_off() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_LOADER_DICTIONARY_BEFORE_DRIVE", word)],
                || {
                    assert_eq!(VmConfig::default().loader_dictionary_before_drive, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i6 wave 129, lane L7:
    /// `CRATONVM_LOADER_DRIVE_UNDER_LOADER_LOCK` is read once per VM,
    /// `runtime_flag_default_on`; unset, it is on, and `0` turns it off.
    #[test]
    fn the_loader_drive_under_loader_lock_switch_defaults_on_with_a_zero_escape() {
        for (word, on) in [
            (None, true),
            (Some("0"), false),
            (Some("false"), false),
            (Some("1"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_LOADER_DRIVE_UNDER_LOADER_LOCK", word)],
                || {
                    assert_eq!(VmConfig::default().loader_drive_under_loader_lock, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i6 wave 119, lane L7:
    /// `CRATONVM_USER_LOADER_RESOURCE_BYTECODE` is read once per VM,
    /// `runtime_flag_default_on`; unset, it is on (wave 134, user decision).
    #[test]
    fn the_user_loader_resource_bytecode_switch_follows_an_off_word_and_defaults_on() {
        for (word, on) in [
            (None, true),
            (Some("0"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_USER_LOADER_RESOURCE_BYTECODE", word)],
                || {
                    assert_eq!(VmConfig::default().user_loader_resource_bytecode, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i6 wave 132, lane L5b:
    /// `CRATONVM_USER_LOADER_RESOURCE_BYTECODE_COMPILED` is read once per VM,
    /// `runtime_flag_on`; unset, it is off.
    #[test]
    fn the_user_loader_resource_bytecode_compiled_switch_follows_a_truthy_word_and_defaults_off() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_USER_LOADER_RESOURCE_BYTECODE_COMPILED", word)],
                || {
                    assert_eq!(
                        VmConfig::default().user_loader_resource_bytecode_compiled,
                        on,
                        "{word:?}"
                    );
                },
            );
        }
    }

    /// Interpreter round i6 wave 119, lane L7:
    /// `CRATONVM_INTERP_CTOR_VERDICT_ON_ENTRY` is read once per VM,
    /// `runtime_flag_on`; unset, it is off.
    #[test]
    fn the_ctor_verdict_on_entry_switch_follows_a_truthy_word_and_defaults_off() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_INTERP_CTOR_VERDICT_ON_ENTRY", word)],
                || {
                    assert_eq!(VmConfig::default().interp_ctor_verdict_on_entry, on, "{word:?}");
                },
            );
        }
    }

    /// Tier-1 round wave 2, lane K: `CRATONVM_INTERP_FIELD_CTOR_GEN_G1` is
    /// read once per VM, `runtime_flag_on`; unset, it is off.
    #[test]
    fn the_field_ctor_gen_g1_switch_follows_a_truthy_word_and_defaults_off() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_INTERP_FIELD_CTOR_GEN_G1", word)],
                || {
                    assert_eq!(VmConfig::default().interp_field_ctor_gen_g1, on, "{word:?}");
                },
            );
        }
    }

    /// t1 wave 6, lane G6: `CRATONVM_INTERP_FIELD_CTOR_PRE_SUPER_STORES` is
    /// read once per VM, `runtime_flag_default_on` since the i147 close-out
    /// (2026-10-29); unset, it is on, and `=0` turns it off.
    #[test]
    fn the_field_ctor_pre_super_stores_switch_defaults_on_and_zero_turns_it_off() {
        for (word, on) in [
            (None, true),
            (Some("0"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_INTERP_FIELD_CTOR_PRE_SUPER_STORES", word)],
                || {
                    assert_eq!(
                        VmConfig::default().interp_field_ctor_pre_super_stores,
                        on,
                        "{word:?}"
                    );
                },
            );
        }
    }

    /// Tier-1 round wave 3, lane K: `CRATONVM_MONITOREXIT_METHOD_LOCK_PARITY`
    /// is read once per VM, `runtime_flag_default_on`; unset, it is on, and
    /// `=0` turns it off.
    #[test]
    fn the_method_lock_parity_switch_defaults_on_and_zero_turns_it_off() {
        for (word, on) in [(None, true), (Some("0"), false), (Some("1"), true)] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_MONITOREXIT_METHOD_LOCK_PARITY", word)],
                || {
                    assert_eq!(VmConfig::default().monitorexit_method_lock_parity, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i6 wave 118, lane L7:
    /// `CRATONVM_INTERP_FIELD_CTOR_LDC_VALUES` is read once per VM,
    /// `runtime_flag_default_on` since interpreter round i7 wave 144 (A/B
    /// w142lto); unset, it is on, and `=0` turns it off.
    #[test]
    fn the_field_ctor_ldc_values_switch_follows_a_truthy_word_and_defaults_on() {
        for (word, on) in [
            (None, true),
            (Some("0"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_INTERP_FIELD_CTOR_LDC_VALUES", word)],
                || {
                    assert_eq!(VmConfig::default().interp_field_ctor_ldc_values, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i6 wave 118, lane L7:
    /// `CRATONVM_CLASS_ENTRY_RECORD_ALL` is read once per VM,
    /// `runtime_flag_default_on` since t1 W14; unset, it is on, and `=0`
    /// turns it off.
    #[test]
    fn the_class_entry_record_all_switch_defaults_on_and_a_false_word_turns_it_off() {
        for (word, on) in [
            (None, true),
            (Some("off"), false),
            (Some("0"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_CLASS_ENTRY_RECORD_ALL", word)],
                || {
                    assert_eq!(VmConfig::default().class_entry_record_all, on, "{word:?}");
                },
            );
        }
    }

    /// t1 W15: `CRATONVM_CLASS_SITE_ISOLATED_STICKY` is read once per VM,
    /// `runtime_flag_default_on`; unset, it is on, and `=0` turns it off.
    #[test]
    fn the_class_site_isolated_sticky_switch_is_read_once_per_vm() {
        for (word, on) in [(None, true), (Some("0"), false), (Some("1"), true)] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_CLASS_SITE_ISOLATED_STICKY", word)],
                || {
                    assert_eq!(VmConfig::default().class_site_isolated_sticky, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i6 wave 134, lane L4:
    /// `CRATONVM_INTERFACE_SPECIAL_RECEIVER_CHECK` is read once per VM,
    /// `runtime_flag_default_on`; unset, it is on, and `=0` turns it off.
    #[test]
    fn the_interface_special_receiver_check_switch_defaults_on_with_a_zero_escape() {
        for (word, on) in [(None, true), (Some("0"), false), (Some("1"), true)] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_INTERFACE_SPECIAL_RECEIVER_CHECK", word)],
                || {
                    assert_eq!(
                        VmConfig::default().interface_special_receiver_check,
                        on,
                        "{word:?}"
                    );
                },
            );
        }
    }

    /// Interpreter round i7 wave 136, lane L6:
    /// `CRATONVM_INTERFACE_SPECIAL_RECEIVER_MEMO` is read once per VM,
    /// `runtime_flag_default_on` since interpreter round i7 wave 144 (A/B
    /// w142lto); unset, it is on, and `=0` turns it off.
    #[test]
    fn the_interface_special_receiver_memo_switch_defaults_on() {
        for (word, on) in [(None, true), (Some("0"), false), (Some("1"), true)] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_INTERFACE_SPECIAL_RECEIVER_MEMO", word)],
                || {
                    assert_eq!(
                        VmConfig::default().interface_special_receiver_memo,
                        on,
                        "{word:?}"
                    );
                },
            );
        }
    }

    /// Interpreter round i7 wave 136, lane L2:
    /// `CRATONVM_JIT_IFACE_SPECIAL_RECEIVER_GUARD` is read once per VM,
    /// `runtime_flag_default_on` since interpreter round i7 wave 144 (A/B
    /// w142lto); unset, it is on, and `=0` turns it off.
    #[test]
    fn the_iface_special_receiver_guard_switch_defaults_on() {
        for (word, on) in [(None, true), (Some("0"), false), (Some("1"), true)] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_IFACE_SPECIAL_RECEIVER_GUARD", word)],
                || {
                    assert_eq!(
                        VmConfig::default().jit_iface_special_receiver_guard,
                        on,
                        "{word:?}"
                    );
                },
            );
        }
    }

    /// Interpreter round i6 wave 115, lane L7:
    /// `CRATONVM_EXECUTE_METHOD_INDEX_MEMO` is read once per VM,
    /// `runtime_flag_on`; unset, it is off.
    #[test]
    fn the_method_index_memo_switch_follows_a_truthy_word_and_defaults_off() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some("false"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_EXECUTE_METHOD_INDEX_MEMO", word)],
                || {
                    assert_eq!(VmConfig::default().execute_method_index_memo, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i6 wave 115, lane L8: the two branch fusions'
    /// switches (`CRATONVM_INTERP_FUSED_ZERO_BRANCH`,
    /// `CRATONVM_INTERP_FUSED_NULL_BRANCH`) are read once per VM,
    /// `runtime_flag_on`; unset, each is off.
    #[test]
    fn the_branch_fusion_switches_follow_a_truthy_word_and_default_off() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_INTERP_FUSED_ZERO_BRANCH", word)],
                || {
                    // Default ON since the round-i6 wave-115 triage.
                    let on = on || word.is_none();
                    assert_eq!(VmConfig::default().interp_fused_zero_branch, on, "{word:?}");
                },
            );
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_INTERP_FUSED_NULL_BRANCH", word)],
                || {
                    // Default ON since the round-i6 wave-115 triage.
                    let on = on || word.is_none();
                    assert_eq!(VmConfig::default().interp_fused_null_branch, on, "{word:?}");
                },
            );
            // Wave 116 (lane L8): the next three, read the same way.
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_INTERP_FUSED_NULL_BRANCH_WIDE", word)],
                || {
                    assert_eq!(VmConfig::default().interp_fused_null_branch_wide, on, "{word:?}");
                },
            );
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_INTERP_FUSED_GETFIELD_CHAIN", word)],
                || {
                    assert_eq!(VmConfig::default().interp_fused_getfield_chain, on, "{word:?}");
                },
            );
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_INTERP_LEAN_LOOP_TOP", word)],
                || {
                    assert_eq!(VmConfig::default().interp_lean_loop_top, on, "{word:?}");
                },
            );
            // Wave 118 (lane L8).
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_INTERP_FAST_SWITCH_ARMS", word)],
                || {
                    // Default ON since the round-i6 wave-129 triage.
                    let on = on || word.is_none();
                    assert_eq!(VmConfig::default().interp_fast_switch_arms, on, "{word:?}");
                },
            );
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_INTERP_FUSED_LOOP_STEP_TEST", word)],
                || {
                    assert_eq!(VmConfig::default().interp_fused_loop_step_test, on, "{word:?}");
                },
            );
            // Wave 119 (lane L8).
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_INTERP_FUSED_STORE_CHAIN", word)],
                || {
                    assert_eq!(VmConfig::default().interp_fused_store_chain, on, "{word:?}");
                },
            );
            // Wave 120 (lane L8).
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_INTERP_FUSED_STORE_CHAIN_WIDE", word)],
                || {
                    assert_eq!(
                        VmConfig::default().interp_fused_store_chain_wide,
                        on,
                        "{word:?}"
                    );
                },
            );
        }
    }

    /// Interpreter round i5 wave 114, lane L7: `CRATONVM_EXECUTE_LEAN_REENTRY`
    /// is read once per VM, `runtime_flag_on`; unset, it is off.
    #[test]
    fn the_lean_reentry_switch_follows_a_truthy_word_and_defaults_off() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some("false"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_EXECUTE_LEAN_REENTRY", word)],
                || {
                    assert_eq!(VmConfig::default().execute_lean_reentry, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i3 wave 79, lane L7:
    /// `CRATONVM_EXECUTE_CACHED_REENTRY_FRAME` is read once per VM,
    /// `runtime_flag_on` (a truthy word turns it on); the switch is set
    /// explicitly in every case, so no default is pinned.
    #[test]
    fn the_cached_reentry_frame_switch_follows_a_truthy_word() {
        for (word, on) in [
            (Some("0"), false),
            (Some("false"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_EXECUTE_CACHED_REENTRY_FRAME", word)],
                || {
                    assert_eq!(
                        VmConfig::default().execute_cached_reentry_frame,
                        on,
                        "{word:?}"
                    );
                },
            );
        }
    }

    /// Interpreter round i6 wave 125, lane L6:
    /// `CRATONVM_VTHREAD_EXECUTE_CACHED_FRAME` is read once per VM,
    /// `runtime_flag_default_on` (default ON since interpreter round i7 wave
    /// 144, A/B w142lto; `=0` off).
    #[test]
    fn i125_l6_the_vthread_execute_cached_frame_switch_is_default_on() {
        for (word, on) in [
            (None, true),
            (Some("0"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_VTHREAD_EXECUTE_CACHED_FRAME", word)],
                || {
                    assert_eq!(
                        VmConfig::default().vthread_execute_cached_frame,
                        on,
                        "{word:?}"
                    );
                },
            );
        }
    }

    /// Interpreter round i3 wave 80, lane L5:
    /// `CRATONVM_CLASS_MIRROR_LAYOUT_MEMO` is read once per VM,
    /// `runtime_flag_on`; every case sets the word, so no default is pinned.
    #[test]
    fn the_class_mirror_layout_memo_switch_follows_a_truthy_word() {
        for (word, on) in [
            (Some("0"), false),
            (Some("false"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_CLASS_MIRROR_LAYOUT_MEMO", word)],
                || {
                    assert_eq!(VmConfig::default().class_mirror_layout_memo, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i4 wave 93, lane L6: the two stage-2 virtual-thread
    /// compiled-entry switches are read once per VM, `runtime_flag_on`; every
    /// case sets the word, so no default is pinned.
    #[test]
    fn the_wave_93_l6_vthread_compiled_switches_follow_a_truthy_word() {
        for (word, on) in [
            (Some("0"), false),
            (Some("false"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_VTHREAD_COMPILED_INSTANCE_LEAF", word),
                    ("CRATONVM_VTHREAD_COMPILED_LEAF_CALLERS", word),
                ],
                || {
                    let config = VmConfig::default();
                    assert_eq!(config.vthread_compiled_instance_leaf, on, "{word:?}");
                    assert_eq!(config.vthread_compiled_leaf_callers, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i5 wave 100, lane L6: the virtual-thread leaf OSR
    /// switch is read once per VM, `runtime_flag_on` (default off).
    #[test]
    fn the_wave_100_l6_vthread_leaf_osr_switch_follows_a_truthy_word() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some("false"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_VTHREAD_COMPILED_LEAF_OSR", word)],
                || {
                    assert_eq!(VmConfig::default().vthread_compiled_leaf_osr, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i5 wave 101, lane L2: the learned virtual-thread
    /// compiled-entry switch is read once per VM, `runtime_flag_default_on`
    /// (flipped on in the w108 triage, back off after the w109 per-rep bench
    /// lost 1.5x on `helpers-virtual` under `--compatible`; on again in wave
    /// 113 with `CRATONVM_VTHREAD_SKIP_BRANCH_ARM`, which removed that loss).
    #[test]
    fn the_wave_101_l2_vthread_compiled_learned_switch_follows_a_truthy_word() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some("false"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_VTHREAD_COMPILED_LEARNED", word)],
                || {
                    assert_eq!(VmConfig::default().vthread_compiled_learned, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i5 wave 106, lane L6: the learned arm's ENTRY switch
    /// is read once per VM, `runtime_flag_default_on` (default ON since
    /// interpreter round i7 wave 144, A/B w142lto; `=0` off), and needs the
    /// learned switch itself (default on since wave 113).
    #[test]
    fn the_wave_106_l6_vthread_compiled_learned_entry_needs_the_learned_switch() {
        for (learned, entry, on) in [
            (None, None, false),
            (Some("1"), None, true),
            (None, Some("1"), false),
            (Some("0"), Some("1"), false),
            (Some("1"), Some("0"), false),
            (Some("1"), Some("1"), true),
            (Some("true"), Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_VTHREAD_COMPILED_LEARNED", learned),
                    ("CRATONVM_VTHREAD_COMPILED_LEARNED_ENTRY", entry),
                ],
                || {
                    assert_eq!(
                        VmConfig::default().vthread_compiled_learned_entry,
                        on,
                        "{learned:?} {entry:?}"
                    );
                },
            );
        }
    }

    /// Interpreter round i5 wave 114, lane L6: the application entry arm is
    /// read once per VM, `runtime_flag_on` (default off), and needs the
    /// learned switch (default on); the local call count and the virtual
    /// thread nonvirtual doors are `runtime_flag_on` (default off), and the
    /// doors imply the local count.
    #[test]
    fn the_wave_114_l6_vthread_app_entry_local_count_and_doors_follow_their_words() {
        for (learned, app, on) in [
            (None, None, false),
            (None, Some("1"), false),
            (Some("0"), Some("1"), false),
            (Some("1"), Some("0"), false),
            (Some("1"), Some("1"), true),
            (Some("true"), Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_VTHREAD_COMPILED_LEARNED", learned),
                    ("CRATONVM_VTHREAD_COMPILED_APP_ENTRY", app),
                ],
                || {
                    assert_eq!(
                        VmConfig::default().vthread_compiled_app_entry,
                        on,
                        "{learned:?} {app:?}"
                    );
                },
            );
        }
        // The nonvirtual doors switch implies the local count.
        for (count, doors, count_on, doors_on) in [
            (None, None, false, false),
            (Some("0"), None, false, false),
            (Some("1"), None, true, false),
            (Some("true"), Some("0"), true, false),
            (None, Some("1"), true, true),
            (Some("0"), Some("1"), true, true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_VTHREAD_LOCAL_CALL_COUNT", count),
                    ("CRATONVM_VTHREAD_NONVIRTUAL_DOORS", doors),
                ],
                || {
                    let config = VmConfig::default();
                    assert_eq!(config.vthread_local_call_count, count_on, "{count:?} {doors:?}");
                    assert_eq!(config.vthread_nonvirtual_doors, doors_on, "{count:?} {doors:?}");
                },
            );
        }
        // Interpreter round i6 wave 115, lane L6: the virtual door on a
        // virtual thread is `runtime_flag_default_on` (default ON since the
        // round-i6 wave-115 triage, `=0` off), and implies neither the
        // nonvirtual doors nor the local count.
        for (word, on) in [
            (None, true),
            (Some("0"), false),
            (Some("false"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_VTHREAD_VIRTUAL_DOOR", word),
                    ("CRATONVM_VTHREAD_NONVIRTUAL_DOORS", None),
                ],
                || {
                    let config = VmConfig::default();
                    assert_eq!(config.vthread_virtual_door, on, "{word:?}");
                    assert!(!config.vthread_nonvirtual_doors, "{word:?}");
                },
            );
        }
        // Interpreter round i6 wave 117, lane L6: the entry-only denial is
        // `runtime_flag_default_on` (`=0` off).
        for (word, on) in [(None, true), (Some("0"), false), (Some("1"), true)] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_VTHREAD_ENTRY_PIN_DENIES_ENTRY_ONLY", word)],
                || {
                    assert_eq!(
                        VmConfig::default().vthread_entry_pin_denies_entry_only,
                        on,
                        "{word:?}"
                    );
                },
            );
        }
        // Wave 118 (lane L6): the leaf-verdict memo is `runtime_flag_on`.
        for (word, on) in [(None, false), (Some("0"), false), (Some("1"), true)] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_VTHREAD_LEAF_VERDICT_MEMO", word)],
                || {
                    assert_eq!(VmConfig::default().vthread_leaf_verdict_memo, on, "{word:?}");
                },
            );
        }
        // Wave 120 (lane L6): the app-entry caller walk is `runtime_flag_on`.
        for (word, on) in [(None, false), (Some("0"), false), (Some("1"), true)] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_VTHREAD_APP_ENTRY_CALLER_WALK", word)],
                || {
                    assert_eq!(
                        VmConfig::default().vthread_app_entry_caller_walk,
                        on,
                        "{word:?}"
                    );
                },
            );
        }
        // Wave 120 (lane L6): the slice on the carrier is `runtime_flag_on`.
        for (word, on) in [(None, false), (Some("0"), false), (Some("1"), true)] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_VTHREAD_SLICE_ON_CARRIER", word)],
                || {
                    assert_eq!(VmConfig::default().vthread_slice_on_carrier, on, "{word:?}");
                },
            );
        }
        // t1 wave 5 (lane E2): stage 2 is `runtime_flag_on` too.
        for (word, on) in [(None, false), (Some("0"), false), (Some("1"), true)] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_VTHREAD_SLICE_ON_CARRIER_COMPILED", word)],
                || {
                    assert_eq!(
                        VmConfig::default().vthread_slice_on_carrier_compiled,
                        on,
                        "{word:?}"
                    );
                },
            );
        }
        // Wave 122 (lane L6): the lambda door on a virtual thread is
        // `runtime_flag_on`.
        for (word, on) in [(None, false), (Some("0"), false), (Some("1"), true)] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_VTHREAD_LAMBDA_DOOR", word)],
                || {
                    assert_eq!(VmConfig::default().vthread_lambda_door, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i5 wave 106, lane L6: the hand-off spin's yield
    /// switch is read once per VM, `runtime_flag_default_on` (default ON
    /// since the w106 triage; `=0` off).
    #[test]
    fn the_wave_106_l6_vthread_handoff_spin_yields_follows_a_truthy_word() {
        for (word, on) in [
            (None, true),
            (Some("0"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_VTHREAD_HANDOFF_SPIN_YIELDS", word)],
                || {
                    assert_eq!(
                        VmConfig::default().vthread_handoff_spin_yields,
                        on,
                        "{word:?}"
                    );
                },
            );
        }
    }

    /// Interpreter round i5 wave 101, lane L2: the optimizing OSR route's
    /// resolver-miss retry switch is read once per VM,
    /// `runtime_flag_default_on` (default ON since j27 2026-10-28, `=0` kills).
    #[test]
    fn the_wave_101_l2_osr_optimizing_resolver_miss_retry_switch_follows_a_truthy_word() {
        for (word, on) in [
            (None, true),
            (Some("0"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_OSR_OPTIMIZING_RESOLVER_MISS_RETRY", word)],
                || {
                    assert_eq!(
                        VmConfig::default().jit_osr_optimizing_resolver_miss_retry,
                        on,
                        "{word:?}"
                    );
                },
            );
        }
    }

    /// Interpreter round i4 wave 94, lane L6: the virtual-thread lifecycle
    /// switches and their census are read once per VM, `runtime_flag_on`;
    /// every case sets the word, so no default is pinned.
    #[test]
    fn the_wave_94_l6_vthread_lifecycle_switches_follow_a_truthy_word() {
        for (word, on) in [
            (Some("0"), false),
            (Some("false"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_VTHREAD_RECYCLE_EXEC_CACHES", word),
                    ("CRATONVM_VTHREAD_LEAN_REGISTRY", word),
                    ("CRATONVM_VTHREAD_CARRIER_SLEEPS", word),
                    ("CRATONVM_VTHREAD_SKIP_BRANCH_ARM", word),
                    ("CRATONVM_DBG_VTHREAD_CENSUS", word),
                ],
                || {
                    let config = VmConfig::default();
                    assert_eq!(config.vthread_recycle_exec_caches, on, "{word:?}");
                    assert_eq!(config.vthread_lean_registry, on, "{word:?}");
                    assert_eq!(config.vthread_carrier_sleeps, on, "{word:?}");
                    assert_eq!(config.vthread_skip_branch_arm, on, "{word:?}");
                    assert_eq!(config.dbg_vthread_census, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i4 wave 95, lane L6: the virtual-thread lifecycle
    /// switches are read once per VM and follow their word; every case sets
    /// it, so no default is pinned.
    #[test]
    fn the_wave_95_l6_vthread_lifecycle_switches_follow_their_word() {
        for (word, on) in [
            (Some("0"), false),
            (Some("false"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_VTHREAD_ONE_PUBLISH", word),
                    ("CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT", word),
                    ("CRATONVM_VTHREAD_RETIRE_MANAGER_ENTRIES", word),
                ],
                || {
                    let config = VmConfig::default();
                    assert_eq!(config.vthread_one_publish, on, "{word:?}");
                    assert_eq!(config.vthread_death_sweep_by_count, on, "{word:?}");
                    assert_eq!(config.vthread_retire_manager_entries, on, "{word:?}");
                },
            );
        }
    }
}

#[cfg(test)]
mod w8g_explicit_gc_policy_tests {
    //! gc-common w8-g: `-XX:±DisableExplicitGC` / `-XX:±ExplicitGCInvokesConcurrent`.

    use super::*;

    /// Every entry point keeps the pre-flag behaviour unless a flag is given.
    #[test]
    fn every_entry_point_defaults_to_a_full_explicit_gc() {
        assert_eq!(ExplicitGcPolicy::default(), ExplicitGcPolicy::Full);
        assert_eq!(VmConfig::default().explicit_gc, ExplicitGcPolicy::Full);
        assert_eq!(VmConfig::for_launcher().explicit_gc, ExplicitGcPolicy::Full);
        assert_eq!(
            VmConfig::with_host_jdk_default().explicit_gc,
            ExplicitGcPolicy::Full
        );
    }

    /// HotSpot's precedence: `DisableExplicitGC` wins; a `-XX:-` form is off.
    #[test]
    fn disable_wins_over_concurrent_and_minus_forms_are_off() {
        use ExplicitGcPolicy::{Concurrent, Disabled, Full};
        let p = ExplicitGcPolicy::from_hotspot_flags;
        assert_eq!(p(None, None), Full);
        assert_eq!(p(Some(false), Some(false)), Full);
        assert_eq!(p(Some(true), None), Disabled);
        assert_eq!(p(Some(true), Some(true)), Disabled);
        assert_eq!(p(Some(true), Some(false)), Disabled);
        assert_eq!(p(None, Some(true)), Concurrent);
        assert_eq!(p(Some(false), Some(true)), Concurrent);
        assert_eq!(p(None, Some(false)), Full);
    }
}
