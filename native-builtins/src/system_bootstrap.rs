// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T14 — System bootstrap native methods for real JDK 25 boot.
//!
//! Implements the native methods required by `java.lang.System.initPhase1()`:
//!
//! - `jdk/internal/util/SystemProps$Raw.platformProperties()[Ljava/lang/String;`
//! - `jdk/internal/util/SystemProps$Raw.vmProperties()[Ljava/lang/String;`
//! - `java/io/FileDescriptor.initIDs()V`
//! - `java/io/FileInputStream.initIDs()V`
//! - `java/io/FileOutputStream.initIDs()V`
//! - `sun/io/Win32ErrorMode.setErrorMode(J)J`
//!
//! These are called during the real JDK 25 `System.initPhase1()` bytecode
//! execution path and must return correct values for the bootstrap to succeed.

use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

// ---------------------------------------------------------------------------
// platformProperties() NDX constants (JDK 25)
// These MUST match the values in jdk.internal.util.SystemProps$Raw exactly.
// Verified by running DumpNDX.java on JDK 25.0.1.
// ---------------------------------------------------------------------------

const DISPLAY_COUNTRY_NDX: usize = 0;
const DISPLAY_LANGUAGE_NDX: usize = 1;
const DISPLAY_SCRIPT_NDX: usize = 2;
const DISPLAY_VARIANT_NDX: usize = 3;
const FILE_SEPARATOR_NDX: usize = 4;
const FORMAT_COUNTRY_NDX: usize = 5;
const FORMAT_LANGUAGE_NDX: usize = 6;
const FORMAT_SCRIPT_NDX: usize = 7;
const FORMAT_VARIANT_NDX: usize = 8;
#[allow(dead_code)]
const FTP_NON_PROXY_HOSTS_NDX: usize = 9;
#[allow(dead_code)]
const FTP_PROXY_HOST_NDX: usize = 10;
#[allow(dead_code)]
const FTP_PROXY_PORT_NDX: usize = 11;
#[allow(dead_code)]
const HTTP_NON_PROXY_HOSTS_NDX: usize = 12;
#[allow(dead_code)]
const HTTP_PROXY_HOST_NDX: usize = 13;
#[allow(dead_code)]
const HTTP_PROXY_PORT_NDX: usize = 14;
#[allow(dead_code)]
const HTTPS_PROXY_HOST_NDX: usize = 15;
#[allow(dead_code)]
const HTTPS_PROXY_PORT_NDX: usize = 16;
const JAVA_IO_TMPDIR_NDX: usize = 17;
const LINE_SEPARATOR_NDX: usize = 18;
const NATIVE_ENCODING_NDX: usize = 19;
const OS_ARCH_NDX: usize = 20;
const OS_NAME_NDX: usize = 21;
const OS_VERSION_NDX: usize = 22;
const PATH_SEPARATOR_NDX: usize = 23;
#[allow(dead_code)]
const SOCKS_NON_PROXY_HOSTS_NDX: usize = 24;
#[allow(dead_code)]
const SOCKS_PROXY_HOST_NDX: usize = 25;
#[allow(dead_code)]
const SOCKS_PROXY_PORT_NDX: usize = 26;
const STDERR_ENCODING_NDX: usize = 27;
const STDIN_ENCODING_NDX: usize = 28;
const STDOUT_ENCODING_NDX: usize = 29;
#[allow(dead_code)]
const SUN_ARCH_ABI_NDX: usize = 30;
const SUN_ARCH_DATA_MODEL_NDX: usize = 31;
const SUN_CPU_ENDIAN_NDX: usize = 32;
#[allow(dead_code)]
const SUN_CPU_ISALIST_NDX: usize = 33;
const SUN_IO_UNICODE_ENCODING_NDX: usize = 34;
const SUN_JNU_ENCODING_NDX: usize = 35;
#[allow(dead_code)]
const SUN_OS_PATCH_LEVEL_NDX: usize = 36;
const USER_DIR_NDX: usize = 37;
const USER_HOME_NDX: usize = 38;
const USER_NAME_NDX: usize = 39;
const FIXED_LENGTH: usize = 40;

// ---------------------------------------------------------------------------
// Native implementations
// ---------------------------------------------------------------------------

/// `jdk/internal/util/SystemProps$Raw.platformProperties()[Ljava/lang/String;`
///
/// Returns a `String[FIXED_LENGTH]` array where each index corresponds to a
/// platform property defined by the `_*_NDX` constants. Null entries mean
/// "not set" (the JDK will use its own defaults or skip).
///
/// This replaces the C++ `SystemProps::platformProperties()` in HotSpot.
fn native_platform_properties(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    platform_properties_for(ctx, false)
}

