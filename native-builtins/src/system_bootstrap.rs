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
use cratonvm_types::{ArrayElementType, Value};

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
    // Allocate a String[] of FIXED_LENGTH (40 elements), all null initially
    let arr = ctx.new_array(ArrayElementType::Reference, FIXED_LENGTH);

    // Helper: set a string at the given index
    let mut set = |idx: usize, val: &str| {
        let s = ctx.create_string(val);
        ctx.set_array_element(arr, idx, Value::Object(Some(s)));
    };

    // --- Locale (display + format) ---
    // Use "en" / "US" as sensible defaults; JDK will refine from env vars.
    set(DISPLAY_COUNTRY_NDX, "US");
    set(DISPLAY_LANGUAGE_NDX, "en");
    // DISPLAY_SCRIPT_NDX, DISPLAY_VARIANT_NDX: leave null
    set(FORMAT_COUNTRY_NDX, "US");
    set(FORMAT_LANGUAGE_NDX, "en");
    // FORMAT_SCRIPT_NDX, FORMAT_VARIANT_NDX: leave null

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
    let tmp = std::env::temp_dir();
    set(JAVA_IO_TMPDIR_NDX, &tmp.to_string_lossy());

    // --- Encoding ---
    set(NATIVE_ENCODING_NDX, "UTF-8");
    set(STDOUT_ENCODING_NDX, "UTF-8");
    set(STDERR_ENCODING_NDX, "UTF-8");
    set(STDIN_ENCODING_NDX, "UTF-8");
    set(SUN_JNU_ENCODING_NDX, "UTF-8");
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

    Ok(Some(Value::Object(Some(arr))))
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

    // Spec / VM vendor info
    props.push((
        "java.vm.specification.name",
        "Java Virtual Machine Specification".to_string(),
    ));
    props.push((
        "java.vm.specification.vendor",
        "Oracle Corporation".to_string(),
    ));
    props.push(("java.vm.specification.version", "25".to_string()));
    props.push(("java.vm.name", "CratonVM".to_string()));
    props.push(("java.vm.vendor", "Craton".to_string()));
    props.push(("java.vm.version", "0.2.0".to_string()));
    props.push(("java.vm.info", "mixed mode".to_string()));

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
        // java.library.path — needed for System.loadLibrary
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
        // sun.boot.library.path
        #[cfg(windows)]
        props.push(("sun.boot.library.path", format!("{java_home}\\bin")));
        #[cfg(not(windows))]
        props.push(("sun.boot.library.path", format!("{java_home}/lib")));
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
    props.push(("jdk.debug", "release".to_string()));
    props.push(("sun.management.compiler", "CratonVM JIT".to_string()));
    props.push(("java.vm.compressedOopsMode", "Zero based".to_string()));

    // Misc
    props.push(("java.awt.headless", "true".to_string()));
    props.push(("file.encoding", "UTF-8".to_string()));
    props.push(("sun.stdout.encoding", "UTF-8".to_string()));
    props.push(("sun.stderr.encoding", "UTF-8".to_string()));
    props.push(("stdout.encoding", "UTF-8".to_string()));
    props.push(("stderr.encoding", "UTF-8".to_string()));
    // Session 108: stdin.encoding is consulted by `java/io/Console.<clinit>`
    // (JDK 21+) when computing STDIN_CHARSET. The bytecode is
    // `Charset.forName(System.getProperty("stdin.encoding"), UTF_8)` which
    // *would* fall back to UTF_8 only when `lookup(name)` throws an
    // IllegalCharsetNameException — but `lookup(null)` throws
    // IllegalArgumentException("Null charset name") which is NOT caught,
    // tripping a Console.<clinit> swallow in real-JDK mode. Setting the
    // property explicitly mirrors what HotSpot's launcher native code does.
    props.push(("stdin.encoding", "UTF-8".to_string()));
    props.push(("native.encoding", "UTF-8".to_string()));
    props.push(("sun.jnu.encoding", "UTF-8".to_string()));

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
    //   - sun.nio.PageAlignDirectMemory=true: aligns direct buffers so
    //     our Unsafe.allocateMemory-backed addresses remain valid even
    //     when the JDK assumes page-aligned layout.
    props.push(("sun.zip.disableMemoryMapping", "true".to_string()));
    props.push(("jdk.nio.enableFastFileTransfer", "false".to_string()));
    props.push(("sun.nio.PageAlignDirectMemory", "true".to_string()));

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
    let total = props.len() + user_props.len();
    let arr = ctx.new_array(ArrayElementType::Reference, total * 2);
    for (i, (key, val)) in props.iter().enumerate() {
        let k = ctx.create_string(key);
        let v = ctx.create_string(val);
        ctx.set_array_element(arr, i * 2, Value::Object(Some(k)));
        ctx.set_array_element(arr, i * 2 + 1, Value::Object(Some(v)));
    }
    let base = props.len();
    for (i, (key, val)) in user_props.iter().enumerate() {
        let k = ctx.create_string(key);
        let v = ctx.create_string(val);
        ctx.set_array_element(arr, (base + i) * 2, Value::Object(Some(k)));
        ctx.set_array_element(arr, (base + i) * 2 + 1, Value::Object(Some(v)));
    }

    Ok(Some(Value::Object(Some(arr))))
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
    registry.register_with_kind(
        "jdk/internal/util/SystemProps$Raw",
        "platformProperties",
        "()[Ljava/lang/String;",
        native_platform_properties,
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

    #[test]
    fn t14_win32_error_mode_returns_zero() {
        let mut ctx = MockNativeContext::new();
        let result = native_win32_set_error_mode(&mut ctx, &[Value::Long(0x8001)]).unwrap();
        assert_eq!(result, Some(Value::Long(0)));
    }
}