/// The JDK 21 image's `SystemProps$Raw` has a different index layout (`_file_encoding_NDX` at 4,
/// no `_native_encoding_NDX`, no `_stdin_encoding_NDX`, `FIXED_LENGTH` 39): the same properties,
/// every index after `display_variant` moved. Registered instead of
/// [`native_platform_properties`] when the real image is older than 25
/// (`register_t14_system_bootstrap`). With the 25 layout on a 21 image `user.dir` landed in the
/// slot of `user.home`, `native.encoding` of `file.separator`'s neighbour, and so on; the
/// JDK's own `System.props` carried them, which the VM's store masked until
/// `System.getProperties()` became the JDK's own body (SBR5 sysprops: `Path.of` failed with
/// "default directory must be absolute").
fn native_platform_properties_jdk21(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    platform_properties_for(ctx, true)
}

/// The JDK 25 index `ndx` in the JDK 21 `SystemProps$Raw` layout; `None` for a property that
/// layout does not have (`native.encoding`, `stdin.encoding`).
fn jdk21_ndx(ndx: usize) -> Option<usize> {
    match ndx {
        0..=3 => Some(ndx),
        4..=18 => Some(ndx + 1),
        NATIVE_ENCODING_NDX | STDIN_ENCODING_NDX => None,
        20..=27 => Some(ndx),
        _ => Some(ndx - 1),
    }
}

fn platform_properties_for(ctx: &mut dyn NativeContext, legacy_layout: bool) -> MethodCallResult {
    // Read BEFORE the `set` closure below borrows `ctx` for the rest of the
    // function. The borrow checker, not style, decides the position of this
    // line.
    //
    // `native.encoding` is DERIVED, not pinned, as of 2026-09-01.
    // `vm/src/vm/vm_init.rs::derive_native_encoding` owns the derivation — the
    // host locale on Unix, still the `UTF-8` constant on Windows because no
    // `GetACP`/`GetConsoleOutputCP` call exists anywhere in this tree — and
    // stamps the answer into the map read back here, so there is ONE
    // derivation rather than a copy per crate.
    //
    // This index is the copy that WINS in real-JDK mode:
    // `SystemProps.initProperties` does an unconditional
    // `put(props, "native.encoding", raw.propDefault(_native_encoding_NDX))`,
    // which overrides both the `vmProperties()` entry below and any `-D` —
    // the command-line immunity the property is specified to have.
    //
    // The fallback is what makes this the change that cannot fail: if the map
    // somehow has no such key (it always does — `SharedVm::new` seeds it long
    // before any bytecode runs) this answers exactly what the line it replaces
    // answered. See
    // docs/known-issues/stdout-encoding-differs-from-hotspot-on-windows-20260901.md.
    let native_encoding = ctx
        .get_system_property("native.encoding")
        .unwrap_or_else(|| "UTF-8".to_string());
    // The three stream keys, on the same rule and for the same reason. Read
    // BEFORE the `set` closure below, which borrows `ctx` mutably for the rest
    // of the function. See the `--- Encoding ---` block for why these are read
    // back rather than pinned even though they are inert here.
    let stdout_encoding = ctx
        .get_system_property("stdout.encoding")
        .unwrap_or_else(|| "UTF-8".to_string());
    let stderr_encoding = ctx
        .get_system_property("stderr.encoding")
        .unwrap_or_else(|| "UTF-8".to_string());
    let stdin_encoding = ctx
        .get_system_property("stdin.encoding")
        .unwrap_or_else(|| "UTF-8".to_string());
    // The locale slots, on the same rule: read back what
    // `vm/src/vm/vm_init.rs::derive_host_locale` published rather than
    // restating `en`/`US`. `SystemProps.fillI18nProps` writes a `user.*` key
    // from these slots whenever the key is absent, so a constant `US` here
    // re-creates the `user.country` that vm_init deliberately leaves unset
    // under `LANG=C.UTF-8` (HotSpot 25 reports it null there; SBR wave 4
    // LOCALE2). DISPLAY is the base key, FORMAT the `.format` overlay falling
    // back to the base -- exactly the pair `fill_i18n_props` was given.
    // JIT round 16 (locale lane (b)) landed the same mirror; its kill switch
    // `CRATONVM_HOST_LOCALE_PARSELOCALE=0` restores the `en` / `US` constants.
    let locale_slots: Option<[Option<String>; 8]> = if matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_HOST_LOCALE_PARSELOCALE").as_deref(),
        Ok("0")
    ) {
        None
    } else {
        ctx.get_system_property("user.language").map(|_| {
            let base = |k: &str| ctx.get_system_property(k).filter(|v| !v.is_empty());
            let fmt = |k: &str| {
                ctx.get_system_property(&format!("{k}.format"))
                    .filter(|v| !v.is_empty())
                    .or_else(|| base(k))
            };
            [
                base("user.country"),
                base("user.language"),
                base("user.script"),
                base("user.variant"),
                fmt("user.country"),
                fmt("user.language"),
                fmt("user.script"),
                fmt("user.variant"),
            ]
        })
    };

    // Allocate a String[] of FIXED_LENGTH (40 elements), all null initially.
    // i7-L2: typed `String[]` (the declared type, never `Object[]`) and
    // rooted, because every `create_string` below can move it.
    let string_component = crate::lang_class::loading_component_id(ctx, "java/lang/String");
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let arr = scope.new_ref_array(
        string_component,
        if legacy_layout {
            FIXED_LENGTH - 1
        } else {
            FIXED_LENGTH
        },
    );
    let arr_h = scope.root(arr);

    // Helper: set a string at the given index
    let mut set = |idx: usize, val: &str| {
        let idx = if legacy_layout {
            match jdk21_ndx(idx) {
                Some(i) => i,
                None => return,
            }
        } else {
            idx
        };
        let s = scope.create_string(val);
        let arr = scope.get(&arr_h);
        scope.set_array_element(arr, idx, Value::Object(Some(s)));
    };

    // --- Locale (display + format) ---
    // Read back from the VM's property map (see `locale_slots` above); a null
    // slot is "not set", which is what the Unix platform layer answers for an
    // absent subtag. With no map (a caller that never seeded one) this answers
    // exactly what it did before: `en` / `US`, script and variant null.
    match locale_slots {
        Some(slots) => {
            let ndx = [
                DISPLAY_COUNTRY_NDX,
                DISPLAY_LANGUAGE_NDX,
                DISPLAY_SCRIPT_NDX,
                DISPLAY_VARIANT_NDX,
                FORMAT_COUNTRY_NDX,
                FORMAT_LANGUAGE_NDX,
                FORMAT_SCRIPT_NDX,
                FORMAT_VARIANT_NDX,
            ];
            for (i, v) in ndx.iter().zip(slots.iter()) {
                if let Some(v) = v {
                    set(*i, v);
                }
            }
        }
        None => {
            set(DISPLAY_COUNTRY_NDX, "US");
            set(DISPLAY_LANGUAGE_NDX, "en");
            set(FORMAT_COUNTRY_NDX, "US");
            set(FORMAT_LANGUAGE_NDX, "en");
        }
    }

    // --- File system ---
    #[cfg(windows)]
    {
        set(FILE_SEPARATOR_NDX, "\\");
        set(PATH_SEPARATOR_NDX, ";");
        set(LINE_SEPARATOR_NDX, "\r\n");
    }
    #[cfg(not(windows))]
    {
        set(FILE_SEPARATOR_NDX, "/");
        set(PATH_SEPARATOR_NDX, ":");
        set(LINE_SEPARATOR_NDX, "\n");
    }

    // --- OS ---
    set(OS_NAME_NDX, std::env::consts::OS);
    set(OS_ARCH_NDX, std::env::consts::ARCH);
    // os.version: query the actual version
    #[cfg(windows)]
    {
        // Windows reports "10.0" for Win10/11
        set(OS_VERSION_NDX, "10.0");
    }
    #[cfg(not(windows))]
    {
        set(OS_VERSION_NDX, "6.1");
    }

    // --- Temp dir ---
    // HotSpot's derivation, which ignores `$TMPDIR` on Linux; see
    // `cratonvm_native_api::os_tmpdir`.
    set(
        JAVA_IO_TMPDIR_NDX,
        &cratonvm_native_api::os_tmpdir::java_io_tmpdir(),
    );

    // --- Encoding ---
    // Every derived key is read back out of the property map, never restated.
    // Three tables hold these six keys — this one, `vmProperties()` below, and
    // `SharedVm::new`'s `sys_props` in `vm/src/vm/vm_init.rs` — and a literal in
    // any of them is a second answer to a question another one already answers.
    // That is not hypothetical: two hard-coded copies under a comment asserting
    // "JDK 18+ pinned to UTF-8" is precisely how that false premise survived
    // unexamined until 2026-09-01.
    //
    // The three STREAM keys are INERT here — `SystemProps.initProperties`
    // reaches these indexes only through `putIfAbsent`, and `vmProperties()`
    // already supplies all three, so whatever is written here is overridden
    // before anything can read it. They are still read back rather than pinned,
    // because "inert" is a property of today's `vmProperties()` and not of this
    // function: the moment that table stops supplying one, a literal `"UTF-8"`
    // here would silently become the answer and undo the derivation, leaving no
    // trace in the key anyone would think to inspect. The stream encodings
    // stopped being UTF-8 on 2026-09-01 (§10 of
    // docs/known-issues/stdout-encoding-differs-from-hotspot-on-windows-20260901.md).
    //
    // `sun.jnu.encoding` is the one key still PINNED, and for a reason rather
    // than by deferral: it decides how FILE NAMES are encoded, so moving it
    // changes class loading rather than printing. It is also, with
    // `native.encoding`, one of only two keys `initProperties` `put`s
    // unconditionally from this table — so unlike the three above, a literal
    // here is the answer, not a shadowed one. §11 of the page above carries it.
    set(NATIVE_ENCODING_NDX, native_encoding.as_str());
    set(STDOUT_ENCODING_NDX, stdout_encoding.as_str());
    set(STDERR_ENCODING_NDX, stderr_encoding.as_str());
    set(STDIN_ENCODING_NDX, stdin_encoding.as_str());
    set(SUN_JNU_ENCODING_NDX, native_encoding.as_str());
    set(SUN_IO_UNICODE_ENCODING_NDX, "UnicodeLittle");

    // --- CPU / architecture ---
    #[cfg(target_pointer_width = "64")]
    set(SUN_ARCH_DATA_MODEL_NDX, "64");
    #[cfg(target_pointer_width = "32")]
    set(SUN_ARCH_DATA_MODEL_NDX, "32");

    #[cfg(target_endian = "little")]
    set(SUN_CPU_ENDIAN_NDX, "little");
    #[cfg(target_endian = "big")]
    set(SUN_CPU_ENDIAN_NDX, "big");

    // --- User ---
    if let Ok(dir) = std::env::current_dir() {
        set(USER_DIR_NDX, &dir.to_string_lossy());
    }
    if let Ok(home) = cratonvm_types::flags::runtime_var("USERPROFILE")
        .or_else(|_| cratonvm_types::flags::runtime_var("HOME"))
    {
        set(USER_HOME_NDX, &home);
    }
    if let Ok(user) = cratonvm_types::flags::runtime_var("USERNAME")
        .or_else(|_| cratonvm_types::flags::runtime_var("USER"))
    {
        set(USER_NAME_NDX, &user);
    }

    Ok(Some(Value::Object(Some(scope.get(&arr_h)))))
}

/// `jdk/internal/util/SystemProps$Raw.vmProperties()[Ljava/lang/String;`
///
/// Returns a `String[]` of interleaved key-value pairs:
///   `[key0, val0, key1, val1, ...]`
///
/// The JDK's `SystemProps$Raw.cmdProperties()` iterates this array in pairs,
/// building a HashMap. A null key terminates the iteration.
///
/// These properties come from the VM (command-line flags, built-in defaults).
fn native_vm_properties(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let mut props: Vec<(&str, String)> = Vec::new();

    // java.home — required by StaticProperty, ClassLoader, etc.
    if let Some(java_home) = ctx.get_system_property("java.home") {
        props.push(("java.home", java_home));
    } else if let Ok(jh) = cratonvm_types::flags::runtime_var("JAVA_HOME") {
        props.push(("java.home", jh));
    }

    // Interpreter round i2 wave 63, lane L1: the VM's identity keys come from
    // the VM's own property map (`vm_init::vm_identity_properties`), the one
    // JVMTI `GetSystemProperty` answers from, so an agent and
    // `System.getProperty` read the same value (MEASURED on HotSpot 25.0.3:
    // `same=1` for each; `L1W61JvmtiAgentStartup`). Interpreter round i6
    // wave 118, lane L1 (i61-L1 stage 2): a context without that map answers
    // the one table of fixed values, `cratonvm_types::vm_properties::VM_IDENTITY`
    // (its own literals here said `CratonVM` / `Craton` / `0.2.0` where the
    // map says `cratonvm` / `cratonvm` / `25.0.1+8`). In the list's order;
    // `jdk.debug` is added below with the other build keys.
    for (key, fixed) in cratonvm_types::vm_properties::VM_IDENTITY {
        if key == "jdk.debug" {
            continue;
        }
        let value = ctx
            .get_system_property(key)
            .unwrap_or_else(|| fixed.to_string());
        props.push((key, value));
    }
    // `java.vm.info` is the launch's execution mode: no fixed value.
    let info = ctx
        .get_system_property("java.vm.info")
        .unwrap_or_else(|| "mixed mode".to_string());
    props.push(("java.vm.info", info));

    // Specification
    props.push((
        "java.specification.name",
        "Java Platform API Specification".to_string(),
    ));
    props.push((
        "java.specification.vendor",
        "Oracle Corporation".to_string(),
    ));
    props.push(("java.specification.version", "25".to_string()));

    // Class / library paths
    if let Some(cp) = ctx.get_system_property("java.class.path") {
        props.push(("java.class.path", cp));
    }
    // Core separators can be queried very early by java.io bootstrap code
    // (e.g. WinNTFileSystem.<clinit>), so keep them present in vmProperties.
    if let Some(v) = ctx.get_system_property("file.separator") {
        props.push(("file.separator", v));
    }
    if let Some(v) = ctx.get_system_property("path.separator") {
        props.push(("path.separator", v));
    }
    if let Some(v) = ctx.get_system_property("line.separator") {
        props.push(("line.separator", v));
    }
    if let Some(v) = ctx.get_system_property("user.dir") {
        props.push(("user.dir", v));
    }
    if let Some(java_home) = ctx.get_system_property("java.home") {
        // java.library.path: needed for System.loadLibrary. Interpreter round
        // i2 wave 63, lane L1: the VM map's value when it has one, the one
        // JVMTI `GetSystemProperty` answers.
        if let Some(lib_path) = ctx.get_system_property("java.library.path") {
            props.push(("java.library.path", lib_path));
        } else {
            #[cfg(windows)]
            {
                let lib_path = format!("{java_home}\\bin");
                props.push(("java.library.path", lib_path));
            }
            #[cfg(not(windows))]
            {
                let lib_path = format!("{java_home}/lib");
                props.push(("java.library.path", lib_path));
            }
        }
        // sun.boot.library.path: the VM map's value too (the i53 residual:
        // HotSpot appends an explicit `-Dsun.boot.library.path` to the
        // default, which `vm_init` does in the map).
        if let Some(boot_path) = ctx.get_system_property("sun.boot.library.path") {
            props.push(("sun.boot.library.path", boot_path));
        } else {
            #[cfg(windows)]
            props.push(("sun.boot.library.path", format!("{java_home}\\bin")));
            #[cfg(not(windows))]
            props.push(("sun.boot.library.path", format!("{java_home}/lib")));
        }
    }

    // Boot class path (empty for modular JDK)
    props.push(("sun.boot.class.path", String::new()));

    // java.version, java.class.version
    props.push(("java.version", "25.0.1".to_string()));
    props.push(("java.class.version", "69.0".to_string()));
    props.push(("java.runtime.version", "25.0.1+8-LTS-27".to_string()));
    props.push((
        "java.runtime.name",
        "Java(TM) SE Runtime Environment".to_string(),
    ));
    props.push(("java.vendor", "Craton / CratonVM".to_string()));
    props.push((
        "java.vendor.url",
        "https://github.com/nicktretyakov/cratonvm".to_string(),
    ));

    // Properties HotSpot 25 sets that this table did not, found 2026-08-04 by
    // diffing `System.getProperties()` against a HotSpot 25 control (45 keys
    // against 48). Each is read by real library code, not only by
    // `-XshowSettings`:
    //
    // * `sun.cpu.endian` — Netty, Chronicle and several serialization libraries
    //   branch on it, and code that finds it absent usually assumes big-endian,
    //   which is wrong on every machine this runs on.
    // * `sun.io.unicode.encoding` — read by `java.io.ObjectStreamClass` and by
    //   the older text codecs.
    // * `java.version.date`, `jdk.debug`, `sun.management.compiler`,
    //   `java.vm.compressedOopsMode` — informational, but they appear in crash
    //   reports, `RuntimeMXBean` dumps and support bundles, and their absence is
    //   what makes a CratonVM dump obviously not a JVM dump.
    //
    // `sun.java.command` and `sun.java.launcher` are deliberately NOT here:
    // only the launcher knows them, and it sets them (`vm-cli`).
    //
    // NOTE, and it cost a build to find out: this table is **not** the one that
    // reaches `System.getProperties()` in real-JDK mode. `SharedVm::new`'s
    // `sys_props` in `vm/src/vm/vm_init.rs` is, and the two overlap without
    // agreeing — the tell is `java.vm.name`, "CratonVM" here and "cratonvm"
    // there, and a real-JDK run reports the lower-case one. The six keys below
    // are in both. **Add a key to both or you will add it to neither.**
    //
    // Amended 2026-09-01: there are THREE tables, not two.
    // `native_platform_properties` above is the third, and for
    // `native.encoding` and `sun.jnu.encoding` it is the one that WINS —
    // `SystemProps.initProperties` `put`s those two from the platform indexes
    // unconditionally, after this table has already been read. The encoding
    // block further down records which copy governs which key. `native.encoding`
    // is now derived exactly once, in
    // `vm/src/vm/vm_init.rs::derive_native_encoding`, and read back by both
    // tables in this file, so that key at least cannot drift between the
    // three. The other five are still literals wherever they appear, on
    // purpose — see the vm_init table for why.
    props.push((
        "sun.cpu.endian",
        if cfg!(target_endian = "big") {
            "big".to_string()
        } else {
            "little".to_string()
        },
    ));
    props.push(("sun.io.unicode.encoding", "UnicodeLittle".to_string()));
    props.push(("java.version.date", "2025-10-21".to_string()));
    // Wave 118 (i61-L1 stage 2): the identity table's value.
    let debug = cratonvm_types::vm_properties::vm_identity_value("jdk.debug").unwrap_or("release");
    props.push((
        "jdk.debug",
        ctx.get_system_property("jdk.debug")
            .unwrap_or_else(|| debug.to_string()),
    ));
    props.push(("sun.management.compiler", "CratonVM JIT".to_string()));
    props.push(("java.vm.compressedOopsMode", "Zero based".to_string()));

    // Misc
    props.push(("java.awt.headless", "true".to_string()));
    // Only `file.encoding` is pinned to UTF-8 (JEP 400, JDK 18). The stream
    // encodings and `native.encoding` follow the host — see
    // `cratonvm_native_api::os_encoding`, and the twin table in
    // `vm/src/vm/vm_init.rs` that this one has to agree with.
    props.push(("file.encoding", "UTF-8".to_string()));
    // NOT SET, and their absence is the point: `sun.stdout.encoding` and
    // `sun.stderr.encoding`. HotSpot 25 leaves both NULL — measured on the
    // audit host in a UTF-8 arm and an `LC_ALL=C` arm alike — and until
    // 2026-09-01 this table pinned both to `UTF-8`.
    //
    // They are the trap for whoever does the `stdout.encoding` stage.
    // `SystemProps.initProperties` consults them FIRST:
    //
    //     putIfAbsent(props, "stdout.encoding",
    //         props.getOrDefault("sun.stdout.encoding",
    //                            raw.propDefault(Raw._stdout_encoding_NDX)));
    //
    // so a pinned `sun.stdout.encoding` overrides a correctly derived platform
    // value and silently undoes that fix, leaving no trace in the key anyone
    // would think to inspect.
    //
    // Removing them was checked rather than assumed to be safe: a grep over
    // the whole tree (`*.rs`, `*.java`, `*.md`, `*.txt`) finds no reader at
    // all outside `probes/StdoutEncoding.java`, which prints the key precisely
    // in order to observe that HotSpot leaves it null.
    //
    // The stage that trap was waiting for landed the same day: `stdout.encoding`
    // and `stderr.encoding` are now DERIVED, and this table reads the answer
    // back out of `vm_init.rs`'s map rather than restating it, so the three
    // copies of these keys cannot disagree. If the map has no such key —
    // built-in mode, or a caller that never seeded it — answer exactly what
    // this line answered before 2026-09-01.
    props.push((
        "stdout.encoding",
        ctx.get_system_property("stdout.encoding")
            .unwrap_or_else(|| "UTF-8".to_string()),
    ));
    props.push((
        "stderr.encoding",
        ctx.get_system_property("stderr.encoding")
            .unwrap_or_else(|| "UTF-8".to_string()),
    ));
    // Session 108: stdin.encoding is consulted by `java/io/Console.<clinit>`
    // (JDK 21+) when computing STDIN_CHARSET. The bytecode is
    // `Charset.forName(System.getProperty("stdin.encoding"), UTF_8)` which
    // *would* fall back to UTF_8 only when `lookup(name)` throws an
    // IllegalCharsetNameException — but `lookup(null)` throws
    // IllegalArgumentException("Null charset name") which is NOT caught,
    // tripping a Console.<clinit> swallow in real-JDK mode. Setting the
    // property explicitly mirrors what HotSpot's launcher native code does.
    props.push((
        "stdin.encoding",
        ctx.get_system_property("stdin.encoding")
            .unwrap_or_else(|| "UTF-8".to_string()),
    ));
    // Derived, and read back from the one owner rather than restated: see
    // `vm/src/vm/vm_init.rs::derive_native_encoding`, and the note at the top
    // of `native_platform_properties` for why the copy THERE is the one that
    // wins in real-JDK mode (this entry survives only if that index comes back
    // null). Two hard-coded copies of the same six keys in two crates is how
    // the false "JDK 18+ pinned to UTF-8" premise stayed unexamined; the same
    // fallback rule applies as above — if the map has no such key, answer
    // exactly what this line answered before 2026-09-01.
    props.push((
        "native.encoding",
        ctx.get_system_property("native.encoding")
            .unwrap_or_else(|| "UTF-8".to_string()),
    ));
    props.push((
        "sun.jnu.encoding",
        ctx.get_system_property("sun.jnu.encoding")
            .unwrap_or_else(|| "UTF-8".to_string()),
    ));

    // --- NIO / ZIP toggles to steer the JDK away from native-memory code
    // paths that depend on FileChannelImpl.map0 / direct buffers backed by
    // Unsafe.allocateMemory raw addresses. JDK 25's ZipFile$Source no
    // longer uses mmap (JDK-8226340, JDK 14+), but other code paths
    // (URLClassLoader, Panama, etc.) can still trigger direct-buffer
    // allocations through sun.nio.ch. These flags force safer paths
    // where available:
    //   - sun.zip.disableMemoryMapping: kept for back-compat; harmless.
    //   - jdk.nio.enableFastFileTransfer=false: disables the mapped
    //     FileChannel.transferTo fast path.
    //   - sun.nio.PageAlignDirectMemory is NOT set (SBR3 niobuf, 2026-10-04).
    //     It used to be `true`, which made `VM.isDirectMemoryPageAligned()`
    //     true and every `DirectByteBuffer` reserve `capacity + 4096`
    //     (`BufferPoolMXBean("direct").getMemoryUsed()` n+4096 where HotSpot
    //     answers n; see docs/internal/jdk-only/SBR3-NIOBUF-*). The unaligned
    //     path is the one HotSpot runs and arena handles need no alignment.
    // HotSpot hands `VM.saveProperties` the `-XX:MaxDirectMemorySize` value as
    // `sun.nio.MaxDirectMemorySize` (and `System.getProperty` shows none of it).
    // Unset, the real `VM.maxDirectMemory()` is `Runtime.maxMemory()`, which is
    // the same number `vm_init` stores as the cap, so the value is passed
    // unconditionally. The real `Bits.reserveMemory` (retired shadow, SBR3
    // niobuf) enforces the cap through it; the VM's own counter used to.
    props.push((
        "sun.nio.MaxDirectMemorySize",
        cratonvm_native_io::direct_buffer::max_direct_memory_bytes().to_string(),
    ));
    props.push(("sun.zip.disableMemoryMapping", "true".to_string()));
    props.push(("jdk.nio.enableFastFileTransfer", "false".to_string()));

    // Collect user-defined -D properties (e.g. -Des.path.home=...) not
    // already in the fixed list above. Without this, System.getProperty()
    // returns null for CLI-supplied properties because vmProperties() is the
    // sole source that populates System.props at JDK startup.
    let already: std::collections::HashSet<&str> = props.iter().map(|(k, _)| *k).collect();
    let user_props: Vec<(String, String)> = ctx
        .list_system_properties()
        .into_iter()
        .filter(|(k, _)| !already.contains(k.as_str()))
        .collect();

    // Build the interleaved String[] array: [key0, val0, key1, val1, ...]
    // i7-L2: typed `String[]` and rooted -- each `create_string` can move the
    // array, and `k` must survive the allocation of `v`, so each string is
    // stored before the next is made.
    let total = props.len() + user_props.len();
    let string_component = crate::lang_class::loading_component_id(ctx, "java/lang/String");
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let arr = scope.new_ref_array(string_component, total * 2);
    let arr_h = scope.root(arr);
    let pairs = props
        .iter()
        .map(|(k, v)| (*k, v.as_str()))
        .chain(user_props.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    for (i, (key, val)) in pairs.enumerate() {
        for (slot, text) in [(i * 2, key), (i * 2 + 1, val)] {
            let s = scope.create_string(text);
            let arr = scope.get(&arr_h);
            scope.set_array_element(arr, slot, Value::Object(Some(s)));
        }
    }

    Ok(Some(Value::Object(Some(scope.get(&arr_h)))))
}

/// `sun/io/Win32ErrorMode.setErrorMode(J)J` — Windows error mode control.
/// Returns the previous error mode. We return 0 (no previous mode set).
fn native_win32_set_error_mode(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Long(0)))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all T14 System bootstrap natives needed for `initPhase1` to
/// execute real JDK 25 bytecode successfully.
pub fn register_t14_system_bootstrap(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    // SystemProps$Raw — platform and VM properties
    // SBR5 sysprops: the JDK 21 image's `Raw` indexes differ (see `jdk21_ndx`).
    let platform_properties: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        if registry.real_jdk_older_than(25) {
            native_platform_properties_jdk21
        } else {
            native_platform_properties
        };
    registry.register_with_kind(
        "jdk/internal/util/SystemProps$Raw",
        "platformProperties",
        "()[Ljava/lang/String;",
        platform_properties,
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        "jdk/internal/util/SystemProps$Raw",
        "vmProperties",
        "()[Ljava/lang/String;",
        native_vm_properties,
        NativeKind::Bridge,
    );

    // FileDescriptor / FileInputStream / FileOutputStream — initIDs noops
    // These are called by <clinit> on first class load; they initialize
    // JNI field IDs in HotSpot but we resolve fields by name, so noop.
    registry.register_with_kind(
        "java/io/FileDescriptor",
        "initIDs",
        "()V",
        crate::native_noop,
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        "java/io/FileInputStream",
        "initIDs",
        "()V",
        crate::native_noop,
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        "java/io/FileOutputStream",
        "initIDs",
        "()V",
        crate::native_noop,
        NativeKind::Bridge,
    );

    // Win32ErrorMode — Windows-only, safe noop returning 0
    registry.register_with_kind(
        "sun/io/Win32ErrorMode",
        "setErrorMode",
        "(J)J",
        native_win32_set_error_mode,
        NativeKind::Bridge,
    );
    registry.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn t14_platform_properties_returns_correct_length() {
        let mut ctx = MockNativeContext::new();
        let result = native_platform_properties(&mut ctx, &[]).unwrap();
        let arr = match result {
            Some(Value::Object(Some(arr))) => arr,
            other => panic!("expected Object(Some(_)), got {:?}", other),
        };
        let len = ctx.array_length(arr);
        assert_eq!(
            len, FIXED_LENGTH,
            "platformProperties must return {FIXED_LENGTH}-element array"
        );
    }

    #[test]
    fn t14_platform_properties_has_os_name() {
        let mut ctx = MockNativeContext::new();
        let result = native_platform_properties(&mut ctx, &[]).unwrap();
        let arr = match result {
            Some(Value::Object(Some(arr))) => arr,
            other => panic!("expected Object(Some(_)), got {:?}", other),
        };
        // OS_NAME_NDX = 21
        let val = ctx.get_array_element(arr, OS_NAME_NDX);
        match val {
            Value::Object(Some(_)) => {} // has a string
            other => panic!(
                "os.name at index {OS_NAME_NDX} should be a string, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn t14_platform_properties_file_separator() {
        let mut ctx = MockNativeContext::new();
        let result = native_platform_properties(&mut ctx, &[]).unwrap();
        let arr = match result {
            Some(Value::Object(Some(arr))) => arr,
            other => panic!("expected Object(Some(_)), got {:?}", other),
        };
        // FILE_SEPARATOR_NDX = 4 — must be a non-null string
        let val = ctx.get_array_element(arr, FILE_SEPARATOR_NDX);
        match val {
            Value::Object(Some(_)) => {} // has a string
            other => panic!(
                "file.separator at index {FILE_SEPARATOR_NDX} should be a string, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn t14_vm_properties_returns_pairs() {
        let mut ctx = MockNativeContext::new();
        let result = native_vm_properties(&mut ctx, &[]).unwrap();
        let arr = match result {
            Some(Value::Object(Some(arr))) => arr,
            other => panic!("expected Object(Some(_)), got {:?}", other),
        };
        let len = ctx.array_length(arr);
        assert!(
            len >= 2,
            "vmProperties must return at least one key-value pair"
        );
        assert_eq!(
            len % 2,
            0,
            "vmProperties must return even-length array (key-value pairs)"
        );
    }

    /// i7-L2: both `SystemProps$Raw` natives answer `String[]`, their declared
    /// type, and `vmProperties` keeps every key and value in order.
    #[test]
    fn i7_l2_system_props_arrays_are_string_typed() {
        let mut ctx = MockNativeContext::new();
        let string = ctx
            .ensure_class_initialized("java/lang/String")
            .expect("mock registers the class");
        let platform = match native_platform_properties(&mut ctx, &[]) {
            Ok(Some(Value::Object(Some(arr)))) => arr,
            other => panic!("expected an array, got {other:?}"),
        };
        let vm = match native_vm_properties(&mut ctx, &[]) {
            Ok(Some(Value::Object(Some(arr)))) => arr,
            other => panic!("expected an array, got {other:?}"),
        };
        assert_eq!(ctx.ref_array_component(platform), Some(string));
        assert_eq!(ctx.ref_array_component(vm), Some(string));
        let first_key = match ctx.get_array_element(vm, 0) {
            Value::Object(Some(s)) => ctx.read_string(s),
            other => panic!("vmProperties[0] must be a key, got {other:?}"),
        };
        let first_value = match ctx.get_array_element(vm, 1) {
            Value::Object(Some(s)) => ctx.read_string(s),
            other => panic!("vmProperties[1] must be a value, got {other:?}"),
        };
        assert!(first_key.is_some() && first_value.is_some());
        for i in 0..ctx.array_length(vm) {
            assert!(
                matches!(ctx.get_array_element(vm, i), Value::Object(Some(_))),
                "vmProperties slot {i} is null"
            );
        }
    }

    #[test]
    fn t14_win32_error_mode_returns_zero() {
        let mut ctx = MockNativeContext::new();
        let result = native_win32_set_error_mode(&mut ctx, &[Value::Long(0x8001)]).unwrap();
        assert_eq!(result, Some(Value::Long(0)));
    }
}
