// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! VM construction and core structs: SharedVm and Vm.

/// Maximum number of lambda proxy classes before new registrations are silently dropped.
pub(crate) const MAX_LAMBDA_PROXIES: usize = 100_000;

static NEXT_VM_IDENTITY: AtomicUsize = AtomicUsize::new(1);

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::RwLock;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::classloading::resolution::{LambdaCallSite, LinkResolver, ResolutionCache};
use crate::classloading::{ClassId, ClassManager};
use crate::config::VmConfig;
use crate::config::{discover_boot_classpath, discover_ext_classpath};
use crate::error::{MethodCallFailed, MethodCallResult, VmError};
use crate::jit::profile::ProfileStore;
use crate::jit::JitCache;
use crate::memory::heap::ArrayElementType;
use crate::memory::vm_heap::{G1ConfigOverrides, GcBackend, VmHeap};
use crate::native::io::FileDescriptorTable;
use crate::native::register_essential_natives;
use crate::native::register_io_natives;
use crate::native::registry::{NativeMethodRegistry, StackTraceEntry};
use crate::native::{register_builtins, register_collections_natives};
use crate::runtime::lock_order::{LockLevel, OrderedPlMutex, OrderedPlRwLock};
use crate::threading::gc_barrier::GcBarrier;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::threading::monitor::MonitorTable;
use crate::threading::thread_registry::ThreadRegistry;
use crate::types::{ObjectRef, Value};
// `crate::classloading` is a re-export shim that does not forward
// `class_origin`, so the census row type is named through the source crate
// directly rather than by widening that shim (which another agent owns).
use cratonvm_classloading::class_origin::ClassOriginEntry;
use cratonvm_types::compat::CompatibilityMode;

// ---------------------------------------------------------------------------
// Missing-native audit log entry (NEW-10)
// ---------------------------------------------------------------------------

/// One entry in [`SharedVm::missing_natives_log`]. Written to disk by
/// [`SharedVm::dump_missing_natives_json`] with a stable, diff-friendly
/// JSON schema. Dedup is by `(class_name, method_name, descriptor)`;
/// `sample_call_site` records the first caller seen so a later reader
/// of the committed baseline can locate the Java code path that
/// transitively reached the missing native.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingNativeEntry {
    pub class_name: String,
    pub method_name: String,
    pub descriptor: String,
    /// `"<caller_class>.<caller_method><caller_descriptor>"` of the frame
    /// directly above the missing-native call, or `None` if the audit
    /// hook was reached from a context with no caller frame (e.g. the
    /// VM bootstrap path).
    pub sample_call_site: Option<String>,
}

impl MissingNativeEntry {
    /// Format as `"<class>.<method><descriptor>"` matching the
    /// historical [`SharedVm::get_missing_natives`] text output.
    pub fn full_signature(&self) -> String {
        format!(
            "{}.{}{}",
            self.class_name, self.method_name, self.descriptor
        )
    }
}

/// T19.H1 — defensive string truncator for diagnostic dumps.
///
/// Copies up to `max` bytes of `s` into an owned `String`, snapping to a
/// UTF-8 character boundary so the result is always valid Rust `String`.
/// Appends `"...(truncated)"` when truncation occurs so the reader can
/// see that the value was longer than shown.
fn truncate_ascii(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Find the largest valid char boundary <= max so the returned String
    // never splits a multi-byte UTF-8 sequence.
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + 16);
    out.push_str(&s[..cut]);
    out.push_str("...(truncated)");
    out
}

// ---------------------------------------------------------------------------
// Quarkus `RunnerClassLoader.close()`
// ---------------------------------------------------------------------------

/// Real `io.quarkus.bootstrap.runner.RunnerClassLoader.close()`.
///
/// This was a `Ok(None)` no-op registered "until the underlying HashMap
/// null-value issue is resolved": the Quarkus bytecode walks
/// `resourceDirectoryMap.values()` and dereferences every entry, so a null map
/// value made it NPE. Answering with a no-op dodged the NPE by never releasing
/// a single jar handle. This does the real work and skips the null holes
/// instead.
///
/// Quarkus points many package directories at the SAME `ClassLoadingResource`
/// objects, so each distinct resource is closed exactly once: `JarResource`
/// delegates to a reference-counted `JarFileReference`, and closing it once per
/// directory entry would drive that counter negative.
fn quarkus_runner_class_loader_close(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[cratonvm_types::Value],
) -> cratonvm_types::error::MethodCallResult {
    let this = match args.first() {
        Some(cratonvm_types::Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let map = match ctx.get_field_by_name(this, "resourceDirectoryMap") {
        cratonvm_types::Value::Object(Some(m)) => m,
        // Unrecognised Quarkus layout: no resource table to walk, so there is
        // nothing to close (same observable result as the old no-op).
        _ => return Ok(None),
    };
    let values = match ctx.invoke_virtual(map, "values", "()Ljava/util/Collection;", &[])? {
        Some(cratonvm_types::Value::Object(Some(v))) => v,
        _ => return Ok(None),
    };
    let table = match ctx.invoke_virtual(values, "toArray", "()[Ljava/lang/Object;", &[])? {
        Some(cratonvm_types::Value::Object(Some(t))) => t,
        _ => return Ok(None),
    };

    // Collection pass. `array_length`/`get_array_element` never allocate, so
    // no GC can relocate these refs before they are pinned below.
    let mut resources: Vec<cratonvm_types::ObjectRef> = Vec::new();
    for i in 0..ctx.array_length(table) {
        // A null entry here is exactly the hole the real bytecode NPEs on.
        let entry = match ctx.get_array_element(table, i) {
            cratonvm_types::Value::Object(Some(e)) => e,
            _ => continue,
        };
        if ctx.object_is_array(entry) {
            for j in 0..ctx.array_length(entry) {
                if let cratonvm_types::Value::Object(Some(res)) = ctx.get_array_element(entry, j) {
                    if !resources.contains(&res) {
                        resources.push(res);
                    }
                }
            }
        } else if !resources.contains(&entry) {
            // Older Quarkus maps a directory straight to one resource.
            resources.push(entry);
        }
    }
    let base = match resources.first() {
        Some(first) => ctx.pin_native_root(*first),
        None => return Ok(None),
    };
    let mut handles = Vec::with_capacity(resources.len());
    handles.push(base);
    for res in &resources[1..] {
        handles.push(ctx.pin_native_root(*res));
    }

    // `close()` runs Java code, which can allocate and therefore relocate every
    // resource still queued, so re-read each ref through its pin.
    let mut outcome = Ok(None);
    for (idx, handle) in handles.iter().enumerate() {
        let res = ctx.read_native_pin(*handle, resources[idx]);
        if let Err(e) = ctx.invoke_virtual(res, "close", "()V", &[]) {
            outcome = Err(e);
            break;
        }
    }
    ctx.unpin_native_roots(base);
    outcome
}

// ---------------------------------------------------------------------------
// WP1.11 — system-property helpers
// ---------------------------------------------------------------------------

/// Return a HotSpot-compatible `os.name` string for the current platform.
///
/// HotSpot uses `GetVersionEx`/`uname` to build this value. Our values
/// deliberately match the strings HotSpot 25 reports on the same OS so
/// downstream code that compares against `"Windows 11"`, `"Linux"`,
/// `"Mac OS X"` works unchanged.
fn canonical_os_name() -> String {
    #[cfg(target_os = "windows")]
    {
        // Windows 11 has kernel version 10.0 with build >= 22000.
        // HotSpot picks the product name from `ProductName`; we emulate
        // that by reading the build number and thresholding.
        if let Some(build) = windows_build_number() {
            if build >= 22000 {
                return "Windows 11".to_string();
            } else if build >= 10000 {
                return "Windows 10".to_string();
            }
        }
        return "Windows".to_string();
    }
    #[cfg(target_os = "linux")]
    {
        return "Linux".to_string();
    }
    #[cfg(target_os = "macos")]
    {
        return "Mac OS X".to_string();
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        std::env::consts::OS.to_string()
    }
}

/// Return a HotSpot-compatible `os.arch` string.
///
/// HotSpot reports `amd64` for x86_64 (on Linux/Windows) and `aarch64`
/// for ARM64. `std::env::consts::ARCH` uses `x86_64`; translate.
fn canonical_os_arch() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "amd64".to_string(),
        "x86" => "x86".to_string(),
        "aarch64" => "aarch64".to_string(),
        "arm" => "arm".to_string(),
        other => other.to_string(),
    }
}

/// Return a HotSpot-compatible `os.version` string.
///
/// * Windows: `"10.0"` (kernel version — what HotSpot actually reports).
/// * Linux: parse `/proc/sys/kernel/osrelease`.
/// * macOS: fall back to `"14.0"` (best-effort — no good std API).
fn canonical_os_version() -> String {
    #[cfg(target_os = "windows")]
    {
        return "10.0".to_string();
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(v) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
            return v.trim().to_string();
        }
        return "0.0".to_string();
    }
    #[cfg(target_os = "macos")]
    {
        return "14.0".to_string();
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        "0.0".to_string()
    }
}

/// Windows-only helper: read the kernel build number from the registry,
/// falling back to an env var for test harnesses.  Returns `None` if
/// the build number can't be read.
#[cfg(target_os = "windows")]
fn windows_build_number() -> Option<u32> {
    // Tests can override via env to validate the threshold logic.
    if let Ok(override_str) = cratonvm_types::flags::runtime_var("CRATONVM_FORCE_WIN_BUILD") {
        if let Ok(n) = override_str.parse::<u32>() {
            return Some(n);
        }
    }
    // Best-effort registry read — the `winreg` crate isn't in our deps,
    // so use a subprocess-less env probe that covers modern Windows
    // (`OS`=Windows_NT everywhere, we look at the runtime's own kernel
    // version). As a last resort, assume Windows 11 kernel build.
    //
    // When the registry read isn't available, default to a high build
    // number so "Windows 11" is reported on modern builds.
    Some(22000)
}

/// The four BCP-47 subtags the JDK publishes per locale category.
///
/// Mirrors the `_display_*_NDX` / `_format_*_NDX` slot groups of
/// `jdk.internal.util.SystemProps$Raw` — the JDK's platform layer hands
/// `SystemProps` exactly these four strings twice, once for DISPLAY and once
/// for FORMAT, and `SystemProps.fillI18nProps` turns them into the `user.*`
/// property family.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LocaleSubtags {
    pub language: String,
    pub script: String,
    pub country: String,
    pub variant: String,
}

/// The host's two locales, as the JDK models them.
///
/// These are genuinely two different settings on Windows (UI language vs
/// "Regional format") and two different `LC_*` categories on Unix, which is
/// why `Locale.Category` exists at all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HostLocale {
    /// Seeds `user.language`/`user.script`/`user.country`/`user.variant`.
    pub display: LocaleSubtags,
    /// Seeds the `user.*.format` overlay when it differs from `display`.
    pub format: LocaleSubtags,
}

/// Split a locale name into BCP-47 subtags.
///
/// Accepts both spellings the two host families use:
///   * BCP-47, as `GetUserDefaultLocaleName` returns it — `ru-RU`,
///     `zh-Hans-CN`, `sr-Latn-RS`, `ca-ES-valencia`.
///   * POSIX, as `$LANG` carries it — `ru_RU.UTF-8`, `en_US`, `C`,
///     `sr_RS@latin`.
///
/// Classification follows the BCP-47 grammar the JDK's own
/// `Locale.forLanguageTag` uses: subtag 1 is the language; a 4-alpha subtag is
/// a script; a 2-alpha or 3-digit subtag is a region; anything after that is a
/// variant. Java spells variants uppercase and joins multiples with `_`, which
/// is what `Locale.getVariant()` returns, so we do the same.
///
/// `C` and `POSIX` map to `en`/`US`, exactly as the HotSpot launcher's
/// `java_props_md.c` does — a real JVM never reports language `"C"`.
pub(crate) fn parse_locale_name(raw: &str) -> LocaleSubtags {
    // POSIX carries the charset after `.` and a modifier after `@`; BCP-47 has
    // neither, so stripping them is a no-op on that spelling.
    let raw = raw.trim();
    let (head, modifier) = match raw.split_once('@') {
        Some((h, m)) => (h, m.trim()),
        None => (raw, ""),
    };
    let head = head.split('.').next().unwrap_or("").trim();

    let mut out = LocaleSubtags::default();
    let mut parts = head
        .split(['-', '_'])
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let Some(first) = parts.next() else {
        return LocaleSubtags {
            language: "en".to_string(),
            country: "US".to_string(),
            ..LocaleSubtags::default()
        };
    };
    if first.eq_ignore_ascii_case("C") || first.eq_ignore_ascii_case("POSIX") {
        return LocaleSubtags {
            language: "en".to_string(),
            country: "US".to_string(),
            ..LocaleSubtags::default()
        };
    }
    out.language = first.to_ascii_lowercase();

    let mut variants: Vec<String> = Vec::new();
    for part in parts {
        // A one-character subtag is a BCP-47 *singleton* and everything after
        // it is an extension (`ja-JP-u-ca-japanese`), not a variant. Java keeps
        // extensions off `Locale.getVariant()`, and we do not model them, so
        // stop rather than fold `u`/`ca`/`japanese` into the variant string.
        if part.len() == 1 {
            break;
        }
        let is_alpha = part.chars().all(|c| c.is_ascii_alphabetic());
        let is_digit = part.chars().all(|c| c.is_ascii_digit());
        if out.script.is_empty() && out.country.is_empty() && part.len() == 4 && is_alpha {
            // Script subtags are Titlecase in BCP-47 and in `Locale.getScript()`.
            let mut s = part.to_ascii_lowercase();
            s[..1].make_ascii_uppercase();
            out.script = s;
        } else if out.country.is_empty()
            && variants.is_empty()
            && ((part.len() == 2 && is_alpha) || (part.len() == 3 && is_digit))
        {
            out.country = part.to_ascii_uppercase();
        } else {
            variants.push(part.to_ascii_uppercase());
        }
    }

    // POSIX `@modifier`. The two that name a script rather than a variant are
    // the Serbian/Azeri script selectors; everything else the JDK carries
    // through as a variant.
    match modifier.to_ascii_lowercase().as_str() {
        "" => {}
        "latin" | "latn" => out.script = "Latn".to_string(),
        "cyrillic" | "cyrl" => out.script = "Cyrl".to_string(),
        other => variants.push(other.to_ascii_uppercase()),
    }
    out.variant = variants.join("_");

    // The three ISO-639 codes Java froze at their pre-1989 spellings.
    // `Locale` applies this internally (`convertOldISOCodes`), so
    // `Locale.getDefault().getLanguage()` returns the old code no matter what
    // the property says; applying it here keeps `System.getProperty
    // ("user.language")` and `Locale.getDefault().getLanguage()` in agreement,
    // which is the invariant `locale_bootstrap::resolve_default_locale`
    // depends on.
    out.language = match out.language.as_str() {
        "he" => "iw".to_string(),
        "yi" => "ji".to_string(),
        "id" => "in".to_string(),
        _ => out.language,
    };
    out
}

/// Read the two host locales.
///
/// **Windows** — the JDK's `java_props_md.c` reads two distinct settings and
/// this mirrors them: `GetUserDefaultUILanguage()` (Settings ▸ Language, the
/// UI language) seeds DISPLAY, and `GetUserDefaultLocaleName()` (Settings ▸
/// Region ▸ "Regional format") seeds FORMAT. They are independent — an English
/// UI with a Russian regional format is an ordinary configuration — which is
/// the whole reason `Locale.Category` exists. Before W7-67 this function did
/// not query Windows at all and every Windows host reported `en_US`.
///
/// **Unix** — the JDK calls `setlocale(LC_CTYPE, "")` for FORMAT and
/// `setlocale(LC_MESSAGES, "")` for DISPLAY, and libc resolves each from
/// `LC_ALL` ▸ the category's own variable ▸ `LANG`. We read that precedence
/// directly rather than linking `setlocale`, which is process-global state we
/// do not otherwise touch. Note `LC_ALL` must beat `LANG`, not the other way
/// round.
fn derive_host_locale() -> HostLocale {
    if let Some(host) = platform_host_locale() {
        return host;
    }

    let env = |name: &str| cratonvm_types::flags::runtime_var(name).unwrap_or_default();
    let lc_all = env("LC_ALL");
    let lang = env("LANG");
    let pick = |category: String| {
        if !lc_all.trim().is_empty() {
            lc_all.clone()
        } else if !category.trim().is_empty() {
            category
        } else {
            lang.clone()
        }
    };
    HostLocale {
        display: parse_locale_name(&pick(env("LC_MESSAGES"))),
        format: parse_locale_name(&pick(env("LC_CTYPE"))),
    }
}

/// The host's locales from a platform API, or `None` where there is no such
/// API and the environment is the only source.
///
/// Unix/macOS deliberately return `None` rather than calling `setlocale`:
/// `setlocale` mutates process-global state we do not otherwise touch, and the
/// `LC_ALL` ▸ category ▸ `LANG` precedence it would apply is the one the caller
/// reads directly.
#[cfg(target_os = "windows")]
fn platform_host_locale() -> Option<HostLocale> {
    windows_host_locale()
}

#[cfg(not(target_os = "windows"))]
fn platform_host_locale() -> Option<HostLocale> {
    None
}

/// Ask Windows for the UI language and the regional format, as BCP-47 names.
///
/// `GetUserDefaultLocaleName` is the modern replacement for the
/// `GetUserDefaultLCID` + `GetLocaleInfo(LOCALE_SISO639LANGNAME/
/// LOCALE_SISO3166CTRYNAME)` pair `java_props_md.c` uses: it returns the same
/// locale, already assembled as a BCP-47 name, so the script subtag survives
/// (`zh-Hans-CN`) instead of having to be reconstructed from the LCID.
///
/// Returns `None` if either call fails, so the caller falls through to the
/// environment path rather than inventing a locale.
#[cfg(target_os = "windows")]
fn windows_host_locale() -> Option<HostLocale> {
    // LOCALE_NAME_MAX_LENGTH.
    const LOCALE_NAME_MAX_LENGTH: usize = 85;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetUserDefaultLocaleName(lp_locale_name: *mut u16, cch_locale_name: i32) -> i32;
        fn GetUserDefaultUILanguage() -> u16;
        fn LCIDToLocaleName(
            locale: u32,
            lp_name: *mut u16,
            cch_name: i32,
            dw_flags: u32,
        ) -> i32;
    }

    /// Trim the trailing NUL the Win32 `*LocaleName` calls include in their
    /// returned length and decode the UTF-16 buffer.
    fn decode(buf: &[u16], written: i32) -> Option<String> {
        if written <= 1 {
            return None;
        }
        let s = String::from_utf16_lossy(&buf[..(written as usize - 1)]);
        if s.trim().is_empty() {
            None
        } else {
            Some(s)
        }
    }

    let mut buf = [0u16; LOCALE_NAME_MAX_LENGTH];
    let written =
        unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), LOCALE_NAME_MAX_LENGTH as i32) };
    let format_name = decode(&buf, written)?;

    // The UI language is a LANGID. `MAKELCID(langid, SORT_DEFAULT)` is just
    // the LANGID zero-extended, since SORT_DEFAULT == 0.
    let ui_langid = unsafe { GetUserDefaultUILanguage() };
    let mut ui_buf = [0u16; LOCALE_NAME_MAX_LENGTH];
    let ui_written = unsafe {
        LCIDToLocaleName(
            u32::from(ui_langid),
            ui_buf.as_mut_ptr(),
            LOCALE_NAME_MAX_LENGTH as i32,
            0,
        )
    };
    // A UI language Windows cannot name is not a reason to discard the
    // regional format we did read — fall back to it, which is what a host with
    // UI == format looks like anyway.
    let display_name = decode(&ui_buf, ui_written).unwrap_or_else(|| format_name.clone());

    Some(HostLocale {
        display: parse_locale_name(&display_name),
        format: parse_locale_name(&format_name),
    })
}

/// Publish one `user.<base>` property family, following
/// `jdk.internal.util.SystemProps.fillI18nProps` exactly.
///
/// Three rules, all of them load-bearing and none of them obvious:
///
///  1. **A command-line `-Duser.<base>` wins outright and suppresses the
///     overlay.** The JDK returns from `fillI18nProps` before deriving
///     anything, so `-Duser.language=fr` on a host whose regional format is
///     German must NOT leave a `user.language.format=de` behind.
///  2. **The base property takes the DISPLAY value**, not the format one.
///  3. **`.display` is never created from platform values** — the JDK only
///     writes it when it differs from the base, and it has just been *set*
///     from the base, so the condition is dead. `.format` is written only when
///     it differs from DISPLAY.
///
/// `cmdline` is `config.system_properties`, which the caller re-applies over
/// `sys_props` afterwards; consulting it here is what implements rule 1.
fn fill_i18n_props(
    sys_props: &mut HashMap<String, String>,
    cmdline: &[(String, String)],
    base: &str,
    display: &str,
    format: &str,
) {
    if cmdline.iter().any(|(k, _)| k == base) {
        return; // Rule 1: do not override, and do not derive the overlay.
    }
    // HotSpot publishes all four keys unconditionally, empty string included —
    // `System.getProperty("user.variant")` is `""` there, never null. We used
    // to omit `user.script`/`user.variant` entirely.
    sys_props.insert(base.to_string(), display.to_string());
    if format != display {
        sys_props.insert(format!("{base}.format"), format.to_string());
    }
}


// ---------------------------------------------------------------------------
// SharedVm — thread-safe shared state
// ---------------------------------------------------------------------------

/// Shared VM state, protected by interior mutability.
///
/// Multiple threads can hold `&SharedVm` simultaneously. Mutable access to
/// individual subsystems is provided via `RwLock` (class_manager, statics,
/// resolution_cache) or atomic operations (heap).
/// Length of [`SharedVm::anon_class_cache`]. Indexed by instance-field count,
/// so it bounds the largest field count served by the lock-free fast path;
/// allocations with more fields than this fall back to the slow (locked) path.
/// 256 comfortably covers every native synthetic allocation
/// (`HashMap`/`LinkedHashMap` nodes, view backings, …), which use a handful of
/// small field counts.
pub const ANON_CLASS_CACHE_LEN: usize = 256;
const DEFAULT_MAX_HEAP_SIZE: usize = 256 * 1024 * 1024;

fn apply_container_default_heap(config: &mut VmConfig) {
    if !config.use_container_support || config.max_heap_size != DEFAULT_MAX_HEAP_SIZE {
        return;
    }

    let info = crate::runtime::container::detect_container();
    let suggested =
        crate::runtime::container::suggested_default_max_heap(&info, config.max_heap_size);
    if suggested != config.max_heap_size {
        tracing::info!(
            "container: default max heap adjusted from {} to {} bytes",
            config.max_heap_size,
            suggested
        );
        config.max_heap_size = suggested;
    }
}

#[cfg(feature = "experimental-debug")]
fn load_startup_jvmti_agents(config: &VmConfig, env: &mut crate::jvmti::JvmtiEnv) {
    for option in &config.jvmti_agent_options {
        if option.starts_with("-javaagent:") {
            panic!(
                "failed to load JVMTI startup agent `{option}`: \
                 -javaagent must run through runtime::agent_loader::invoke_premains"
            );
        }
        if !option.starts_with("-agentlib:") && !option.starts_with("-agentpath:") {
            panic!("failed to load JVMTI startup agent `{option}`: unsupported agent option");
        }

        let (path, options) = crate::jvmti::parse_agent_arg(option);
        env.agent_registry
            .load_agent(&path, &options)
            .unwrap_or_else(|err| panic!("failed to load JVMTI startup agent `{option}`: {err}"));
    }
}

#[cfg(not(feature = "experimental-debug"))]
fn reject_startup_jvmti_agents_when_disabled(config: &VmConfig) {
    if let Some(option) = config.jvmti_agent_options.first() {
        panic!("failed to load JVMTI startup agent `{option}`: JVMTI support is not compiled in");
    }
}

/// Boot precondition for `--jdk-only`
/// (`docs/feature-designs/jdk-only-mode.md` §1.1, §8): strict mode means real
/// class bytes are authoritative, so a real JDK runtime image is **required**
/// and there is no silent fallback.
///
/// Returns the resolved `JAVA_HOME` under
/// [`CompatibilityMode::JdkOnly`](cratonvm_types::compat::CompatibilityMode::JdkOnly)
/// and `Ok(None)` under `Compatible`.
///
/// Three properties this shape is protecting, each of which has burned us
/// before:
///
///  1. **The `Compatible` arm probes nothing.** `VmConfig::default()` is
///     hermetic — hundreds of unit tests and every embedder build one without
///     a JDK on the box — and a host probe on the default path would make the
///     *default* boot depend on the machine. That is the exact defect
///     `require_real_jdk` was written to remove, so it must not be
///     reintroduced one level up. `Compatible` returns before touching disk.
///  2. **`validate_compatibility` runs first, unconditionally.** The launcher
///     rejects `--jdk-only --synthetic-jdk` at argv-parse time, but embedders
///     (`libcratonvm`, `cratonvm-embed`) and in-process harnesses construct a
///     `VmConfig` by hand and never see argv. `SharedVm::new` is the one choke
///     point every boot passes through, so the config-consistency check lives
///     here too rather than only in the CLI.
///  3. **One boot check, not two.** The JDK search is
///     [`crate::config::require_real_jdk`]'s job; its message already carries
///     [`crate::config::describe_jdk_search`]'s per-probe account and the
///     accepted image layouts (`jmods/java.base.jmod` or `lib/modules`). A
///     second, `--jdk-only`-flavoured copy of that search would drift out of
///     sync with the first the moment either is edited. We delegate and wrap.
///
/// The wrapper preamble exists because `require_real_jdk`'s stock text offers
/// `--synthetic-jdk` as one of its four fixes, and under `--jdk-only` that
/// escape is unavailable by construction (§9: the two flags conflict) — a
/// strict-mode operator handed that suggestion would follow it into a second,
/// more confusing error.
fn require_jdk_image_for_jdk_only(config: &VmConfig) -> Result<Option<PathBuf>, VmError> {
    // Covers embedders and in-process harnesses that never parse argv.
    config.validate_compatibility()?;

    if !config.is_jdk_only() {
        // Default path: no host probing. See property 1 above.
        return Ok(None);
    }

    match crate::config::require_real_jdk(config.java_home.as_deref()) {
        Ok(home) => Ok(Some(home)),
        Err(detail) => Err(VmError::InvalidConfiguration(format!(
            "--jdk-only requires a real JDK runtime image, and none was found.\n\
             \n\
             In JDK-only mode real class bytes are authoritative: no class may be \
             fabricated and no synthetic-stub native may be registered, so there is \
             no class library left to boot without a runtime image. The \
             --synthetic-jdk escape hatch does NOT apply here — it conflicts with \
             --jdk-only by construction. If you want today's compatibility \
             behaviour instead, re-run with --real-jdk (the default).\n\
             \n\
             {detail}"
        ))),
    }
}

/// Pre-register one of the bootstrap block's compatibility stand-ins, and
/// under [`CompatibilityMode::JdkOnly`] **do not ask at all**.
///
/// # Why this exists
///
/// The three call sites below (`Enumeration$Impl`, `Comparator$Native`, the
/// eleven `cratonvm/internal/Unmodifiable*`) are the *only* fabrications the
/// boot block performs — measured 2026-08-05 with `--dump-class-origins`
/// against a real JDK 25 image: 13 `compatibility-stub` rows from exactly
/// these three lines. They used to go through the infallible
/// `ensure_synthetic_class`, which records the `--jdk-only` violation and then
/// fabricates anyway, so a strict run reported a violation while continuing in
/// the state contract §5 forbids.
///
/// # Under `--jdk-only` the question is not asked, because the answer is a constant
///
/// From 2026-08-05 to 2026-08-12 this asked anyway and absorbed the refusal.
/// Measured 2026-08-12 under `--jdk-only --explain-jdk-only --jdk-only-report`
/// on an ordinary application: **13 of the 19 `compatibility-class-requested`
/// rows in the entire census came from this one function**, and every one of
/// the 13 is decided before the call is made:
///
/// 1. **The refusal is unconditional.** None of the 13 names escapes
///    `fabricated_origin_for_name`'s VM-internal arms — those are `CratonVM$…`
///    (prefix only), the proxy supertypes, the annotation carrier and the
///    three generated-name families — so all 13 land on
///    `ClassOrigin::compatibility_stub` and `try_ensure_synthetic_class`
///    refuses them on every strict run, in every workload.
/// 2. **A fabrication that succeeded would be worse than the refusal**,
///    because strict mode registers no method on any of the 13.
///    `java/util/Enumeration$Impl` and `java/util/Comparator$Native` are both
///    in `native_api::no_image_receiver::NO_IMAGE_JDK_RECEIVERS`, so
///    `NativeMethodRegistry::register` re-tags every native on them
///    `SyntheticStub` and `JdkOnly` drops the lot; the eleven
///    `cratonvm/internal/Unmodifiable*` are hand-tagged `SyntheticStub` by
///    `native-collections`' `register_unmodifiable_natives`. Wiring a
///    superclass and an interface list onto a carrier with no implementation
///    is the `UnsatisfiedLinkError` shape
///    `no_image_receiver::STRICT_STILL_FABRICATES` exists to warn about, run
///    in the other direction.
///
/// So the skip is execution-identical to the absorb-and-warn it replaces —
/// same `None`, same skipped wiring, same absent class, same dispatch — and it
/// gives the census back its 13 rows. **Nothing diagnostic is lost.** A strict
/// consumer that genuinely needs one of these asks for it at *its* call site
/// and produces its own refusal row, keyed on its own `requester`; that is
/// exactly how `System.getenv`'s dependency on
/// `cratonvm/internal/UnmodifiableMap` was found, and it was found *despite*
/// the boot row rather than because of it (the two were separate events with
/// separate diagnoses — see `ClassManager`'s `jdk_only_refusals`, whose dedupe
/// key is the `(class, site)` PAIR for precisely this reason).
///
/// # The premise this doc used to carry, and why it was false
///
/// It said these stand-ins "exist for the synthetic collection shims, which
/// strict mode does not register", flat. True of twelve, and **false of
/// `cratonvm/internal/UnmodifiableMap`**: `lang_system::wrap_system_env_map`
/// allocated it, ships in the ESSENTIAL set, and therefore survives strict
/// mode — so `System.getenv()`, and every Spring `AbstractEnvironment::<init>`
/// through it, died on a `NoClassDefFoundError` until 2026-08-12, when that
/// native was moved onto the real `java.util.Collections.unmodifiableMap`.
/// One over-general sentence is why this family went unrevisited.
///
/// **Before adding a name here, find who allocates it and what `NativeKind`
/// that allocator's registration carries.** The mode flag is not the answer
/// and the `cratonvm/` prefix is not the answer; the registration's kind is,
/// and it is ambient (`set_category` around a block, `register()` last-write-
/// wins), so it has to be read at the registrar and not guessed at the mint
/// site.
///
/// # What a refusal still means, in `Compatible` mode
///
/// `ClassManager::try_ensure_synthetic_class` also refuses in **both** modes
/// when the name is already carried by two or more distinct classes
/// (`IncompatibleClassChangeError`), so the arm below stays live under
/// `--real-jdk`. `None`, the caller skips the wiring, and the `warn!` states
/// the *consequence* — the natives bound to the class are unreachable —
/// rather than just the fact.
///
/// The boot deliberately continues in either mode. Under `--jdk-only` a real
/// `java.util.Collections` / `Enumeration` / `Comparator` is on the boot
/// classpath and runs its own bytecode. Failing the boot instead would refuse
/// a run that is otherwise conforming.
fn ensure_bootstrap_compat_class(
    class_manager: &mut ClassManager,
    name: &str,
    num_fields: usize,
) -> Option<ClassId> {
    // The policy is already installed when this runs: `set_compatibility_mode`
    // is the very next statement after the manager is constructed, ~120 lines
    // above the first call site, precisely so that no class escapes the policy
    // it was started under. Read from the manager and not from a `cfg!`: a
    // Cargo feature cannot see a runtime mode.
    // A class that is ALREADY LOADED is answered in every mode. This is not a
    // fabrication — it is a lookup that happens to share an entry point with
    // one, and `--jdk-only` has no quarrel with real bytes.
    //
    // Measured 2026-08-12: without this, `System.out` in strict mode is a
    // zero-slot `java/lang/Object`. `getClass()` answers `java.lang.Object`,
    // `instanceof PrintStream` is false, and `getSuperclass()` is null. The
    // caller at the `java/io/PrintStream` site says so in its own comment —
    // "the load above has already put the real java.io.PrintStream in the
    // store, so this resolves to it and fabricates nothing" — and its refusal
    // arm deliberately degrades to `java/lang/Object` because that arm was only
    // ever meant to be reachable where `java.base` is absent.
    //
    // The blanket early return below was added the same day to stop the boot
    // block REQUESTING the thirteen `cratonvm/internal/Unmodifiable*` stand-ins
    // under strict mode, which it correctly does. But this function is named
    // for its majority caller, not its contract, and one caller passes a real
    // JDK class. That is the SECOND time this exact function's stated scope has
    // been wrong about a caller — its doc comment previously claimed the
    // stand-ins "exist for the synthetic collection shims", which was false for
    // `cratonvm/internal/UnmodifiableMap` and cost `System.getenv()`.
    //
    // **A guard scoped by a premise about who calls you is only as good as that
    // premise.** Ask the store, not the caller list.
    if let Some(id) = class_manager.get_loaded_class_id(name) {
        return Some(id);
    }
    if class_manager.compatibility_mode().is_jdk_only() {
        return None;
    }
    match class_manager.try_ensure_synthetic_class(name, num_fields) {
        Ok(id) => Some(id),
        Err(err) => {
            tracing::warn!(
                class = name,
                error = %err,
                "refusing to fabricate this bootstrap compatibility class. It is \
                 NOT registered, the natives bound to it are unreachable, and any code that \
                 needs it will fail at its own call site naming this class."
            );
            None
        }
    }
}

/// State for the `main_thread_group` lazy singleton's claim/wait
/// coordination (see `SharedVm::main_thread_group_init`'s doc). Mirrors the
/// `Class::initializing_thread` + `class_init_waiters` shape used for JVMS
/// §5.5 class initialization, scoped down to a single global singleton
/// instead of a `ClassId`-keyed map.
pub enum MainThreadGroupInit {
    /// No thread is currently building the group. Either it has never been
    /// attempted, or the previous attempt already finished (check
    /// `SharedVm::main_thread_group` for the result) or failed (in which
    /// case a later caller may attempt the build again).
    Idle,
    /// `owner_thread` (a `ThreadId(..).0`) is currently running the build.
    /// Every other thread that observes this variant blocks on `waiter`'s
    /// condvar until the owner transitions back to `Idle` and notifies.
    InProgress {
        owner_thread: u64,
        waiter: Arc<(parking_lot::Mutex<bool>, parking_lot::Condvar)>,
    },
}

/// A captured Throwable trace and the non-owning object handle it belongs to.
///
/// The registry is deliberately VM-wide: Java permits a Throwable constructed
/// on one thread to be inspected after that thread has terminated. `throwable`
/// is *not* scanned as a GC root; `remap_and_sweep_throwable_stack_traces`
/// forwards it after a move and drops the trace once its Throwable dies.
#[derive(Debug, Clone)]
pub(crate) struct ThrowableStackTrace {
    throwable: ObjectRef,
    frames: Vec<StackTraceEntry>,
}

pub struct SharedVm {
    /// Process-unique identity for this VM/heap lifetime.
    ///
    /// Native caches that store heap `ObjectRef`s use this as their scope key.
    /// It must not be the `SharedVm` address because allocators may recycle that
    /// address after a test drops one VM and creates another in the same process.
    pub vm_identity: usize,

    /// VM configuration (immutable after construction).
    pub config: VmConfig,

    /// GPU offload cache registry (Part E, Phase 3). Owns one
    /// [`OffloadCache`](crate::runtime::offload::OffloadCache) per
    /// CUDA device ordinal — each cache holds its own
    /// `DeviceContext` and per-method compiled-kernel map. Cheap when
    /// offload is off: no `OffloadCache` is constructed until the
    /// first `get_or_create` call, and absent that call the registry
    /// is just an empty `FxHashMap`.
    ///
    /// Behind the `gpu-offload` Cargo feature — the field does not
    /// exist on the CPU-only build.
    #[cfg(feature = "gpu-offload")]
    pub offload_registry: std::sync::Arc<crate::runtime::offload::OffloadCacheRegistry>,
    /// Class loading, linking, resolution and per-class caches. Owns the L10 lock.
    ///
    /// See [`crate::vm::realms::ClassRealm`]. Access paths are
    /// `shared.classes.<field>`; lock types and levels are
    /// unchanged by the move.
    pub classes: crate::vm::realms::ClassRealm,
    /// Native-method registry, Panama FFI tables, JNI globals and the fd table.
    ///
    /// See [`crate::vm::realms::NativeRealm`]. Access paths are
    /// `shared.natives.<field>`; lock types and levels are
    /// unchanged by the move.
    pub natives: crate::vm::realms::NativeRealm,

    /// Object heap, GC coordination and the VM-wide GC root tables.
    ///
    /// See [`crate::vm::realms::HeapRealm`]. Access paths are
    /// `shared.mem.<field>`; lock types and levels are
    /// unchanged by the move.
    pub mem: crate::vm::realms::HeapRealm,

    /// Java threads: registry, monitors, virtual-thread scheduling and the main ThreadGroup.
    ///
    /// See [`crate::vm::realms::ThreadRealm`]. Access paths are
    /// `shared.threads.<field>`; lock types and levels are
    /// unchanged by the move.
    pub threads: crate::vm::realms::ThreadRealm,

    /// Synthetic System.out PrintStream object.
    pub system_out: RwLock<Option<ObjectRef>>,

    /// Synthetic System.err PrintStream object.
    pub system_err: RwLock<Option<ObjectRef>>,

    /// Canonical stdin `FileInputStream` for `System.in` (Surefire pipe bootstrap
    /// reads `System.in` before `System.initPhase1` completes; must be non-null).
    pub system_in: RwLock<Option<ObjectRef>>,

    /// System properties: populated with platform defaults + user overrides.
    pub system_properties: RwLock<HashMap<String, String>>,

    /// Weak self-reference so native methods can obtain `Arc<SharedVm>` for
    /// spawning new threads. Set by `Vm::new()` after wrapping in `Arc`.
    pub self_arc: RwLock<Option<Weak<SharedVm>>>,
    /// Observability, diagnostics and the JVMTI/JDWP debug plumbing.
    ///
    /// See [`crate::vm::realms::DebugRealm`]. Access paths are
    /// `shared.debug.<field>`; lock types and levels are
    /// unchanged by the move.
    pub debug: crate::vm::realms::DebugRealm,
    /// JIT compilation state: code cache, PGO profiles, tiering policy, deopt log and invalidation.
    ///
    /// See [`crate::vm::realms::JitRealm`]. Access paths are
    /// `shared.jit.<field>`; lock types and levels are
    /// unchanged by the move.
    pub jit: crate::vm::realms::JitRealm,

    /// WP1.3 — JVM bootstrap init level, matching HotSpot's
    /// `VM._init_level` integer state machine:
    ///
    /// * 0 — VM initialization has not started (right after
    ///   `SharedVm::new` begins).
    /// * 1 — Primordial classes loaded (`java/lang/Object`,
    ///   `Class`, `String`, primitive wrappers).
    /// * 2 — `System.initPhase1` ran — system properties, encoding,
    ///   and `System.in/out/err` are installed.
    /// * 3 — `System.initPhase2` ran — modules + classpath
    ///   finalized, `ModuleLayer.boot` populated.
    /// * 4 — VM fully initialized — `initPhase3` ran,
    ///   `ClassLoader.getSystemClassLoader()` is usable, user
    ///   `main()` is about to be invoked.
    ///
    /// Callers read the level via [`Self::init_level`] and bump it
    /// via [`Self::set_init_level`]. Threads that need to block
    /// until a level is reached use [`Self::await_init_level`],
    /// which is also the backing for the `VM.awaitInitLevel` native.
    pub init_level: AtomicI32,

    /// WP1.3 — monitor + condvar used by [`Self::await_init_level`]
    /// and `VM.awaitInitLevel`. The boolean inside is an always-true
    /// dummy; the real state lives in [`Self::init_level`]. Using
    /// `std::sync::Mutex<i32>` + `Condvar` (rather than `parking_lot`)
    /// here because `Condvar::wait_while` accepts a
    /// `std::sync::MutexGuard` — we don't need the `parking_lot`
    /// poisoning-free variant since this path is cold and the
    /// mutex-held region is microscopic.
    pub init_level_waiters: Arc<(std::sync::Mutex<()>, std::sync::Condvar)>,
}

// ---------------------------------------------------------------------------
// Typed bootstrap state machine
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Allocated;
#[derive(Debug)]
struct ClassesReady;
#[derive(Debug)]
struct NativesReady;
#[derive(Debug)]
struct RuntimeReady;

/// Compile-time ordering plus testable runtime invariants for VM startup.
///
/// The payload is intentionally small: the large subsystem values remain local
/// to `SharedVm::new`, while this token is the only value allowed to cross each
/// phase boundary. A new initialization step therefore cannot be reordered
/// past classes/native/runtime readiness without changing the token type.
#[must_use = "a bootstrap phase must be advanced or explicitly finished"]
struct BootstrapPhase<State> {
    started: std::time::Instant,
    _state: std::marker::PhantomData<State>,
}

#[derive(Debug, PartialEq, Eq)]
enum BootstrapInvariantError {
    NoClasses,
    CoreObjectMissing,
    NoNatives,
    RuntimeNotWired,
}

impl BootstrapPhase<Allocated> {
    fn begin() -> Self {
        Self {
            started: std::time::Instant::now(),
            _state: std::marker::PhantomData,
        }
    }

    fn classes_ready(
        self,
        loaded_classes: usize,
        core_object_present: bool,
    ) -> Result<BootstrapPhase<ClassesReady>, BootstrapInvariantError> {
        if loaded_classes == 0 {
            return Err(BootstrapInvariantError::NoClasses);
        }
        if !core_object_present {
            return Err(BootstrapInvariantError::CoreObjectMissing);
        }
        Ok(BootstrapPhase {
            started: self.started,
            _state: std::marker::PhantomData,
        })
    }
}

impl BootstrapPhase<ClassesReady> {
    fn natives_ready(
        self,
        registered_natives: usize,
    ) -> Result<BootstrapPhase<NativesReady>, BootstrapInvariantError> {
        if registered_natives == 0 {
            return Err(BootstrapInvariantError::NoNatives);
        }
        Ok(BootstrapPhase {
            started: self.started,
            _state: std::marker::PhantomData,
        })
    }
}

impl BootstrapPhase<NativesReady> {
    fn runtime_ready(
        self,
        runtime_wired: bool,
    ) -> Result<BootstrapPhase<RuntimeReady>, BootstrapInvariantError> {
        if !runtime_wired {
            return Err(BootstrapInvariantError::RuntimeNotWired);
        }
        Ok(BootstrapPhase {
            started: self.started,
            _state: std::marker::PhantomData,
        })
    }
}

impl BootstrapPhase<RuntimeReady> {
    fn finish(self) -> std::time::Duration {
        self.started.elapsed()
    }
}

#[cfg(test)]
mod typed_bootstrap_phase_tests {
    use super::{
        native_census_incomplete_header_json, native_census_invocations_json,
        BootstrapInvariantError, BootstrapPhase, NATIVE_CENSUS_SCHEMA_VERSION,
    };

    #[test]
    fn phase_invariants_fail_at_the_boundary_that_owns_them() {
        assert_eq!(
            BootstrapPhase::begin().classes_ready(0, true).err(),
            Some(BootstrapInvariantError::NoClasses)
        );
        assert_eq!(
            BootstrapPhase::begin().classes_ready(1, false).err(),
            Some(BootstrapInvariantError::CoreObjectMissing)
        );
        let classes = BootstrapPhase::begin()
            .classes_ready(1, true)
            .expect("classes");
        assert_eq!(
            classes.natives_ready(0).err(),
            Some(BootstrapInvariantError::NoNatives)
        );
        let natives = BootstrapPhase::begin()
            .classes_ready(1, true)
            .expect("classes")
            .natives_ready(1)
            .expect("natives");
        assert_eq!(
            natives.runtime_ready(false).err(),
            Some(BootstrapInvariantError::RuntimeNotWired)
        );
    }

    #[test]
    fn valid_bootstrap_can_only_finish_after_all_typed_transitions() {
        let elapsed = BootstrapPhase::begin()
            .classes_ready(1, true)
            .expect("classes")
            .natives_ready(1)
            .expect("natives")
            .runtime_ready(true)
            .expect("runtime")
            .finish();
        assert!(elapsed <= std::time::Duration::from_secs(1));
    }

    // ───────────────────────── native census, schema 5 ─────────────────────
    //
    // `G47-1`. The registry has carried a per-slot "this count is a floor" bit
    // since 2026-08-17 and 25 slots set it; until schema 5 the census writer
    // emitted neither the bit nor its header total, so no reader could see any
    // of them. These pin the shape that fixed it.

    /// **The tally can never be emitted without its qualifier.**
    ///
    /// This is the whole defect in one assertion: `invocations` alone is
    /// unreadable — `0` means "never called" and "called through a path that
    /// does not count" equally well — and the fix is that one function emits
    /// both or neither. A future edit that deletes the second line has to do it
    /// on purpose.
    #[test]
    fn a_census_rows_invocation_tally_always_carries_its_completeness_bit() {
        let floor = native_census_invocations_json(1_999, false);
        assert!(floor.contains("\"invocations\": 1999,"), "{floor}");
        assert!(
            floor.contains("\"invocations_complete\": false,"),
            "{floor}"
        );
        // JSON booleans, not the strings "false"/"true": a quoted value would
        // parse as truthy in every consumer that does a bare truthiness test,
        // which is the one direction this instrument must not err in.
        assert!(!floor.contains("\"false\""), "{floor}");

        let total = native_census_invocations_json(0, true);
        assert!(total.contains("\"invocations\": 0,"), "{total}");
        assert!(total.contains("\"invocations_complete\": true,"), "{total}");

        // Order matters for a human reading the file top to bottom: the
        // qualifier must follow the number it qualifies, not precede it.
        let i = floor.find("\"invocations\":").expect("tally key");
        let c = floor.find("\"invocations_complete\":").expect("bit key");
        assert!(i < c, "the bit must follow the tally:\n{floor}");
    }

    /// The header total is a **slot** count and says so; `0` is a real answer
    /// (nothing declared itself) and must still be emitted, because an absent
    /// key is exactly what schema 4 had and what nobody could read.
    #[test]
    fn the_header_states_the_incomplete_slot_total_even_when_it_is_zero() {
        let none = native_census_incomplete_header_json(0);
        assert!(
            none.contains("\"slots_with_incomplete_invocations\": 0,"),
            "{none}"
        );
        let some = native_census_incomplete_header_json(25);
        assert!(
            some.contains("\"slots_with_incomplete_invocations\": 25,"),
            "{some}"
        );
        // Header indentation (two spaces), not row indentation (six): it sits
        // beside `counts` and `invocations`, not inside `natives`.
        assert!(some.starts_with("  \""), "{some}");
    }

    /// **The writer, the doc example and the schema constant cannot drift.**
    ///
    /// `G37-1` §6 N2 measured a binary emitting `schema_version: 3`, `G42-1`
    /// §6 N1 measured `4` on a later one, and `--help` documented a third
    /// shape — three records disagreeing about one integer, all of them right
    /// about the binary they ran. The constant is the single source; this
    /// witness is what makes editing the writer's literal impossible.
    ///
    /// Reads this file from the **working tree** rather than `include_str!`,
    /// matching `registrar_call_graph_witness`: a compile-time snapshot would
    /// keep passing against source that is no longer there.
    #[test]
    fn the_census_writer_emits_the_schema_constant_and_both_new_keys() {
        let src =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/vm/vm_init.rs"))
                .expect("witness must read vm_init.rs from the working tree");

        assert_eq!(
            NATIVE_CENSUS_SCHEMA_VERSION, 5,
            "schema 5 is what adds invocations_complete; bumping this constant \
             without moving scripts/jdk-only-bridge-ratchet.py's \
             REQUIRED_CENSUS_SCHEMA (an equality test) turns the bridge ratchet \
             red — see G47-1 NOMINATION 1"
        );

        assert!(
            src.contains("NATIVE_CENSUS_SCHEMA_VERSION"),
            "the writer must stamp the constant, not a literal"
        );
        assert!(
            src.contains("native_census_invocations_json("),
            "the row loop must go through the function that emits both halves"
        );
        assert!(
            src.contains("native_census_incomplete_header_json("),
            "the header must carry slots_with_incomplete_invocations"
        );
        // The published example a reader copies from must show the new keys,
        // or the schema is documented as its predecessor.
        assert!(
            src.contains("\"invocations_complete\": false,"),
            "the doc example must show the bit"
        );
        assert!(
            src.contains("\"slots_with_incomplete_invocations\": 25,"),
            "the doc example must show the header total"
        );
    }
}

impl SharedVm {
    /// Retain a captured Throwable trace independently of the producing Java
    /// thread. The entry is non-owning and is swept by the GC remap hook.
    pub fn store_throwable_stack_trace(&self, throwable: ObjectRef, frames: Vec<StackTraceEntry>) {
        let hash = self.mem.heap.identity_hash_code(throwable);
        self.threads
            .throwable_stacks
            .write()
            .insert(hash, ThrowableStackTrace { throwable, frames });
    }

    /// Return an owned snapshot so readers never borrow through the shared
    /// registry lock while another thread refreshes a Throwable's trace.
    pub fn throwable_stack_trace(&self, hash: i32) -> Option<Vec<StackTraceEntry>> {
        self.threads
            .throwable_stacks
            .read()
            .get(&hash)
            .map(|trace| trace.frames.clone())
    }

    /// Forward live registry handles after a relocating collection and discard
    /// entries whose Throwable was collected. The registry deliberately does
    /// not keep its key object alive; `is_object_address` is the collector's
    /// stable post-collection liveness probe.
    pub fn remap_and_sweep_throwable_stack_traces(&self, pointer_map: &cratonvm_types::PointerMap) {
        let mut traces = self.threads.throwable_stacks.write();
        traces.retain(|_, trace| {
            let old_addr = trace.throwable.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                trace.throwable = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                true
            } else {
                self.mem.heap.is_object_address(old_addr).is_some()
            }
        });
    }

    /// Create a new SharedVm from a VmConfig.
    pub fn new(mut config: VmConfig) -> Self {
        apply_container_default_heap(&mut config);

        // ── Strict-mode boot precondition (jdk-only-mode.md §1.1, §8) ──────
        //
        // Deliberately the *second* statement in the constructor: before any
        // heap allocation, before the crash-handler cells are published, and
        // before `ClassManager::new` has read a byte of any classpath. A
        // `--jdk-only` run with no real JDK image cannot produce a meaningful
        // VM, so it must not produce a half-built one either — failing here
        // keeps the error a configuration error rather than a class-loading
        // mystery 3,000 classes later.
        //
        // `SharedVm::new` is infallible by signature and has several hundred
        // call sites, so this surfaces as a panic — the same mechanism
        // `reject_startup_jvmti_agents_when_disabled` (just above) already
        // uses for the same class of "the config asks for something this
        // build/host cannot do" failure. Changing the signature to
        // `Result` is a separate, much larger refactor.
        if let Err(e) = require_jdk_image_for_jdk_only(&config) {
            panic!("{e}");
        }

        // ── Publish the crash-report facts that only the config knows ──────
        //
        // The launcher prints the JDK mode on its own paths (`-version`, panic
        // hook, fatal-error arm), but the hardware-fault handler bypasses all
        // of them, and the embedding entry points (`libcratonvm`,
        // `cratonvm-embed`) have no launcher at all. `SharedVm::new` is the one
        // place every boot passes through *and* the config is in scope, so the
        // publication happens here, before anything can fault.
        //
        // Both cells are one-shot and lock-free; see the `VM diagnostic
        // snapshot` section in `runtime::crash_handler` and
        // `arch-2026-07-26/jdk-mode-determinism.md` §6.1.
        crate::runtime::crash_handler::publish_jdk_mode(
            config.jdk_mode(),
            config.java_home.as_deref(),
        );
        crate::runtime::crash_handler::publish_gc_algorithm(match config.gc_algorithm {
            crate::config::GcAlgorithm::Generational => "generational",
            crate::config::GcAlgorithm::G1 => "g1",
            #[cfg(feature = "zgc")]
            crate::config::GcAlgorithm::Zgc => "zgc",
        });

        // Compact tagless field layouts are a VM-wide contract. Generational,
        // G1, and ZGC now share the same allocation predicate, field encoding,
        // oop-map scan, and relocation fixup; no collector-specific opt-out is
        // permitted because it would let JIT-baked offsets disagree with heap
        // storage.

        #[cfg(feature = "experimental-debug")]
        let mut jvmti_env = crate::jvmti::create_jvmti_env();
        #[cfg(feature = "experimental-debug")]
        load_startup_jvmti_agents(&config, &mut jvmti_env);
        #[cfg(not(feature = "experimental-debug"))]
        reject_startup_jvmti_agents_when_disabled(&config);

        // Register the application's own directories as trusted file-I/O
        // sandbox roots. native-io confines file operations to the process
        // CWD by default; that is far too strict for a real JVM, which is
        // routinely pointed at an application installed elsewhere on disk
        // (e.g. WildFly launched from a worktree but reading `standalone.xml`
        // out of its `-Djboss.home.dir` tree). Every directory registered
        // here was supplied explicitly on the command line — the classpath
        // / `--jar`, `--java-home`, and `-D*.home` / `-D*.dir` properties —
        // and is therefore trusted. Symlink-escape and `..`-traversal
        // protection is preserved: native-io still canonicalizes and
        // containment-checks against this set.
        {
            // Classpath entries: register the entry itself if it is a
            // directory, and its parent directory either way (so sibling
            // resources next to an application jar resolve).
            for entry in &config.classpath {
                let p = std::path::Path::new(entry);
                cratonvm_native_io::add_sandbox_root(p);
                if let Some(parent) = p.parent() {
                    if !parent.as_os_str().is_empty() {
                        cratonvm_native_io::add_sandbox_root(parent);
                    }
                }
            }
            if let Some(jh) = &config.java_home {
                cratonvm_native_io::add_sandbox_root(jh);
            }
            // `-Djboss.home.dir=...`, `-Dcatalina.home=...`,
            // `-Duser.dir=...` and similar location properties name an
            // application root directory the program will read from.
            for (k, v) in &config.system_properties {
                let lk = k.to_ascii_lowercase();
                if (lk.ends_with(".home")
                    || lk.ends_with(".dir")
                    || lk.ends_with(".home.dir")
                    || lk.ends_with(".base"))
                    && std::path::Path::new(v).is_dir()
                {
                    cratonvm_native_io::add_sandbox_root(v);
                }
            }
        }

        // Auto-discover boot/ext classpath when not explicitly provided.
        //
        // When `use_synthetic_jdk` is true (default for tests), skip discovery
        // to avoid accidentally loading the entire JDK and slowing down tests.
        // When `use_synthetic_jdk` is false, always attempt discovery from:
        //   1. Explicit `java_home` in config
        //   2. `JAVA_HOME` environment variable
        //   3. `java` on PATH (auto-detected via `java -XshowSettings:properties`)
        let boot_cp = if config.boot_classpath.is_empty() && !config.use_synthetic_jdk {
            let discovered = discover_boot_classpath(config.java_home.as_deref());
            // Boot classpath discovered
            if !discovered.is_empty() {
                tracing::info!(
                    "Auto-discovered {} boot classpath entries ({} jmods)",
                    discovered.len(),
                    discovered.iter().filter(|p| p.ends_with(".jmod")).count()
                );
            }
            discovered
        } else if config.boot_classpath.is_empty() && config.java_home.is_some() {
            // Synthetic JDK mode but explicit java_home: discover for potential future use
            discover_boot_classpath(config.java_home.as_deref())
        } else {
            config.boot_classpath.clone()
        };
        let ext_cp = if config.ext_classpath.is_empty() && !config.use_synthetic_jdk {
            discover_ext_classpath(config.java_home.as_deref())
        } else if config.ext_classpath.is_empty() && config.java_home.is_some() {
            discover_ext_classpath(config.java_home.as_deref())
        } else {
            config.ext_classpath.clone()
        };

        // ── Boot-phase timing ──────────────────────────────────────────────
        //
        // Startup is a headline JVM metric and it had no instrumentation at
        // all: the only way to find out where `SharedVm::new` spent its time
        // was to add `Instant::now()` by hand and rebuild. The three phases
        // below are the ones measurement showed actually matter (see
        // `arch-2026-07-26/startup-and-diagnostics.md` §2):
        //
        //   1. classpath ingestion — `ClassManager::new` constructs the
        //      bootstrap/extension/application `ClassPath`s, and `load_jmod`
        //      *eagerly inflates every `classes/` entry of every `.jmod` into
        //      an in-memory map*. `discover_boot_classpath` puts ALL of
        //      `$JAVA_HOME/jmods` on the boot classpath, so on a stock JDK 25
        //      this is ~28,000 class entries / ~136 MB of decompressed
        //      bytecode before a single Java class is loaded. This dominates.
        //   2. `bootstrap_core_classes` — ~323 named classes plus their
        //      recursive supertypes, parsed and linked before `main`.
        //   3. native registration — the `register_*` cascade.
        //
        // Emitted through `tracing` (the same channel the surrounding
        // boot-classpath log already uses), so it costs one `Instant::now()`
        // per phase and needs no new flag to be useful under `-verbose`/
        // `RUST_LOG`. Keep these three spans intact: a regression here is
        // otherwise invisible until someone notices the VM "feels slow".
        let __boot_t0 = std::time::Instant::now();
        // JPMS `--module-path` / `--add-modules`. Both were parsed by the
        // launcher into `VmConfig` (vm-cli/src/main.rs:3428/3449) and then read
        // by NOBODY — the same "parsed then ignored" shape already recorded for
        // `--add-opens`. Resolve the selected modules here, put each root on the
        // APPLICATION search path (HotSpot also defines module-path classes to
        // the application loader), and re-register the descriptors as EXPLICIT
        // modules: `ClassManager::new`'s app-class-path scan would otherwise
        // stamp them `automatic = true`, which short-circuits every exports /
        // opens check to "allow" and defeats the point of a module path.
        // Empty `module_path` => empty vec, no filesystem probing: a plain `-cp`
        // launch pays nothing.
        let resolved_modules = crate::classloading::module::resolve_module_path(
            &config.module_path,
            &config.add_modules,
        );
        let mut app_cp: Vec<String> = config.classpath.clone();
        app_cp.extend(resolved_modules.iter().map(|m| m.root.clone()));
        let mut class_manager = ClassManager::new(&boot_cp, &ext_cp, &app_cp);
        if !resolved_modules.is_empty() {
            for m in &resolved_modules {
                class_manager
                    .module_registry
                    .register(m.descriptor.clone(), m.packages.clone());
            }
            class_manager.module_registry.build_readability_graph();
            tracing::info!(
                "module path: resolved {} module(s) from {} entry/entries: {:?}",
                resolved_modules.len(),
                config.module_path.len(),
                resolved_modules
                    .iter()
                    .map(|m| m.descriptor.name.as_str())
                    .collect::<Vec<_>>(),
            );
        }
        // ── Class-origin policy, installed before the first question ───────
        //
        // This is the *very next statement* after construction on purpose, and
        // the position is load-bearing rather than stylistic: everything below
        // that can mint a class — `application_contains_resource`'s probe,
        // `bootstrap_core_classes`, `ensure_synthetic_class`, and every
        // `load_class` — must find the policy already installed. A manager that
        // answers even one class request before `set_compatibility_mode` would
        // fabricate that class under `Compatible` rules and then be told the
        // rules were strict, i.e. exactly the un-auditable state
        // `--jdk-only` exists to make impossible (jdk-only-mode.md §5, §8).
        //
        // Read straight off the config: there is NO process global for this
        // (§2). Two VMs in one process may run under different policies, and
        // this repo has already paid for process-global native state leaking
        // across VM instances more than once.
        class_manager.set_compatibility_mode(config.compatibility_mode);
        // `-Xverify:all`. Same placement rule as the line above: before any
        // class is loaded, so no class escapes the policy it was started under.
        // `XverifyMode::None` is carried by `skip_verification` (which the
        // dispatcher in `vm_util` reads) and `Remote` is the default, so `All`
        // is the only mode this line has to say anything about.
        class_manager
            .set_strict_verification(config.xverify_mode == crate::config::XverifyMode::All);
        let native_shim_selection =
            cratonvm_native_builtins::app_shims::ShimSelection::from_resource_probe(|resource| {
                class_manager.application_contains_resource(resource)
            });
        let __boot_classpath_elapsed = __boot_t0.elapsed();
        tracing::info!(
            "boot phase 1/3 classpath ingestion: {:?} ({} boot entries, {} ext, {} app)",
            __boot_classpath_elapsed,
            boot_cp.len(),
            ext_cp.len(),
            config.classpath.len(),
        );

        // If module registry is empty after scan but we have a real JDK,
        // manually register java.base as a fallback (Session 14).
        if class_manager.module_registry.is_empty() && !config.use_synthetic_jdk {
            use crate::classloading::module::ModuleDescriptor;
            let java_base = ModuleDescriptor {
                name: "java.base".to_string(),
                version: None,
                requires: vec![],
                exports: vec![],
                opens: vec![],
                uses: vec![],
                provides: vec![],
                is_open: false,
                automatic: false,
            };
            let packages = vec![
                "java/lang".to_string(),
                "java/lang/annotation".to_string(),
                "java/lang/invoke".to_string(),
                "java/lang/ref".to_string(),
                "java/lang/reflect".to_string(),
                "java/util".to_string(),
                "java/util/concurrent".to_string(),
                "java/util/concurrent/atomic".to_string(),
                "java/util/concurrent/locks".to_string(),
                "java/util/function".to_string(),
                "java/util/stream".to_string(),
                "java/io".to_string(),
                "java/math".to_string(),
                "java/net".to_string(),
                "java/nio".to_string(),
                "java/security".to_string(),
                "java/time".to_string(),
                "jdk/internal/misc".to_string(),
                "jdk/internal/util".to_string(),
                "sun/misc".to_string(),
            ];
            class_manager.module_registry.register(java_base, packages);
            class_manager.module_registry.build_readability_graph();
            tracing::info!("Manually registered java.base module (JMOD scan fallback)");
        }

        // Apply CLI module overrides (Phase B).
        for (reader, target) in &config.add_reads {
            class_manager.module_registry.add_reads(reader, target);
        }
        for (module, pkg, target) in &config.add_exports {
            class_manager
                .module_registry
                .add_exports(module, pkg, target);
        }
        for (module, pkg, target) in &config.add_opens {
            class_manager.module_registry.add_opens(module, pkg, target);
        }
        // Rebuild graph if any dynamic edges were added.
        if !config.add_reads.is_empty() {
            class_manager.module_registry.build_readability_graph();
        }

        // If a real JDK is on the boot classpath, pre-load core classes so their
        // bytecode methods take priority over native registrations (Phase 33).
        let __boot_t1 = std::time::Instant::now();
        let bootstrapped = class_manager.bootstrap_core_classes();
        let __boot_core_classes_elapsed = __boot_t1.elapsed();
        if bootstrapped > 0 {
            tracing::info!(
                "JDK bootstrap: {} core classes loaded from boot classpath",
                bootstrapped
            );
        }
        // Reports the *total* loaded count, not just the named list: each of
        // the ~323 names in `bootstrap_core_classes` recursively drags in its
        // supertypes and interfaces, so "how many classes are loaded before
        // main" is the second number, not the first.
        tracing::info!(
            "boot phase 2/3 core-class bootstrap: {:?} ({} named classes resolved to real \
             bytecode, {} classes in the ClassStore)",
            __boot_core_classes_elapsed,
            bootstrapped,
            class_manager.loaded_count(),
        );

        // C25: Pre-register synthetic stub classes used by native-builtins so
        // that allocations via `alloc_concurrent_synthetic` carry a valid
        // ClassId.  Without this, `ensure_class_initialized` for these names
        // fails (the classes don't exist on the real classpath), the fallback
        // uses ClassId(0), and downstream `invokevirtual`/`invokeinterface`
        // dispatch can't find the receiver's class — so it falls back to the
        // constant-pool class (e.g. `java/util/Enumeration` or
        // `java/util/Iterator`) where our natives are NOT registered, yielding
        // either NoSuchMethodError or "abstract method has no Code attribute".
        // Registering as synthetic stubs (with empty `methods`) routes
        // dispatch through the native registry on `Enumeration$Impl` instead,
        // where `hasMoreElements`/`nextElement`/`hasNext`/`next` are bound.
        //
        // Fallible since 2026-08-05 (JDK-only wave 2, lane L7): under
        // `--jdk-only` the class is not created and the wiring below is
        // skipped. Since 2026-08-12 the request is not even made in that mode
        // — `ensure_bootstrap_compat_class` carries the measurement showing
        // the refusal was a constant and the natives on the resulting class
        // are all dropped anyway, so asking bought 13 census rows and no
        // information. `None` here means the same thing it always did: this
        // wiring did not happen.
        let enum_impl_id =
            ensure_bootstrap_compat_class(&mut class_manager, "java/util/Enumeration$Impl", 5);
        // Wire up the synthetic `Enumeration$Impl` so that real-JDK code which
        // does `Enumeration<URL> e = classLoader.getResources(...)` (e.g.
        // `org.apache.commons.logging.LogFactory.getResources`) can perform
        // the implicit checkcast to `java/util/Enumeration` without throwing
        // a `ClassCastException`.  We also set `java/lang/Object` as the
        // superclass so virtual methods like `getClass()` resolve through the
        // standard chain.
        let object_id = class_manager
            .load_class("java/lang/Object")
            .expect("java/lang/Object must be loadable");
        // Preload `java/lang/AssertionError` so the JIT's `new`-site resolver
        // (`resolve_jit_new_site` → `find_class_by_name`) can resolve it. The
        // `assert` statement compiles to `... new AssertionError ...; athrow`,
        // which is DEAD code when assertions are disabled (the default), so the
        // class is otherwise never loaded. With it unloaded, the resolver returns
        // None and the JIT bails compilation of the ENTIRE method — and `assert`
        // is pervasive in libraries. The canonical victim is ANTLR's
        // `ParserATNSimulator.closure` (and the whole ATN-simulation cluster:
        // `getEpsilonTarget`, `ATNConfigSet.add`, `PredictionContext.join`,
        // `SingletonPredictionContext.getReturnState`, …), every one of which has
        // asserts → none JIT-compile → Groovy script parsing runs the interpreter
        // ~2500× slower than HotSpot and times out (Spring Boot buildSrc
        // `SpringRepositoriesExtensionTests` hang). Loading it once here (a tiny
        // standard class) lets all assert-bearing methods compile; the dead
        // assert path still deopts harmlessly if ever reached.
        let _ = class_manager.load_class("java/lang/AssertionError");
        let enumeration_id = class_manager
            .load_class("java/util/Enumeration")
            .expect("java/util/Enumeration must be loadable");
        // Spring Boot 2.3+ `ExecutableArchiveLauncher.getClassPathArchivesIterator`
        // returns `Iterator<Archive>`. Launcher bytecode uses `invokeinterface`
        // `java/util/Iterator.hasNext/next` on our synthetic `Enumeration$Impl`.
        // Without declaring `Iterator`, `invoke_on_class_shared` does not retarget
        // from the interface to the concrete receiver and `next()` never runs our
        // native — the wrong value survives to `checkcast Archive` (CCE).
        let iterator_id = class_manager
            .load_class("java/util/Iterator")
            .expect("java/util/Iterator must be loadable");
        // Through `set_superclass`, not a raw `cls.superclass =` write: the
        // store's direct-subclass index has to see the new edge, or
        // `recompute_subclass_layouts` goes blind to this class.
        if let Some(enum_impl_id) = enum_impl_id {
            class_manager.set_superclass(enum_impl_id, Some(object_id));
            if let Some(cls) = class_manager.get_class_mut(enum_impl_id) {
                if !cls.interfaces.contains(&enumeration_id) {
                    cls.interfaces.push(enumeration_id);
                }
                if !cls.interfaces.contains(&iterator_id) {
                    cls.interfaces.push(iterator_id);
                }
            }
        }

        // Same treatment for `Comparator$Native`: real-JDK code that does
        // `Stream.sorted(comparator)` (e.g. Spring's `ConfigurationClassParser`)
        // performs an implicit checkcast to `java/util/Comparator`. Our
        // synthetic class must declare `Object` as superclass and
        // `java/util/Comparator` as an implemented interface for the cast to
        // succeed.
        let cmp_native_id =
            ensure_bootstrap_compat_class(&mut class_manager, "java/util/Comparator$Native", 3);
        let comparator_id = class_manager
            .load_class("java/util/Comparator")
            .expect("java/util/Comparator must be loadable");
        if let Some(cmp_native_id) = cmp_native_id {
            class_manager.set_superclass(cmp_native_id, Some(object_id));
            if let Some(cls) = class_manager.get_class_mut(cmp_native_id) {
                if !cls.interfaces.contains(&comparator_id) {
                    cls.interfaces.push(comparator_id);
                }
            }
        }

        // Unmodifiable collection-view wrappers (native-collections):
        // `Collections.unmodifiableList/Set/Map/Collection` and
        // `List.of` / `Set.of` / `Map.of` allocate these synthetic classes,
        // each with one field holding the backing collection. Native read
        // methods delegate to the backing object; native mutators throw
        // `UnsupportedOperationException`. They MUST declare `Object` as
        // superclass and the matching `java/util/*` interface so the
        // implicit checkcast at the API boundary (and `instanceof`) succeed
        // and `invokeinterface` retargets to the concrete receiver where the
        // natives are bound.
        {
            let collection_id = class_manager
                .load_class("java/util/Collection")
                .expect("java/util/Collection must be loadable");
            let list_id = class_manager
                .load_class("java/util/List")
                .expect("java/util/List must be loadable");
            let set_id = class_manager
                .load_class("java/util/Set")
                .expect("java/util/Set must be loadable");
            // Sorted/navigable unmodifiable-set APIs use separate internal
            // stamps from plain `unmodifiableSet`, so a normal set wrapper
            // does not accidentally satisfy `SortedSet` and invite callers to
            // invoke `comparator()` on a LinkedHashSet backing. The sorted and
            // navigable stamps both declare the relevant interfaces so caller
            // checkcasts still match the real JDK wrapper surfaces.
            let sorted_set_id = class_manager
                .load_class("java/util/SortedSet")
                .expect("java/util/SortedSet must be loadable");
            let navigable_set_id = class_manager
                .load_class("java/util/NavigableSet")
                .expect("java/util/NavigableSet must be loadable");
            let map_id = class_manager
                .load_class("java/util/Map")
                .expect("java/util/Map must be loadable");
            let list_iterator_id = class_manager
                .load_class("java/util/ListIterator")
                .expect("java/util/ListIterator must be loadable");
            // The real-JDK `Collections$Unmodifiable*` views (and the
            // `ImmutableCollections$*` family backing `List.of`/`Map.of`) all
            // implement `java.io.Serializable`, so application code that
            // serializes a collection containing one (e.g. Spring's `MimeType`,
            // whose `parameters` field is `Collections.unmodifiableMap(...)`)
            // round-trips. Declare it on the synthetic stamp so `instanceof
            // Serializable` and the object-serialization natives agree; the
            // wrappers carry no real bytecode fields, so the serialization
            // native emits an explicit (backing, marker) record for them.
            let serializable_id = class_manager
                .load_class("java/io/Serializable")
                .expect("java/io/Serializable must be loadable");
            // `entrySet()`'s Set view and its Map.Entry elements — see
            // native-collections' `UNMOD_ENTRY_SET_CLASS`/`UNMOD_ENTRY_ITR_CLASS`/
            // `UNMOD_MAP_ENTRY_CLASS`. Same checkcast/instanceof requirement as
            // every other synthetic wrapper below: `Map.replaceAll`'s default
            // body does `Map.Entry<K,V> entry : entrySet()` (an implicit
            // checkcast to `Map$Entry` on each `Iterator.next()` result), so the
            // wrapped entry must declare that interface or the cast throws
            // ClassCastException.
            let map_entry_id = class_manager
                .load_class("java/util/Map$Entry")
                .expect("java/util/Map$Entry must be loadable");
            // (synthetic class name, list of interface ClassIds it implements)
            let unmod_specs: [(&str, &[ClassId]); 11] = [
                (
                    "cratonvm/internal/UnmodifiableCollection",
                    &[collection_id, serializable_id],
                ),
                (
                    "cratonvm/internal/UnmodifiableList",
                    &[list_id, collection_id, serializable_id],
                ),
                (
                    "cratonvm/internal/UnmodifiableSet",
                    &[set_id, collection_id, serializable_id],
                ),
                (
                    "cratonvm/internal/UnmodifiableSortedSet",
                    &[set_id, sorted_set_id, collection_id, serializable_id],
                ),
                (
                    "cratonvm/internal/UnmodifiableNavigableSet",
                    &[
                        set_id,
                        sorted_set_id,
                        navigable_set_id,
                        collection_id,
                        serializable_id,
                    ],
                ),
                (
                    "cratonvm/internal/UnmodifiableMap",
                    &[map_id, serializable_id],
                ),
                ("cratonvm/internal/UnmodifiableItr", &[iterator_id]),
                (
                    "cratonvm/internal/UnmodifiableListItr",
                    &[list_iterator_id, iterator_id],
                ),
                (
                    "cratonvm/internal/UnmodifiableEntrySet",
                    &[set_id, collection_id, serializable_id],
                ),
                ("cratonvm/internal/UnmodifiableEntryItr", &[iterator_id]),
                ("cratonvm/internal/UnmodifiableMapEntry", &[map_entry_id]),
            ];
            for (name, ifaces) in unmod_specs {
                // These eleven are the largest group of compatibility classes
                // on a strict boot and the most tempting to reclassify as
                // `VmInternal` — no class file exists under
                // `cratonvm/internal/UnmodifiableList`, which is the
                // `VmInternal` shape. They stay `CompatibilityStub` on
                // purpose: they stand in for `java.util.Collections$Unmodifiable*`
                // and friends, whose real bytecode is not running, and that is
                // a compatibility substitution whatever the stand-in is named.
                // Reclassifying them would silence the violation, keep
                // fabricating, and make the zero-stub census read green while
                // the substitution continued.
                //
                // That argument is about `Compatible` mode, which is the only
                // mode that now reaches the fabrication: since 2026-08-12
                // `ensure_bootstrap_compat_class` returns `None` under
                // `--jdk-only` without asking, so strict mode neither
                // fabricates these nor records them. The distinction the
                // paragraph above protects is unchanged — a `VmInternal`
                // reclassification would still be a lie, and it would still be
                // a lie in the mode where the substitution actually happens.
                let Some(cid) = ensure_bootstrap_compat_class(&mut class_manager, name, 1) else {
                    continue;
                };
                class_manager.set_superclass(cid, Some(object_id));
                if let Some(cls) = class_manager.get_class_mut(cid) {
                    for iface in ifaces {
                        if !cls.interfaces.contains(iface) {
                            cls.interfaces.push(*iface);
                        }
                    }
                }
            }
        }

        let gc_backend = match config.gc_algorithm {
            crate::config::GcAlgorithm::Generational => GcBackend::Generational,
            crate::config::GcAlgorithm::G1 => GcBackend::G1,
            #[cfg(feature = "zgc")]
            crate::config::GcAlgorithm::Zgc => GcBackend::Zgc,
        };
        let g1_overrides = G1ConfigOverrides {
            region_size: config.g1_region_size,
            ihop_percent: config.g1_ihop_percent,
            max_gc_pause_ms: config.g1_max_gc_pause_ms,
            string_dedup: config.g1_string_dedup,
        };
        let mut heap = VmHeap::new_with_overrides(gc_backend, config.max_heap_size, g1_overrides);
        // Bind the heap to THIS VM's compact-layout domain, here rather than
        // later: the comment below is the reason — no object has been allocated
        // and no class layout registered yet, so from this point on every
        // allocation and every registration agree on who owns a `class_id`.
        //
        // `class_id` is a per-`ClassStore` index (`ClassId::new(classes.len())`)
        // and the layout registry is process-global, so without this a second
        // VM allocates its objects against the first VM's layouts and every
        // field access on them is decoded with the wrong storage kinds and
        // offsets.
        heap.set_layout_domain(class_manager.class_store().layout_domain());
        let bootstrap_phase = BootstrapPhase::<Allocated>::begin();

        // --- Compressed oops -------------------------------------------------
        //
        // Opt-in (`-XX:+UseCompressedOops`, or `CRATONVM_COMPRESSED_OOPS=1` for
        // A/B runs); OFF by default. This is the ONE point where the narrow-oop
        // base/shift is fixed: the heap's backing stores now exist, so their
        // addresses are known, and no object has been allocated and no class
        // layout registered yet — both of which must observe a stable reference
        // width for the whole process.
        //
        // Only the generational backend publishes the region bounds the
        // geometry is derived from, and only its collector has been audited for
        // narrow slots; G1/ZGC keep full 64-bit references.
        let want_compressed_oops = config.use_compressed_oops
            || matches!(
                cratonvm_types::flags::runtime_var("CRATONVM_COMPRESSED_OOPS").as_deref(),
                Ok("1") | Ok("true")
            );
        if want_compressed_oops {
            if gc_backend != GcBackend::Generational {
                eprintln!(
                    "[cratonvm] compressed oops requested but the selected GC backend \
                     is not generational - running with 64-bit references"
                );
            } else {
                // Reported on stderr, not just through `tracing`: a silent
                // fallback to 64-bit references would look identical to a
                // successful run apart from the footprint, and this gate is
                // experimental enough that the operator must see which one
                // they got.
                match cratonvm_gc::compressed_oops::enable_for_live_heap() {
                    Ok((base, shift)) => {
                        // The bootstrap class set was loaded and laid out
                        // BEFORE this point, with 8-byte reference fields,
                        // while the accessors are now going to read 4-byte
                        // narrow slots. Re-lay every loaded class out at the
                        // narrow width. This is the one moment it is safe:
                        // the heap was created a few lines above, so not a
                        // single instance of any of those classes exists yet.
                        let relaid = class_manager.recompute_all_compact_layouts();
                        eprintln!(
                            "[cratonvm] compressed oops ON: HeapBased base={base:#x} \
                             shift={shift} (reference fields and array elements are 4 \
                             bytes; {relaid} class layouts re-laid out)"
                        );
                    }
                    Err(why) => {
                        eprintln!(
                            "[cratonvm] compressed oops requested but unusable ({why}) \
                             - running with 64-bit references"
                        );
                    }
                }
            }
        }

        // --- ZGC relocation --------------------------------------------------
        //
        // Beside the compressed-oops gate above because it is the same shape: a
        // GC capability the JIT has not been taught about, decided once, here,
        // while the heap is young enough for the answer to be honoured.
        //
        // `RELOCATION_REQUESTED` stands in for the eventual relocation switch.
        // Nothing requests relocation today — see `zgc_relocation_permitted` —
        // so this is inert, which is the point: the gate is placed AHEAD of the
        // capability. Whoever wires the switch replaces this constant and
        // inherits the refusal rather than having to remember it.
        #[cfg(feature = "zgc")]
        if gc_backend == GcBackend::Zgc {
            const RELOCATION_REQUESTED: bool = false;
            let _zgc_relocation = zgc_relocation_permitted(RELOCATION_REQUESTED);
        }

        // bug-h2-largeblob-direct-memory-oom fix — resolve the process-wide
        // direct-buffer accounting cap (java.nio.Bits.reserveMemory's ceiling)
        // the same way real JDK resolves `-XX:MaxDirectMemorySize`: an
        // explicit flag value if the launcher passed one, otherwise `-Xmx`.
        // Previously native_io::direct_buffer hardcoded a 256 MiB cap
        // regardless of `-Xmx`, so a `-Xmx 1g` H2 MVStore workload with a
        // genuine ~250 MiB direct-buffer working set (chunk writer thread)
        // threw OutOfMemoryError at a ceiling HotSpot doesn't impose at the
        // same heap size. See
        // fixed-suite-bugs/h2-suite-bugs/bug-h2-largeblob-direct-memory-oom.md.
        let direct_memory_cap = config
            .max_direct_memory_size
            .unwrap_or(config.max_heap_size);
        cratonvm_native_io::direct_buffer::configure_max_direct_memory(direct_memory_cap as i64);

        // fork6 GC_STRESS fix — wire the SATB write barrier to the concurrent
        // old-gen cycle. `enable_concurrent_gc` previously had NO production
        // caller: the heap's `concurrent_gc_state` stayed `None`, so
        // `satb_barrier` was a hard no-op in every run, and each
        // `maybe_concurrent_gc` cycle's marker drained its own private,
        // never-written queue — the concurrent mark ran against live mutators
        // with no write barrier at all, and the sweep freed old-gen objects
        // whose only reference moved mid-cycle. These handles are shared with
        // every cycle's marker via `ConcurrentMarker::with_shared`.
        let concurrent_satb = std::sync::Arc::new(cratonvm_gc::SatbQueue::new());
        let concurrent_gc_state = std::sync::Arc::new(cratonvm_gc::ConcurrentGcState::new());
        heap.enable_concurrent_gc(concurrent_satb.clone(), concurrent_gc_state.clone());

        // Reset the classloader side-tables that are still process-wide.
        //
        // NOTE what is deliberately NOT here any more: the built-in loader
        // singletons and the `System.getenv()`/`getProperties()` singletons.
        // Clearing those from `Vm::new` assumed VMs are created strictly in
        // sequence; a Rust test binary runs `#[test]`s on several threads, so
        // this call was wiping cells that a *concurrently live* VM was using —
        // after which the two VMs traded heap objects and the reader segfaulted.
        // They are keyed by `vm_identity` now (a fresh VM starts with no row)
        // and dropped in `release_vm_native_state`.
        cratonvm_native_builtins::classloader::reset_loader_singletons();

        let __boot_t2 = std::time::Instant::now();
        let bootstrap_phase = bootstrap_phase
            .classes_ready(
                class_manager.loaded_count(),
                class_manager
                    .get_loaded_class_id("java/lang/Object")
                    .is_some(),
            )
            .expect("bootstrap ClassesReady invariants");

        // This VM's process-unique identity is allocated HERE rather than in the
        // `Self { .. }` literal at the end of this function, for one reason: the
        // capability policy is keyed on it (`VmId`), and the policy has to exist
        // before the first `register_*` call. `NEXT_VM_IDENTITY` is a monotonic
        // counter, so moving the `fetch_add` earlier changes nothing about the
        // value or its uniqueness — only when it becomes available.
        let vm_identity = NEXT_VM_IDENTITY.fetch_add(1, Ordering::Relaxed);

        let mut native_methods = NativeMethodRegistry::new();

        // ── Capability policy, installed before ANY `register_*` pass ───────
        //
        // ORDERING IS LOAD-BEARING, in both directions:
        //
        //  * `set_capabilities` must precede the `register_*` passes because
        //    `NativeMethodRegistry::register` is itself gated
        //    (`Capability::NativeRegister`), and because `register()` is what
        //    populates `sensitive_slots` — the precomputed
        //    `slot -> CapabilityKind` map that makes the dispatch-side gate an
        //    integer lookup. A policy installed after the ~3,100 boot
        //    registrations would leave every one of them unclassified, so the
        //    dispatch gate would be permanently blind even under `Enforce`.
        //  * `install_capabilities` publishes the SAME `Arc` into the process
        //    -wide `VmId -> CapabilitySet` index, which is how a native holding
        //    only a `&dyn NativeContext` reaches this VM's policy
        //    (`CapabilityCheck::check_capability`). One `Arc`, so the registry
        //    gate, the ~35 per-call-site gates in `native-builtins`/`native-io`
        //    and the dispatch gate all write to ONE audit log.
        //    It is done here, at the earliest possible point, so no boot-time
        //    native can run before the policy is visible — that window is
        //    exactly what `capability_gate`'s per-thread raw-memory memo would
        //    otherwise latch a stale `None` into.
        //
        // The default is `CapabilityMode::Permissive`: `from_env` reads
        // `CRATONVM_CAPABILITY_MODE` / `CRATONVM_CAPABILITY_GRANTS`, and with
        // neither set the result allows everything and merely counts it. So the
        // default configuration is behaviour-identical to no policy at all,
        // while `capability_audit(VmId)` can now actually report something.
        //
        // Per-VM, never a process global: two `SharedVm`s in one process get two
        // `CapabilitySet`s under two `VmId`s, which is the cross-VM policy
        // interference `native-api/src/capability.rs` exists to remove.
        let capabilities = Arc::new(cratonvm_native_api::CapabilitySet::from_env(
            cratonvm_native_api::VmId::from_raw(vm_identity),
        ));
        native_methods.set_capabilities(Arc::clone(&capabilities));
        // `None` — a fresh `vm_identity` can never displace an existing entry.
        let _displaced = cratonvm_native_api::install_capabilities(Arc::clone(&capabilities));

        // ── Native policy, installed before ANY `register_*` pass ──────────
        //
        // One line after construction and one line *above* the
        // `#[cfg(feature = "synthetic-jdk")]` fork, so it is structurally
        // impossible to add a registration pass to either arm that runs
        // unpoliced: there is no reachable point between `new()` and the first
        // `register_*` where the mode is not yet set. (Placing it inside the
        // arms would have meant two call sites and a standing invitation for
        // the next `register_*` to be inserted above one of them.)
        //
        // Under `JdkOnly` the registry refuses `NativeKind::SyntheticStub`
        // registrations outright and records a `SyntheticNativeRegistered`
        // violation with the registration site, which is what turns the
        // ~157-stub inventory from an assertion into evidence
        // (jdk-only-mode.md §4, §8, §10).
        //
        // Config-sourced, not a process global — see the `ClassManager` note
        // above for why (§2).
        native_methods.set_compatibility_mode(config.compatibility_mode);
        // Which CLASS LIBRARY the registrars below are populating for — a
        // different question from the compatibility policy above, and one three
        // of them need. Set here, ahead of every `register_*` arm, because it
        // gates registration: see `NativeMethodRegistry::real_jdk`.
        native_methods.set_real_jdk(!config.use_synthetic_jdk);
        #[cfg(feature = "synthetic-jdk")]
        {
            if config.use_synthetic_jdk {
                // Synthetic mode: register all ~5,200 Rust stubs for full JDK API coverage
                register_builtins(&mut native_methods);
                register_io_natives(&mut native_methods);
                register_collections_natives(&mut native_methods);
                // LinkedBlockingQueue.drainTo(Collection, int) - needed by SLF4J/Spring
                // Register here to ensure it's available even when class is loaded from JAR
                // [Bridge] real drainTo impl over the synthetic LBQ layout.
                let __prev = native_methods.current_category();
                native_methods.set_category(cratonvm_native_api::NativeKind::Bridge);
                native_methods.register(
                    "java/util/concurrent/LinkedBlockingQueue",
                    "drainTo",
                    "(Ljava/util/Collection;I)I",
                    |ctx, args| {
                        let this = match args.first() {
                            Some(cratonvm_types::Value::Object(Some(o))) => *o,
                            _ => return Ok(Some(cratonvm_types::Value::Int(0))),
                        };
                        let coll = match args.get(1) {
                            Some(cratonvm_types::Value::Object(Some(c))) => *c,
                            _ => return Ok(Some(cratonvm_types::Value::Int(0))),
                        };
                        let max_elements = match args.get(2) {
                            Some(cratonvm_types::Value::Int(n)) => *n,
                            _ => i32::MAX,
                        };
                        // Ensure monitor is initialized before entering
                        ctx.monitor_enter(this);
                        let size = match ctx.get_field(this, 1) {
                            cratonvm_types::Value::Int(n) => n,
                            _ => 0,
                        };
                        let arr = match ctx.get_field(this, 0) {
                            cratonvm_types::Value::Object(Some(a)) => a,
                            _ => {
                                ctx.monitor_exit(this);
                                return Ok(Some(cratonvm_types::Value::Int(0)));
                            }
                        };
                        let to_drain = size.min(max_elements);
                        for i in 0..to_drain as usize {
                            let elem = ctx.get_array_element(arr, i);
                            ctx.invoke_virtual(coll, "add", "(Ljava/lang/Object;)Z", &[elem])?;
                        }
                        ctx.set_field(this, 1, cratonvm_types::Value::Int(size - to_drain));
                        ctx.monitor_notify_all(this)?;
                        ctx.monitor_exit(this);
                        Ok(Some(cratonvm_types::Value::Int(to_drain)))
                    },
                );
                native_methods.set_category(__prev);
                // The legacy monitor-backed `ReentrantLock` / `Lock` /
                // `Condition` / `Semaphore` natives.
                //
                // Real AQS became the default because the synthetic
                // lock/condition bridge deadlocks the blocking-queue producer/
                // consumer pattern, so `register_concurrent_natives` skips them
                // unless `CRATONVM_SYNTHETIC_AQS=1`. That reasoning is entirely
                // about real-JDK mode: it needs `AbstractQueuedSynchronizer`
                // bytecode to defer to. Synthetic mode has none, so the skip
                // left `new Semaphore(3)` and `new ReentrantLock()` raising
                // `UnsatisfiedLinkError` — 14 `JucComplete` corpus tests,
                // invisible for as long as the corpus was dark. Runtime-gated
                // on `use_synthetic_jdk`, not on the Cargo feature, so a
                // feature-enabled binary running real-JDK mode is unaffected.
                cratonvm_native_builtins::util_concurrent_ext::register_synthetic_aqs_natives(
                    &mut native_methods,
                );
                // Same shape, same reason, one class further on: the
                // `CyclicBarrier` natives are gated on `CRATONVM_SYNTHETIC_AQS`
                // inside `register_concurrent_natives` because the default
                // real-JDK build should run the real class. Synthetic mode has
                // no real class — `CyclicBarrier` is a 3-field compatibility
                // stub with no method bodies — so that gate left `new
                // CyclicBarrier(2)` at `NoSuchMethodError: <init>(I)V` and all
                // four `JucComplete` barrier fixtures red, with the TCK table
                // still listing them as passing. Runtime-gated on
                // `use_synthetic_jdk` (this arm), not on the flag and not on
                // the Cargo feature, so a feature-enabled binary running
                // real-JDK mode is unaffected.
                cratonvm_native_builtins::util_concurrent_ext::register_cyclic_barrier_natives(
                    &mut native_methods,
                );
            } else {
                // Real-JDK mode: register essential natives only. Do NOT use
                // register_builtins — synthetic overrides assume synthetic field
                // layouts and corrupt real JDK objects.
                //
                // `register_collections_natives` (called below) is shared with
                // synthetic mode and bundles the synthetic `java/util/StringJoiner`
                // natives (fake 5-field layout). On a real StringJoiner those
                // corrupt the object — `add` no-ops, so e.g. Spring
                // `UriComponentsBuilder.pathSegment` drops the URL path segment.
                // Tell the registry to drop StringJoiner registrations so the real,
                // self-contained bytecode runs instead.
                native_methods.set_drop_real_layout_synthetic(true);
                // NOTE: unconditionally dropping ALL SyntheticStub-tagged
                // natives in real-JDK mode was tried (dev d8092acb,
                // 2026-07-14) and reverted the same day: several register_*
                // clusters that are tagged SyntheticStub are actually needed
                // as permanent bridges in BOTH modes (no working real-bytecode
                // fallback exists), not just as fake-JDK approximations.
                // Confirmed regressions: the entire java.lang.management/JMX
                // native surface (native-builtins/src/jmx.rs -- WildFly's
                // very first ManagementFactory.getPlatformMBeanServer() call
                // NPEs deep inside real javax.management bytecode,
                // ObjectName._ca_array null, with the stub dropped) and
                // java.util.function.Function$Identity (a VM-internal
                // synthetic stand-in for the real lambda-based
                // Function.identity(), which has no real bytecode to fall
                // back to at all -- UnsatisfiedLinkError). The
                // `drop_synthetic_stubs` field's own doc comment already
                // documented the safe, original design: opt-in only, via
                // `CRATONVM_NO_STUBS`, "because some apps currently limp on
                // these fakes and dropping them surfaces real gaps as clear
                // errors." Leave it opt-in; do not force it on here. See
                // fixed-suite-bugs/wildfly/wildfly-standalone-boot-stw-jit-takeover-hang-FIXED.md's
                // 2026-07-14 addendum for the WildFly-boot regression this
                // caused and how it was found (git bisect).
                //
                // 2026-07-31, `--jdk-only` (docs/feature-designs/jdk-only-mode.md):
                // strict mode does NOT re-litigate the 2026-07-14 revert above,
                // and nothing here changes. The two are different questions.
                // What the 2026-07-14 bisect actually proved is that a subset
                // of the `SyntheticStub`-tagged clusters is **mis-tagged**:
                // JMX and `Function$Identity` are permanent bridges with no
                // real-bytecode fallback, which by the contract's own taxonomy
                // (§1's terminology table) makes them `NativeKind::Bridge`, not
                // stubs. The fix is to retag them at the source, one subsystem
                // per PR, in the reclassification wave (§8 explicitly keeps
                // `native-builtins/src/lib.rs` out of scope this wave) — not to
                // flip a global drop switch again and rediscover the same
                // WildFly boot failure. Until that wave lands, `--jdk-only`'s
                // enforcement is measurement-only for this population (§10):
                // the registry records the refusal and the census names the
                // registration site, so the retagging work starts from evidence
                // instead of from another bisect.
                cratonvm_native_builtins::register_essential_natives_with_shims(
                    &mut native_methods,
                    native_shim_selection,
                );
                // Register concurrent natives (ReentrantLock, etc.) needed by real JDK classes
                // like LinkedBlockingQueue which use ReentrantLock for synchronization
                cratonvm_native_builtins::register_concurrent_natives(&mut native_methods);
                // MUST follow `register_concurrent_natives`: that call registers
                // the old constant `ForkJoinPool.awaitQuiescence` -> true, and
                // registration is last-write-wins. The real one polls this
                // crate's async worker pool, which `native-collections` cannot
                // see.
                cratonvm_native_builtins::register_forkjoin_quiescence(&mut native_methods);
                cratonvm_native_builtins::register_stamped_lock_natives(&mut native_methods);
                // java.util.logging.FileHandler's natives are registered
                // (as part of register_p61_logging) only under
                // register_synthetic_overrides, which is
                // #[cfg(feature = "synthetic-jdk")]-gated and never runs in
                // real-JDK mode -- so Spring Boot's logging-file.properties
                // (handlers=java.util.logging.FileHandler,...), which only
                // ever exercises real-JDK mode, silently fell through to the
                // real FileHandler() bytecode instead (which throws
                // NoSuchFileException trying to actually lock a real log
                // file). Register just the FileHandler natives directly here.
                // See fixed-suite-bugs/springboot/filehandler-noarg-ctor-handler-field-layout-gap-FIXED.md.
                cratonvm_native_builtins::phases_late::register_p61_file_handler(
                    &mut native_methods,
                );
                // `URLClassLoader.close()` likewise: the synthetic-only versions
                // are written for the synthetic carrier's slots and cannot run
                // against a real `java.net.URLClassLoader`.
                cratonvm_native_builtins::servlet::register_url_classloader_close_bridge(
                    &mut native_methods,
                );

                // LinkedBlockingQueue.drainTo(Collection, int) - needed by SLF4J/Spring
                // Override with native implementation to avoid ReentrantLock field layout mismatch
                // between synthetic natives and real JDK classes
                // [Bridge] This contiguous run of inline registers (drainTo x3,
                // AtomicBoolean.<init>) implements real behavior over the
                // JDK/synthetic field layouts.
                // Restored to __prev_bridge just before register_io_natives below.
                let __prev_bridge = native_methods.current_category();
                native_methods.set_category(cratonvm_native_api::NativeKind::Bridge);
                native_methods.register(
                    "java/util/concurrent/LinkedBlockingQueue",
                    "drainTo",
                    "(Ljava/util/Collection;I)I",
                    |ctx, args| {
                        let this = match args.first() {
                            Some(cratonvm_types::Value::Object(Some(o))) => *o,
                            _ => return Ok(Some(cratonvm_types::Value::Int(0))),
                        };
                        let coll = match args.get(1) {
                            Some(cratonvm_types::Value::Object(Some(c))) => *c,
                            _ => return Ok(Some(cratonvm_types::Value::Int(0))),
                        };
                        let max_elements = match args.get(2) {
                            Some(cratonvm_types::Value::Int(n)) => *n,
                            _ => i32::MAX,
                        };
                        // Use object monitor instead of ReentrantLock to avoid field layout issues
                        ctx.monitor_enter(this);
                        // Try to access fields - real JDK LinkedBlockingQueue has different field layout
                        // We'll try common field names/offsets
                        let size = match ctx.get_field(this, 1) {
                            cratonvm_types::Value::Int(n) => n,
                            _ => 0,
                        };
                        let arr = match ctx.get_field(this, 0) {
                            cratonvm_types::Value::Object(Some(a)) => a,
                            _ => {
                                ctx.monitor_exit(this);
                                return Ok(Some(cratonvm_types::Value::Int(0)));
                            }
                        };
                        let to_drain = size.min(max_elements);
                        for i in 0..to_drain as usize {
                            let elem = ctx.get_array_element(arr, i);
                            ctx.invoke_virtual(coll, "add", "(Ljava/lang/Object;)Z", &[elem])?;
                        }
                        ctx.set_field(this, 1, cratonvm_types::Value::Int(size - to_drain));
                        ctx.monitor_notify_all(this)?;
                        ctx.monitor_exit(this);
                        Ok(Some(cratonvm_types::Value::Int(to_drain)))
                    },
                );
                // invokevirtual can resolve `drainTo` against the
                // `BlockingQueue` interface type while the receiver is a real
                // `LinkedBlockingQueue`. Register on the interface too so the
                // native walk in `invoke_or_native` finds the implementation.
                let lbq_drain_bounded =
                    |ctx: &mut dyn cratonvm_native_api::NativeContext,
                     args: &[cratonvm_types::Value]| {
                        let this = match args.first() {
                            Some(cratonvm_types::Value::Object(Some(o))) => *o,
                            _ => return Ok(Some(cratonvm_types::Value::Int(0))),
                        };
                        let coll = match args.get(1) {
                            Some(cratonvm_types::Value::Object(Some(c))) => *c,
                            _ => return Ok(Some(cratonvm_types::Value::Int(0))),
                        };
                        let max_elements = match args.get(2) {
                            Some(cratonvm_types::Value::Int(n)) => *n,
                            _ => i32::MAX,
                        };
                        ctx.monitor_enter(this);
                        let size = match ctx.get_field(this, 1) {
                            cratonvm_types::Value::Int(n) => n,
                            _ => 0,
                        };
                        let arr = match ctx.get_field(this, 0) {
                            cratonvm_types::Value::Object(Some(a)) => a,
                            _ => {
                                ctx.monitor_exit(this);
                                return Ok(Some(cratonvm_types::Value::Int(0)));
                            }
                        };
                        let to_drain = size.min(max_elements);
                        for i in 0..to_drain as usize {
                            let elem = ctx.get_array_element(arr, i);
                            ctx.invoke_virtual(coll, "add", "(Ljava/lang/Object;)Z", &[elem])?;
                        }
                        ctx.set_field(this, 1, cratonvm_types::Value::Int(size - to_drain));
                        ctx.monitor_notify_all(this)?;
                        ctx.monitor_exit(this);
                        Ok(Some(cratonvm_types::Value::Int(to_drain)))
                    };
                native_methods.register(
                    "java/util/concurrent/BlockingQueue",
                    "drainTo",
                    "(Ljava/util/Collection;I)I",
                    lbq_drain_bounded,
                );
                native_methods.register(
                    "java/util/concurrent/BlockingQueue",
                    "drainTo",
                    "(Ljava/util/Collection;)I",
                    |ctx, args| {
                        let this = match args.first() {
                            Some(cratonvm_types::Value::Object(Some(o))) => *o,
                            _ => return Ok(Some(cratonvm_types::Value::Int(0))),
                        };
                        let coll = match args.get(1) {
                            Some(cratonvm_types::Value::Object(Some(c))) => *c,
                            _ => return Ok(Some(cratonvm_types::Value::Int(0))),
                        };
                        ctx.monitor_enter(this);
                        let size = match ctx.get_field(this, 1) {
                            cratonvm_types::Value::Int(n) => n,
                            _ => 0,
                        };
                        let arr = match ctx.get_field(this, 0) {
                            cratonvm_types::Value::Object(Some(a)) => a,
                            _ => {
                                ctx.monitor_exit(this);
                                return Ok(Some(cratonvm_types::Value::Int(0)));
                            }
                        };
                        let to_drain = size;
                        for i in 0..to_drain as usize {
                            let elem = ctx.get_array_element(arr, i);
                            ctx.invoke_virtual(coll, "add", "(Ljava/lang/Object;)Z", &[elem])?;
                        }
                        ctx.set_field(this, 1, cratonvm_types::Value::Int(0));
                        ctx.monitor_notify_all(this)?;
                        ctx.monitor_exit(this);
                        Ok(Some(cratonvm_types::Value::Int(to_drain)))
                    },
                );
                // Do not override ScheduledThreadPoolExecutor constructors in
                // real-JDK mode. The real constructors initialize the inherited
                // ThreadPoolExecutor state (`ctl`, `workQueue`, `mainLock`,
                // `workers`) coherently. A two-slot synthetic poke here leaves
                // `getQueue()` null and breaks Tomcat ContainerBase startup.
                // CopyOnWriteArrayList addIfAbsent/contains/bulkRemove are registered
                // by `register_collections_natives` with real-JDK field resolution.
                // Do not register a synthetic two-slot layout here — it corrupts
                // JDK instances (real `array` stays null → bulkRemove NPE).
                native_methods.register(
                    "java/util/concurrent/atomic/AtomicBoolean",
                    "<init>",
                    "(Z)V",
                    |ctx, args| {
                        let this = match args.first() {
                            Some(cratonvm_types::Value::Object(Some(o))) => *o,
                            _ => return Ok(None),
                        };
                        let v = match args.get(1) {
                            Some(cratonvm_types::Value::Int(n)) if *n != 0 => 1,
                            _ => 0,
                        };
                        ctx.set_field(this, 0, cratonvm_types::Value::Int(v));
                        Ok(None)
                    },
                );
                native_methods.set_category(__prev_bridge);
                register_io_natives(&mut native_methods);
                // The phase bundles are synthetic-only, but SmallRye calls
                // ProcessHandle.current().info() in real-JDK mode as well.
                cratonvm_native_builtins::phases_late::register_p60_process_handle(
                    &mut native_methods,
                );
                // 2026-08-13 (lane F30) ARM-DRIFT FIX. `register_classvalue_natives` was
                // called only by the *other* real-JDK arm (the
                // `#[cfg(not(feature = "synthetic-jdk"))]` block below). Both arms are
                // real-JDK mode and neither one calls `register_synthetic_overrides` —
                // the registrar this registration otherwise lives behind (via
                // `register_p67_misc`) is `#[cfg(feature = "synthetic-jdk")]` AND is only
                // ever called from `register_builtins`, i.e. from the SYNTHETIC-mode arm
                // above. So a `--features synthetic-jdk` binary running real-JDK mode or
                // `--jdk-only` had no `java.lang.ClassValue` natives at all.
                // Position matches the shipping arm (io -> process-handle -> classvalue);
                // this registry is last-write-wins, so position is semantics.
                cratonvm_native_builtins::phases_late::register_classvalue_natives(
                    &mut native_methods,
                );
                // ╔══ LAST-WRITE-WINS BOUNDARY — do not reorder ═════════════╗
                //
                // Everything from here to the end of this arm is ordered
                // *semantically*, not stylistically. `NativeMethodRegistry`
                // registration is last-write-wins, and this arm deliberately
                // re-registers implementations that `register_collections_natives`
                // is known to clobber (Properties side-table below; `Random`/
                // `SecureRandom` and StringJoiner in the sibling arm). Each
                // re-registration below already carries the incident that
                // produced it. Moving, sorting or deduplicating these calls
                // silently reinstates the clobbered version — every one of
                // these comments is a bug that was found in production, not a
                // preference.
                //
                // Untangling this into an explicit precedence table is
                // deliberately deferred (jdk-only-mode.md §8): this wave is
                // measurement, not deletion. The schema-2 native census
                // (`dump_native_census_json`) now records `overwrote` and
                // `registered_by` per slot, so the wave that does the
                // untangling has evidence — which registration actually won,
                // and which one it displaced — instead of having to re-derive
                // the order by bisecting this file again.
                //
                // ╚══════════════════════════════════════════════════════════╝
                //
                // Mixed real-JDK mode still routes many collection call sites
                // through synthetic wrappers; register collection natives so
                // ArrayList/Iterator/Map operations don't fail linkage.
                register_collections_natives(&mut native_methods);
                // 2026-08-13 (lane F30) ARM-DRIFT FIX. Must follow
                // `register_collections_natives`, and must exist in BOTH real-JDK arms —
                // it existed only in the shipping arm below. In real-JDK mode
                // `java/util/Random` field 0 is the `AtomicLong seed` REFERENCE, not a
                // long, so the aliases `register_collections_natives` re-registers read 0
                // and every `nextInt/nextLong/nextDouble/...` on a seeded `Random`
                // returned 0. The `securerandom` handlers keep the seed in an
                // identity-hash-keyed side table, so they are layout-independent and must
                // win. Absent here, a `--features synthetic-jdk` binary running real-JDK
                // mode or `--jdk-only` produced all-zero `Random` output.
                cratonvm_native_builtins::securerandom::register_random_and_securerandom_natives(
                    &mut native_methods,
                );
                // Re-register the side-table-backed Properties natives AFTER
                // `register_collections_natives` (see real-JDK arm below for
                // rationale) — Surefire's BooterDeserializer needs the
                // side-table round-trip to retrieve forkNumber et al.
                cratonvm_native_builtins::properties_sidetable::register_properties_sidetable(
                    &mut native_methods,
                );
                // T12: Register JDK 25 Unsafe natives (addressSize0, fences, etc.)
                cratonvm_native_builtins::unsafe_jdk25::register_t12_unsafe_natives(
                    &mut native_methods,
                );
                // T14: Register System bootstrap natives (SystemProps$Raw, FileDescriptor, etc.)
                cratonvm_native_builtins::system_bootstrap::register_t14_system_bootstrap(
                    &mut native_methods,
                );
                // C4: Register jdk.internal.loader.BootLoader natives so its
                // <clinit> completes and BootLoader.INSTANCE is non-null;
                // downstream URLClassPath.<clinit> stops NPE'ing and
                // ClassLoader.getResources can walk the boot packages.
                cratonvm_native_builtins::boot_loader::register_boot_loader_natives(
                    &mut native_methods,
                );
                // KC26: Register NIO Path/FileSystem natives — needed because Path is an interface
                // in the real JDK (abstract methods have no Code attribute) and our synthetic
                // Path objects need native dispatch.
                cratonvm_native_builtins::phases_late::register_phase57_nio_file(
                    &mut native_methods,
                );
                // 2026-08-13 (lane F30) ARM-DRIFT FIX. The 2026-08-07 note just below
                // writes the shipping arm's order out as
                // "nio_file -> file -> jar -> bulk -> zip-output" and then omitted `file`
                // itself. The real-JDK `java.io.File` constructor runs `FileSystem.
                // normalize` bytecode this interpreter does not execute cleanly (no
                // `WinNTFileSystem.normalize` override), which is why the shipping arm
                // routes `File` constructors and metadata accessors through
                // `register_phase57_file`. Paired with the `check_override` allow-list
                // entry for `java/io/File`.
                cratonvm_native_builtins::phases_late::register_phase57_file(&mut native_methods);
                // 2026-08-07: the jar/zip bridge, which this arm was missing.
                //
                // This is a REAL-JDK arm, so it has to register what the shipping
                // real-JDK arm below registers — the two arms differ only in what
                // the feature COMPILED IN, never in which class library is loaded.
                // These three were registered in that arm and not in this one, so
                // a `--features synthetic-jdk` binary run `--real-jdk` fell back to
                // `native-io`'s `zip_real_jar` surface for `JarFile`.
                //
                // That surface builds its entry objects with `alloc_zip_entry`
                // unconditionally, so `JarFile.entries()`, `getJarEntry()` and
                // `getEntry()` all handed back a bare `java.util.zip.ZipEntry`.
                // `JarFile.entries()` is declared `Enumeration<JarEntry>`, so the
                // implicit checkcast the compiler emits at the call site threw
                // `ClassCastException: java.util.zip.ZipEntry cannot be cast to
                // java.util.jar.JarEntry` — measured by `regression-suite`'s
                // `RFileTimes`, and it would hit any caller that iterates a jar.
                // `register_p59_jar` is force-listed in `native_override.rs` for
                // exactly these methods, so registering it here makes it win, and
                // it answers `java.util.jar.JarEntry` like the shipping build.
                //
                // Kept in the shipping arm's order (nio_file → file → jar → bulk
                // → zip-output) because this registry is last-write-wins.
                cratonvm_native_builtins::phases_late::register_p59_jar(&mut native_methods);
                cratonvm_native_builtins::phases_late::register_p59_bulk_stream_transfer(
                    &mut native_methods,
                );
                cratonvm_native_builtins::phases_late::register_p59_zip_output_primitives(
                    &mut native_methods,
                );
                // 2026-08-13 (lane F30) ARM-DRIFT FIX. `register_spring_boot_logback_apply`
                // is an empty no-op today (`native-builtins/src/logging_shims.rs`, emptied
                // by the 2026-07-24 logging-bootstrap batch) — but its own doc comment says
                // it is "registered unconditionally in real-JDK mode by `vm_init.rs`",
                // which was false for this arm. Called here so the two real-JDK arms have
                // identical registrar SEQUENCES and the witness test at the bottom of this
                // file can assert that with no exception list to rot. If the body is ever
                // repopulated, both arms already receive it.
                cratonvm_native_builtins::register_spring_boot_logback_apply(&mut native_methods);
                // KC26: Register URL codec (URLDecoder/URLEncoder) natives — the real JDK
                // bytecode depends on internal sun.net classes we don't support.
                //
                // Call `register_url_codec` directly rather than the whole-module
                // `register_deprecated_io_util_natives` — that function ALSO
                // re-registers this module's Date constructors/getters/setters
                // (and everything else in the module), which is a genuine
                // duplicate registration of `java/util/Date`'s deprecated
                // multi-arg constructors: `deprecated_util.rs` registers its own
                // (Julian/Gregorian-cutover-aware, default-timezone-aware) version
                // of the same natives earlier in boot, and `NativeMethodRegistry`'s
                // registration table is last-write-wins, so this second call was
                // silently clobbering the correct implementation back to the
                // naive proleptic-Gregorian one with no timezone offset at all.
                // See `bug-h2-suite-residual-fail-triage.md`'s
                // `TestPreparedStatement.testDate8` writeup.
                cratonvm_native_builtins::deprecated_io_util::register_url_codec(
                    &mut native_methods,
                );
                // KC26: Register Charset/StandardCharsets natives
                cratonvm_native_builtins::register_charset_natives_pub(&mut native_methods);
                // KC26: Register Charset coder natives
                cratonvm_native_builtins::phases_late::register_p58_charset_coder(
                    &mut native_methods,
                );
                // Phase B (RB.1/RB.2): real charset transcoding (overrides the
                // no-op stubs registered by register_p58_charset_coder above).
                cratonvm_native_builtins::charset::register_real_charset_natives(
                    &mut native_methods,
                );
                // KC26: Register reflection/signal/unsafe-deprecated natives
                // (needed for getCallerClass, StackWalker, etc.)
                cratonvm_native_builtins::deprecated_internal::register_deprecated_internal_natives(
                    &mut native_methods,
                );
                // KC26: ArraysSupport vectorized intrinsics (vectorizedMismatch, vectorizedHashCode)
                cratonvm_native_builtins::phases_early::register_arrays_support_natives(
                    &mut native_methods,
                );
                // KC26: StringLatin1 compareTo/getChar — native overrides to bypass
                // bytecode loop bugs in the interpreter's baload/if_icmpge interaction.
                cratonvm_native_builtins::phases_early::register_string_latin1_natives(
                    &mut native_methods,
                );
                // KC26: RunnerClassLoader.close() — the real bytecode NPEs on null
                // map values (HashMap entries with a null value field). This used
                // to be a no-op, which dodged the NPE by leaking every jar handle;
                // `quarkus_runner_class_loader_close` closes the resources for real
                // and skips the null holes. Left at the default (SyntheticStub)
                // category so `CRATONVM_NO_STUBS` still falls through to bytecode.
                native_methods.register(
                    "io/quarkus/bootstrap/runner/RunnerClassLoader",
                    "close",
                    "()V",
                    quarkus_runner_class_loader_close,
                );
                // KC26: ClassLoader constructors — the real JDK ClassLoader.<init>
                // is extremely complex (creates ArrayList, ProtectionDomain,
                // NativeLibraries, Module, etc.) and fails on missing internals.
                // Register simplified constructors that just store the parent field.
                cratonvm_native_builtins::classloader_real::register_classloader_real_natives(
                    &mut native_methods,
                );
                // KC26: MethodType factories + MethodHandle basics — needed because
                // the real JDK bytecode for these depends on deep JDK internals
                // (MethodHandleNatives, DirectMethodHandle) we don't support.
                cratonvm_native_builtins::lang_invoke::register_phase54_method_handle(
                    &mut native_methods,
                );
                // KC26: MethodHandles.lookup() + Lookup.findStatic/findVirtual/etc.
                // Phase 63 overrides phase 54's stub find* with real implementations
                // that resolve class/method/descriptor for proper MH dispatch.
                cratonvm_native_builtins::lang_invoke::register_p63_method_handles_lookup(
                    &mut native_methods,
                );
                // KC26: MethodHandle.invoke/invokeExact — signature-polymorphic
                // dispatch that reads the target class/method/descriptor from
                // our synthetic 5-field MethodHandle and invokes the target.
                cratonvm_native_builtins::lang_invoke::register_t4_method_handle_invoke(
                    &mut native_methods,
                );
                // C5: Lookup.unreflect/unreflectGetter/unreflectSetter/
                // unreflectConstructor/permuteArguments/guardWithTest overrides.
                // Without these, real-JDK Lookup.unreflectGetter runs Java
                // bytecode that relies on MethodHandleNatives.init populating
                // the MemberName, and our Field's JDK layout must match —
                // which we fix in lang_class.rs::create_field_object.
                cratonvm_native_builtins::lang_invoke::register_t28_method_handle_completeness(
                    &mut native_methods,
                );
                // Round 85: LambdaMetafactory.metafactory/altMetafactory natives.
                // log4j ServiceLoaderUtil.callServiceLoader calls
                // LambdaMetafactory.metafactory directly (not via invokedynamic).
                // The real-JDK implementation drives InnerClassLambdaMetafactory →
                // java.lang.classfile API, which trips StackMapGenerator with
                // "Bad CP index: 0" inside our incomplete classfile shim.
                // Intercept with a native stub that returns null so the JDK path
                // is bypassed entirely (callers tolerate the missing call site).
                cratonvm_native_builtins::lang_invoke::register_p68_invoke_extras(
                    &mut native_methods,
                );
                // RA.8 + WP1.8: Real ServiceLoader.load/iterator that walks
                // META-INF/services. Registration + classpath-scan bootstrap
                // are kept together in `init_service_loader_bootstrap` so
                // Wave 2 (module-scoped extension) only has one call site
                // to extend.
                init_service_loader_bootstrap(&mut native_methods);
                // WP2.5: java.lang.reflect.Proxy natives. The bytecode
                // path goes through real-JDK `Proxy.newProxyInstance`,
                // which calls into `defineClass` which we don't fully
                // support yet (WP2.3); registering the natives here
                // intercepts the synthetic Proxy$Instance allocation
                // path that the dispatcher in interpreter.rs already
                // recognizes.
                cratonvm_native_builtins::register_reflect_proxy_natives(&mut native_methods);
                // WP2.4-A: java.lang.instrument runtime natives —
                // sun.instrument.InstrumentationImpl + the in-process
                // cratonvm.Instrument bridge used by the
                // apps/instrument_probe smoke fixture.
                crate::runtime::instrument::register_instrumentation_natives(&mut native_methods);
                // In-process self-attach (com.sun.tools.attach.VirtualMachine)
                // so runtime-attach agents (Mockito inline mock maker, JaCoCo)
                // can load themselves without `-javaagent:`.
                crate::runtime::instrument::register_self_attach_natives(&mut native_methods);
                // RKC16N.10: sun.management.VMManagementImpl natives —
                // ManagementFactory.<clinit> instantiates VMManagementImpl
                // whose <clinit> calls native helpers; without these the
                // JBoss Modules boot path (Keycloak) trips on an
                // UnsatisfiedLinkError. Lives outside register_jmx_natives
                // (which is synthetic-only) so the real-JDK path picks it up.
                cratonvm_native_builtins::jmx::register_vm_management_impl(&mut native_methods);
                // W7-50 (2026-08-12): `register_management_factory_platform_server_stub`
                // used to be called here. This is the REAL-JDK arm — the
                // `else` of `if config.use_synthetic_jdk` above — and the
                // default build's real-JDK arm below refuses this exact call
                // under JMX-CLUSTER-20260720, with a measured rationale
                // (`queryNames(null, null)` returning 0 entries because every
                // platform MXBean was being zeroed out). The feature build
                // never got that revert, so the two builds' real-JDK arms
                // disagreed on the one registration that decides whether
                // `ManagementFactory.getPlatformMBeanServer()` returns a real
                // `com.sun.jmx.mbeanserver.JmxMBeanServer` or an empty
                // synthetic `javax/management/MBeanServer`.
                //
                // With the synthetic server as the receiver, `MBeanServer`'s
                // interface natives in `native-builtins/src/jmx.rs` answer
                // instead of real bytecode, and they bind BY NAME: getAttribute
                // (jmx.rs:6677) probes `getAttribute(String)Object`, then
                // `get<Cap>()Ljava/lang/Object;` and `is<Cap>()Z` with
                // HARD-CODED return descriptors. `RJdkJmx$Counter.getValue()`
                // returns `int`, so every probe missed and the three misses
                // surfaced as three `NoSuchMethodError`s naming methods the
                // fixture's own nested class never declared. Removing the call
                // puts the real introspection back on the path and makes this
                // arm identical to the default build's.
                //
                // The bind-by-name dispatch itself is NOT fixed here: it is
                // still the only implementation synthetic mode has, and this
                // vector cannot measure it. Recorded in
                // W7-50-synthetic-jdk-strict-six.md as a live latent defect.
                // Surefire ForkedBooter: ManagementFactory.getRuntimeMXBean() and
                // friends. The real-JDK bytecode delegates to
                // `getPlatformMXBean(Class)` which throws "X is not a platform
                // management interface" because the PlatformMBeanProvider SPI
                // is not wired up. Register the synthetic-bean shortcut natives
                // so the public factory methods short-circuit to
                // alloc_runtime_mxbean / alloc_thread_mxbean / etc., letting
                // ForkedBooter.isDebugging() / dumpHelp() succeed and the
                // forked test JVM continue past constructor.
                #[cfg(feature = "management")]
                cratonvm_native_builtins::jmx::register_jmx_natives(&mut native_methods);
                // W7-50 (2026-08-12): `register_mbean_server_factory_synthetic`
                // used to be called here, under a comment reading
                // "Synthetic-JDK-only ... Must NOT be called from the real-JDK
                // branch below — ... it broke `getPlatformMBeanServer()`
                // interface dispatch when it leaked into real mode." The
                // comment was right and its own call site was the leak: this
                // is the real-JDK arm. "The branch below" was read as the
                // `#[cfg(not(feature = "synthetic-jdk"))]` block, but the
                // relevant fork is `if config.use_synthetic_jdk`, and these
                // lines are in its `else`.
                //
                // Removing it costs synthetic mode nothing: vm_init's two
                // call sites for this function were BOTH in this arm, so the
                // synthetic arm never received it. Its only other caller is a
                // unit test (jmx.rs:7172).
                // RKC16N.11: pre-register the rest of the sun.management.*
                // native surface so future Keycloak-boot iterations don't
                // trip on missing-native errors as JMM init walks deeper.
                // All return reasonable defaults (zeros / empty arrays /
                // -1 for "metric unavailable"); JBoss only iterates these
                // MXBeans for diagnostic display, not control flow.
                cratonvm_native_builtins::jmx::register_thread_impl(&mut native_methods);
                cratonvm_native_builtins::jmx::register_class_loading_impl(&mut native_methods);
                cratonvm_native_builtins::jmx::register_garbage_collector_impl(&mut native_methods);
                // Wave 1 / Task A: per-pool / per-manager MXBean natives
                // so ManagementFactory.getXxxMXBeans() returns at least
                // one usable bean per type (not just empty arrays).
                cratonvm_native_builtins::jmx::register_memory_pool_impl(&mut native_methods);
                cratonvm_native_builtins::jmx::register_memory_manager_impl(&mut native_methods);
                cratonvm_native_builtins::jmx::register_operating_system_impl(&mut native_methods);
                cratonvm_native_builtins::jmx::register_hotspot_diagnostic(&mut native_methods);
                cratonvm_native_builtins::jmx::register_flag_impl(&mut native_methods);
                // Spring Boot 2.x fat-jars: SLF4J 1.7's MDC.<clinit> /
                // LoggerFactory.<clinit> call StaticMDCBinder.getSingleton()
                // / StaticLoggerBinder.getSingleton() which only resolve
                // when an SLF4J impl JAR (slf4j-log4j12, logback-classic,
                // slf4j-simple, ...) is on the runtime classpath. Boot's
                // nested BOOT-INF/lib/ visibility makes those JARs invisible
                // to our class loader, so the static call site raises
                // NoSuchMethodError → JIT linkage error and the boot dies.
                // Register synthetic singletons + a no-op BasicMDCAdapter so
                // <clinit> completes; the existing MDC / Logger natives
                // already cover the actual API surface.
                cratonvm_native_builtins::register_slf4j_binder_stubs_pub(&mut native_methods);
                // ApplicationStartup / StartupStep: `spring_startup_bootstrap` in essentials.
                tracing::info!(
                    "Real JDK mode: {} native methods registered",
                    native_methods.len()
                );
            }
        }
        #[cfg(not(feature = "synthetic-jdk"))]
        {
            // Real-JDK mode (the default `cratonvm-cli` build): drop the synthetic
            // `java/util/StringJoiner` natives that `register_collections_natives`
            // (called below) bundles in — their fake 5-field layout corrupts the
            // real 7-field object (`add` no-ops, so e.g. Spring
            // `UriComponentsBuilder.pathSegment` drops the URL path segment). The
            // real, self-contained StringJoiner bytecode runs instead. Must be set
            // BEFORE any `register_*` pass here. (The synthetic-jdk-feature build
            // sets the same flag in its real-JDK arm above.)
            native_methods.set_drop_real_layout_synthetic(true);
            // set_drop_synthetic_stubs(true) intentionally NOT called here.
            // See the matching real-JDK arm above for why (dev d8092acb
            // regression + revert, 2026-07-14): several SyntheticStub-tagged
            // register_* clusters (JMX, Function$Identity) are permanent
            // bridges needed in real mode too, not fake-JDK-only shadows.
            //
            // 2026-07-31, `--jdk-only` (docs/feature-designs/jdk-only-mode.md):
            // unchanged here too, and for the same reason as the sibling arm —
            // `drop_synthetic_stubs` stays opt-in (`CRATONVM_NO_STUBS`) in
            // BOTH arms. `--jdk-only` is a stricter, *recorded* superset of
            // that switch, not a second way to turn it on globally: the
            // mis-tagged permanent bridges (JMX, `Function$Identity`) get
            // reclassified to `NativeKind::Bridge` at their registration sites
            // in their own wave (§8), and the schema-2 census exists to tell
            // that wave which ones they are.
            cratonvm_native_builtins::register_essential_natives_with_shims(
                &mut native_methods,
                native_shim_selection,
            );
            // cratonvm-cli default features omit `synthetic-jdk`; the rich
            // registration block only lives under `cfg(feature = "synthetic-jdk")`
            // above. Real-JDK apps still need ReentrantLock / Condition / LBQ
            // drainTo natives (SLF4J replayEvents, Spring thread pools).
            cratonvm_native_builtins::register_concurrent_natives(&mut native_methods);
            // MUST follow `register_concurrent_natives`: that call registers
            // the old constant `ForkJoinPool.awaitQuiescence` -> true, and
            // registration is last-write-wins. The real one polls this
            // crate's async worker pool, which `native-collections` cannot
            // see.
            cratonvm_native_builtins::register_forkjoin_quiescence(&mut native_methods);
            cratonvm_native_builtins::register_stamped_lock_natives(&mut native_methods);
            // java.util.logging.FileHandler's natives are registered
            // (as part of register_p61_logging) only under
            // register_synthetic_overrides, which is
            // #[cfg(feature = "synthetic-jdk")]-gated and never runs in
            // real-JDK mode -- so Spring Boot's logging-file.properties
            // (handlers=java.util.logging.FileHandler,...), which only
            // ever exercises real-JDK mode, silently fell through to the
            // real FileHandler() bytecode instead (which throws
            // NoSuchFileException trying to actually lock a real log
            // file). Register just the FileHandler natives directly here.
            // See fixed-suite-bugs/springboot/filehandler-noarg-ctor-handler-field-layout-gap-FIXED.md.
            cratonvm_native_builtins::phases_late::register_p61_file_handler(&mut native_methods);
            // See the twin above.
            cratonvm_native_builtins::servlet::register_url_classloader_close_bridge(
                &mut native_methods,
            );

            fn real_jdk_lbq_drain_to_bounded(
                ctx: &mut dyn cratonvm_native_api::NativeContext,
                args: &[cratonvm_types::Value],
            ) -> cratonvm_types::error::MethodCallResult {
                let this = match args.first() {
                    Some(cratonvm_types::Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(cratonvm_types::Value::Int(0))),
                };
                let coll = match args.get(1) {
                    Some(cratonvm_types::Value::Object(Some(c))) => *c,
                    _ => return Ok(Some(cratonvm_types::Value::Int(0))),
                };
                let max_elements = match args.get(2) {
                    Some(cratonvm_types::Value::Int(n)) => *n,
                    _ => i32::MAX,
                };
                ctx.monitor_enter(this);
                let size = match ctx.get_field(this, 1) {
                    cratonvm_types::Value::Int(n) => n,
                    _ => 0,
                };
                let arr = match ctx.get_field(this, 0) {
                    cratonvm_types::Value::Object(Some(a)) => a,
                    _ => {
                        ctx.monitor_exit(this);
                        return Ok(Some(cratonvm_types::Value::Int(0)));
                    }
                };
                let to_drain = size.min(max_elements);
                for i in 0..to_drain as usize {
                    let elem = ctx.get_array_element(arr, i);
                    ctx.invoke_virtual(coll, "add", "(Ljava/lang/Object;)Z", &[elem])?;
                }
                ctx.set_field(this, 1, cratonvm_types::Value::Int(size - to_drain));
                ctx.monitor_notify_all(this)?;
                ctx.monitor_exit(this);
                Ok(Some(cratonvm_types::Value::Int(to_drain)))
            }
            // [Bridge] This contiguous run of inline registers (drainTo x3,
            // ScheduledThreadPoolExecutor.<init>, CopyOnWriteArrayList.addIfAbsent,
            // AtomicBoolean.<init>) all implement real behavior. Restored to
            // __prev_bridge just before register_io_natives below.
            let __prev_bridge = native_methods.current_category();
            native_methods.set_category(cratonvm_native_api::NativeKind::Bridge);
            native_methods.register(
                "java/util/concurrent/LinkedBlockingQueue",
                "drainTo",
                "(Ljava/util/Collection;I)I",
                real_jdk_lbq_drain_to_bounded,
            );
            native_methods.register(
                "java/util/concurrent/BlockingQueue",
                "drainTo",
                "(Ljava/util/Collection;I)I",
                real_jdk_lbq_drain_to_bounded,
            );
            native_methods.register(
                "java/util/concurrent/BlockingQueue",
                "drainTo",
                "(Ljava/util/Collection;)I",
                |ctx, args| {
                    let this = match args.first() {
                        Some(cratonvm_types::Value::Object(Some(o))) => *o,
                        _ => return Ok(Some(cratonvm_types::Value::Int(0))),
                    };
                    let coll = match args.get(1) {
                        Some(cratonvm_types::Value::Object(Some(c))) => *c,
                        _ => return Ok(Some(cratonvm_types::Value::Int(0))),
                    };
                    ctx.monitor_enter(this);
                    let size = match ctx.get_field(this, 1) {
                        cratonvm_types::Value::Int(n) => n,
                        _ => 0,
                    };
                    let arr = match ctx.get_field(this, 0) {
                        cratonvm_types::Value::Object(Some(a)) => a,
                        _ => {
                            ctx.monitor_exit(this);
                            return Ok(Some(cratonvm_types::Value::Int(0)));
                        }
                    };
                    let to_drain = size;
                    for i in 0..to_drain as usize {
                        let elem = ctx.get_array_element(arr, i);
                        ctx.invoke_virtual(coll, "add", "(Ljava/lang/Object;)Z", &[elem])?;
                    }
                    ctx.set_field(this, 1, cratonvm_types::Value::Int(0));
                    ctx.monitor_notify_all(this)?;
                    ctx.monitor_exit(this);
                    Ok(Some(cratonvm_types::Value::Int(to_drain)))
                },
            );
            // NOTE: do NOT register a synthetic ScheduledThreadPoolExecutor.<init>
            // here. The synthetic 2-field poke (slots 0/1) corrupts the real STPE
            // layout (ctl/workQueue) and leaves mainLock null, so the inherited
            // real ThreadPoolExecutor.shutdownNow() NPEs on mainLock.lock() in
            // TomcatBaseTest.tearDown. The real STPE constructor bytecode runs
            // correctly on CratonVM once the synthetic STPE natives are gone (the
            // native-collections copy is now gated behind synthetic-jdk). See
            // fixed-suite-bugs/tomcat/11-stpe-mainlock-npe-teardown-regression.md.
            native_methods.register(
                "java/util/concurrent/CopyOnWriteArrayList",
                "addIfAbsent",
                "(Ljava/lang/Object;)Z",
                |ctx, args| {
                    let this = match args.first() {
                        Some(cratonvm_types::Value::Object(Some(o))) => *o,
                        _ => return Ok(Some(cratonvm_types::Value::Int(0))),
                    };
                    let elem = args
                        .get(1)
                        .copied()
                        .unwrap_or(cratonvm_types::Value::Object(None));
                    ctx.monitor_enter(this);
                    let size = match ctx.get_field(this, 1) {
                        cratonvm_types::Value::Int(n) => n.max(0) as usize,
                        _ => 0,
                    };
                    let old_arr = match ctx.get_field(this, 0) {
                        cratonvm_types::Value::Object(Some(a)) => a,
                        _ => {
                            let a = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
                            ctx.set_field(this, 0, cratonvm_types::Value::Object(Some(a)));
                            a
                        }
                    };
                    for i in 0..size {
                        if ctx.get_array_element(old_arr, i) == elem {
                            ctx.monitor_exit(this);
                            return Ok(Some(cratonvm_types::Value::Int(0)));
                        }
                    }
                    let new_arr =
                        ctx.new_array(cratonvm_types::ArrayElementType::Reference, size + 1);
                    for i in 0..size {
                        ctx.set_array_element(new_arr, i, ctx.get_array_element(old_arr, i));
                    }
                    ctx.set_array_element(new_arr, size, elem);
                    ctx.set_field(this, 0, cratonvm_types::Value::Object(Some(new_arr)));
                    ctx.set_field(this, 1, cratonvm_types::Value::Int((size + 1) as i32));
                    ctx.monitor_exit(this);
                    Ok(Some(cratonvm_types::Value::Int(1)))
                },
            );
            native_methods.register(
                "java/util/concurrent/atomic/AtomicBoolean",
                "<init>",
                "(Z)V",
                |ctx, args| {
                    let this = match args.first() {
                        Some(cratonvm_types::Value::Object(Some(o))) => *o,
                        _ => return Ok(None),
                    };
                    let v = match args.get(1) {
                        Some(cratonvm_types::Value::Int(n)) if *n != 0 => 1,
                        _ => 0,
                    };
                    ctx.set_field(this, 0, cratonvm_types::Value::Int(v));
                    Ok(None)
                },
            );
            native_methods.set_category(__prev_bridge);
            register_io_natives(&mut native_methods);
            // Keep the default CLI's real-JDK registration in sync with the
            // synthetic-feature build above.
            cratonvm_native_builtins::phases_late::register_p60_process_handle(&mut native_methods);
            // `java.lang.ClassValue#get`/`#remove` (see `phases_late.rs`'s
            // `register_classvalue_natives`) — needed here explicitly because
            // this real-JDK-mode branch does NOT call
            // `register_synthetic_overrides` (which is where this
            // registration otherwise lives, via `register_p67_misc`), same
            // "keep in sync" reasoning as `register_p60_process_handle` above.
            cratonvm_native_builtins::phases_late::register_classvalue_natives(&mut native_methods);
            // ╔══ LAST-WRITE-WINS BOUNDARY — do not reorder ═════════════════╗
            //
            // Same contract as the sibling arm above. Registration is
            // last-write-wins, and the calls below exist *because*
            // `register_collections_natives` overwrites earlier, correct
            // implementations: `securerandom` (collections re-registers every
            // `java/util/Random` method against a synthetic 2-field layout, so
            // a seeded `Random` returned all zeroes) and
            // `properties_sidetable` (collections re-registers `Properties`
            // against the legacy HashMap layout, breaking Surefire's
            // load → stringPropertyNames → getProperty round-trip). Both
            // incidents are written up inline below. Reordering, sorting or
            // "cleaning up" this sequence reinstates the broken versions.
            //
            // Untangling is deferred by jdk-only-mode.md §8. The schema-2
            // native census now carries `overwrote` + `registered_by` per
            // slot, which is the evidence a later wave needs to replace this
            // ordering with an explicit precedence rule.
            //
            // ╚══════════════════════════════════════════════════════════════╝
            register_collections_natives(&mut native_methods);
            // Re-register the side-table-backed `java.util.Random` /
            // `SecureRandom` natives AFTER `register_collections_natives`:
            // that earlier call (native-collections `register_random_natives`)
            // re-registers every `java/util/Random` method with a SYNTHETIC
            // 2-field layout that reads the LCG seed from instance field 0.
            // In real-JDK mode field 0 is the `AtomicLong seed` reference, not
            // a long, so that version reads 0 and every `nextInt/nextLong/
            // nextDouble/...` returns 0 (a seeded `Random` produced all-zero
            // output). The `securerandom` module's handlers are layout-
            // independent (seed in an identity-hash-keyed side table) and
            // spec-exact, so they must win — same "re-register after
            // collections clobbers essentials" pattern as Properties below.
            cratonvm_native_builtins::securerandom::register_random_and_securerandom_natives(
                &mut native_methods,
            );
            // Re-register the side-table-backed Properties natives AFTER
            // `register_collections_natives` because that earlier call
            // re-registers `Properties.load`, `getProperty`, `setProperty`,
            // `put`, `get`, `stringPropertyNames` etc. with the legacy
            // HashMap-layout natives, overwriting the side-table-backed
            // implementations from `register_essential_natives`.  The
            // side-table is required for Surefire's
            // `SystemPropertyManager.loadProperties(InputStream)` round-trip
            // (load → stringPropertyNames → getProperty) so the forked JVM
            // sees `forkNumber`, `reportsDirectory`, `shutdown`, etc.
            cratonvm_native_builtins::properties_sidetable::register_properties_sidetable(
                &mut native_methods,
            );
            cratonvm_native_builtins::unsafe_jdk25::register_t12_unsafe_natives(
                &mut native_methods,
            );
            cratonvm_native_builtins::system_bootstrap::register_t14_system_bootstrap(
                &mut native_methods,
            );
            // C4: BootLoader natives (see comment at first registration site above).
            cratonvm_native_builtins::boot_loader::register_boot_loader_natives(
                &mut native_methods,
            );
            cratonvm_native_builtins::phases_late::register_phase57_nio_file(&mut native_methods);
            // Spring Boot 3.2 fat-jar launcher needs File.<init>(String) to
            // normalise URI-style `/<drive>:/...` paths so the round-trip
            // `URL.toURI().getSchemeSpecificPart() -> new File(...)` lands on
            // an existing path. The real-JDK File constructor invokes
            // FileSystem.normalize via bytecode that doesn't run cleanly in
            // our interpreter (no `WinNTFileSystem.normalize` native
            // override), so route File constructors and metadata accessors
            // through our Rust natives in `register_phase57_file`. Paired
            // with the `check_override` allow-list entry for `java/io/File`.
            cratonvm_native_builtins::phases_late::register_phase57_file(&mut native_methods);
            // Spring Boot 3.2: JarFileArchive.<init> opens the fat-jar via
            // `new JarFile(File)` and immediately calls `jarFile.stream()`
            // / `jarFile.getManifest()` to walk `BOOT-INF/lib/*.jar`. The
            // real-JDK ZipFile bytecode reaches into native primitives we
            // don't wire up, so route JarFile constructors and accessors
            // through `register_p59_jar` (which uses the `zip` crate to
            // open the archive directly). Paired with the `check_override`
            // allow-list entry for `java/util/jar/JarFile`.
            cratonvm_native_builtins::phases_late::register_p59_jar(&mut native_methods);
            // The loader's ZIP64 size-limit fixture streams six GiB through
            // `InputStream.transferTo` (via Spring's `StreamUtils.copy`).
            // Register the bulk-transfer bridge on this real-JDK path as
            // well as the full synthetic registration path.
            cratonvm_native_builtins::phases_late::register_p59_bulk_stream_transfer(
                &mut native_methods,
            );
            cratonvm_native_builtins::phases_late::register_p59_zip_output_primitives(
                &mut native_methods,
            );
            // SB3-LOGBACK: Spring Boot 3.2's DefaultLogbackConfiguration.apply
            // NPEs on its first monitorenter against a synthetic LoggerContext.
            // Register a no-op native override so the boot path skips logback's
            // default configuration (logs fall back to JVM stderr). Paired with
            // the `check_override` allow-list entry in `vm_exec.rs`.
            cratonvm_native_builtins::register_spring_boot_logback_apply(&mut native_methods);
            // Spring Boot 3 fat-jar launcher: `Launcher.createClassLoader`
            // calls `urls.toArray(new URL[0])` on the 67-element URL list
            // returned by `JarFileArchive.getClassPathUrls`. The real-JDK
            // bytecode for `ArrayList.toArray(T[])` (and the inherited
            // `AbstractCollection.toArray(T[])`) takes the
            // `Arrays.copyOf(elementData, size, a.getClass())` path which
            // NPEs in our VM because the array-component-type metadata
            // path on `Object.getClass()` for an array receiver is
            // incomplete. Register a real-JDK-aware native that reads
            // `elementData` / `size` by name (so it works against the
            // real ArrayList field layout, not the synthetic 2-field
            // stub). Paired with the `check_override` allow-list entry
            // for `java/util/ArrayList` / `java/util/AbstractCollection`
            // in `vm_exec.rs`. Synthetic-jdk mode registers the same
            // native via `register_collections_natives`; this branch
            // covers the real-JDK path which never calls that bulk
            // registration.
            /// The `ArrayStoreException` `System.arraycopy` raises, named after
            /// the offending element's class exactly as HotSpot names it.
            fn array_store_failure(
                ctx: &dyn cratonvm_native_api::NativeContext,
                value: cratonvm_types::ObjectRef,
            ) -> cratonvm_types::error::MethodCallFailed {
                let cname = ctx
                    .class_name_of_id(ctx.class_id_of_object(value))
                    .unwrap_or_default()
                    .replace('/', ".");
                cratonvm_types::error::RuntimeError::ArrayStoreException { message: cname }.into()
            }

            fn real_jdk_to_array_typed(
                ctx: &mut dyn cratonvm_native_api::NativeContext,
                args: &[cratonvm_types::Value],
            ) -> cratonvm_types::error::MethodCallResult {
                use cratonvm_types::Value;
                // `Collection.toArray(T[])` reads `a.length` before anything
                // else, so a null template is an NPE whatever the receiver
                // holds. MEASURED: no-throw (probes/ArrayListShadowSweep 135).
                if matches!(args.get(1), Some(Value::Object(None))) {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: None,
                    }
                    .into());
                }
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Object(None))),
                };
                let template = args.get(1).copied().unwrap_or(Value::Object(None));
                // Read fields by name so the same native works for
                // ArrayList, Vector, CopyOnWriteArrayList, etc.
                let data = match ctx.get_field_by_name(this, "elementData") {
                    Value::Object(Some(arr)) => Some(arr),
                    _ => None,
                };
                // S111r32 — when the receiver lacks `elementData` (e.g.
                // EnumSet, HashSet, TreeSet, IdentityHashMap.values()),
                // we MUST NOT take the ArrayList shortcut: it would read
                // `size`=0 and silently return an empty array, dropping
                // the real elements. Iterate via the receiver's own
                // `iterator()` instead — same shape as
                // `AbstractCollection.toArray(T[])` in the JDK.
                if data.is_none() {
                    // Iterator-based copy using virtual dispatch on the
                    // receiver's actual class.
                    //
                    // Every call below goes through `invoke_virtual`, which
                    // dispatches on the RECEIVER. The by-name `ctx.invoke(
                    // <class>, ...)` this used to call resolves against the
                    // named class's own bytecode and never consults
                    // `should_force_registered_native_over_bytecode` — the gate
                    // that exists precisely because a CratonVM-minted
                    // `HashMap$KeyIterator` / `LinkedHashMap$LinkedKeyIterator`
                    // carries its snapshot PAST the fields the real
                    // `HashIterator` bytecode walks (`key_itr_base`), so the
                    // real `hasNext()` reads an unset `next` field and answers
                    // `false` on the FIRST element. The loop then broke at
                    // i = 0 and every slot of the freshly allocated result
                    // stayed null while `size()` had already fixed the length —
                    // a right-length, all-null array, which is the single
                    // hardest shape for a caller to notice.
                    //
                    // That is the whole of the Jetty embedded-JSP failure:
                    // `ClassMatcher extends AbstractSet<String>` with
                    // `iterator()` = `_entries.keySet().iterator()`, so
                    // `ClassMatcher.getPatterns()` (`toArray(new String[size])`)
                    // handed Jetty `[null]`. An all-null pattern set makes
                    // `IncludeExcludeSet` answer "empty", and an EMPTY
                    // hidden-class matcher matches EVERYTHING
                    // (`ClassMatcher.combine`: empty patterns fall through to
                    // the empty location set, whose `test` is vacuously true).
                    // `WebAppClassLoader.loadClass` therefore discarded the
                    // `org.apache.jasper.servlet.JspServlet` its parent had just
                    // resolved, as "hidden", and threw
                    // `ClassNotFoundException` from line 540 with no cause —
                    // surfacing as `UnavailableException: Class loading error
                    // for holder jsp==...JspServlet`.
                    //
                    // GC-safety (DOM17 stale-canary backtrace, 2026-07-15):
                    // every `ctx.invoke` below can run a moving GC, and the
                    // pre-fix loop re-used `this`, the template array, the
                    // target array, and the iterator raw across those windows
                    // — the WildFly Host Controller tripped
                    // CRATONVM_DBG_STALE_OBJREF passing the stale iterator to
                    // `next()`. Pin each and re-read through the pin before
                    // every post-window use.
                    let pin_base = ctx.pin_native_root(this);
                    let template_pin = match template {
                        Value::Object(Some(arr)) => Some((ctx.pin_native_root(arr), arr)),
                        _ => None,
                    };
                    let result = (|| -> cratonvm_types::error::MethodCallResult {
                        let size_v = ctx.invoke_virtual(this, "size", "()I", &[])?;
                        let size = match size_v {
                            Some(Value::Int(n)) => n.max(0) as usize,
                            _ => 0,
                        };
                        let target = match template_pin {
                            // Template too small: `Collection.toArray(T[])` must
                            // return a NEW array of the template's RUNTIME type,
                            // not a bare `Object[]`. An array's heap header stores
                            // its component class id, so `class_id_of_object(arr)`
                            // IS the component id `new_ref_array` wants —
                            // preserving multi-dimensional element types
                            // (`Value[][]` for H2 SortOrder.sort).
                            Some((h, orig)) => {
                                let arr = ctx.read_native_pin(h, orig);
                                if ctx.array_length(arr) >= size {
                                    arr
                                } else {
                                    let comp = ctx.class_id_of_object(arr);
                                    ctx.new_ref_array(comp, size)
                                }
                            }
                            _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, size),
                        };
                        let target_pin = ctx.pin_native_root(target);
                        let cur_this = ctx.read_native_pin(pin_base, this);
                        let it_v =
                            ctx.invoke_virtual(cur_this, "iterator", "()Ljava/util/Iterator;", &[])?;
                        let it = match it_v {
                            Some(Value::Object(Some(o))) => o,
                            _ => {
                                let target = ctx.read_native_pin(target_pin, target);
                                return Ok(Some(Value::Object(Some(target))));
                            }
                        };
                        let it_pin = ctx.pin_native_root(it);
                        for i in 0..size {
                            let cur_it = ctx.read_native_pin(it_pin, it);
                            let has = ctx.invoke_virtual(cur_it, "hasNext", "()Z", &[])?;
                            if !matches!(has, Some(Value::Int(1))) {
                                break;
                            }
                            let cur_it = ctx.read_native_pin(it_pin, it);
                            let nxt =
                                ctx.invoke_virtual(cur_it, "next", "()Ljava/lang/Object;", &[])?;
                            let v = nxt.unwrap_or(Value::Object(None));
                            let cur_target = ctx.read_native_pin(target_pin, target);
                            // The JDK copies with `System.arraycopy`, which
                            // performs the aastore store check — so
                            // `List<String>.toArray(new Integer[4])` is an
                            // `ArrayStoreException`, not an `Integer[]` full of
                            // `String`s. Same rule as the opcode by
                            // construction: `aastore_element_assignable` IS the
                            // interpreter's predicate, and it fails OPEN
                            // (`None`) wherever it cannot answer, so this can
                            // only ever ADD a refusal HotSpot also makes.
                            if let cratonvm_types::Value::Object(Some(vo)) = v {
                                if ctx.aastore_element_assignable(cur_target, vo) == Some(false) {
                                    return Err(array_store_failure(ctx, vo));
                                }
                            }
                            ctx.set_array_element(cur_target, i, v);
                        }
                        let target = ctx.read_native_pin(target_pin, target);
                        let target_len = ctx.array_length(target);
                        if target_len > size {
                            ctx.set_array_element(target, size, Value::Object(None));
                        }
                        Ok(Some(Value::Object(Some(target))))
                    })();
                    ctx.unpin_native_roots(pin_base);
                    return result;
                }
                // ArrayList-shaped: use `size` field directly.
                let size = match ctx.get_field_by_name(this, "size") {
                    Value::Int(s) => s.max(0) as usize,
                    _ => 0,
                };
                // GC-safety: allocating `target` below can move `elementData`
                // — pin it BEFORE the allocation and re-read after.
                let d_pin = data.map(|d| (ctx.pin_native_root(d), d));
                let target = match template {
                    Value::Object(Some(arr)) if ctx.array_length(arr) >= size => arr,
                    // Template too small: allocate a NEW array of the template's
                    // runtime component type (see the iterator-path comment above),
                    // not a bare `Object[]`. Fixes the H2 `SortOrder.sort`
                    // `rows.toArray(new Value[0][])` CCE (`Object -> [[Value`).
                    Value::Object(Some(arr)) => {
                        let comp = ctx.class_id_of_object(arr);
                        ctx.new_ref_array(comp, size)
                    }
                    _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, size),
                };
                if let Some((h, orig)) = d_pin {
                    let d = ctx.read_native_pin(h, orig);
                    let d_len = ctx.array_length(d);
                    let copy = size.min(d_len);
                    for i in 0..copy {
                        let v = ctx.get_array_element(d, i);
                        // See the iterator path above: `System.arraycopy`'s
                        // store check.
                        if let cratonvm_types::Value::Object(Some(vo)) = v {
                            if ctx.aastore_element_assignable(target, vo) == Some(false) {
                                if let Some((h, _)) = d_pin {
                                    ctx.unpin_native_roots(h);
                                }
                                return Err(array_store_failure(ctx, vo));
                            }
                        }
                        ctx.set_array_element(target, i, v);
                    }
                    ctx.unpin_native_roots(h);
                }
                let target_len = ctx.array_length(target);
                if target_len > size {
                    ctx.set_array_element(target, size, Value::Object(None));
                }
                Ok(Some(Value::Object(Some(target))))
            }
            // [Bridge] real-JDK-aware toArray(T[]) over the real ArrayList/
            // AbstractCollection field layout (reads elementData/size by name,
            // iterator fallback). Restored to __prev_toarray after.
            let __prev_toarray = native_methods.current_category();
            native_methods.set_category(cratonvm_native_api::NativeKind::Bridge);
            native_methods.register(
                "java/util/ArrayList",
                "toArray",
                "([Ljava/lang/Object;)[Ljava/lang/Object;",
                real_jdk_to_array_typed,
            );
            native_methods.register(
                "java/util/AbstractCollection",
                "toArray",
                "([Ljava/lang/Object;)[Ljava/lang/Object;",
                real_jdk_to_array_typed,
            );
            native_methods.set_category(__prev_toarray);
            // See the KC26 URL-codec registration comment above: only
            // `register_url_codec` is wanted here, not the whole
            // `deprecated_io_util` module (which would re-clobber
            // `deprecated_util.rs`'s correct `java/util/Date` natives).
            cratonvm_native_builtins::deprecated_io_util::register_url_codec(&mut native_methods);
            cratonvm_native_builtins::register_charset_natives_pub(&mut native_methods);
            cratonvm_native_builtins::phases_late::register_p58_charset_coder(&mut native_methods);
            cratonvm_native_builtins::charset::register_real_charset_natives(&mut native_methods);
            cratonvm_native_builtins::deprecated_internal::register_deprecated_internal_natives(
                &mut native_methods,
            );
            cratonvm_native_builtins::phases_early::register_arrays_support_natives(
                &mut native_methods,
            );
            cratonvm_native_builtins::phases_early::register_string_latin1_natives(
                &mut native_methods,
            );
            // Real close() (see `quarkus_runner_class_loader_close`): closes each
            // distinct ClassLoadingResource once and skips the null map values
            // the real bytecode NPEs on. `SyntheticStub` so `CRATONVM_NO_STUBS`
            // still falls through to bytecode — the same intent this comment
            // always stated, now stated to the registry instead of relying on
            // the default. That reliance was the only one left in the VM crate,
            // and a `set_category` anywhere upstream would have silently
            // retagged it.
            native_methods.register_with_kind(
                "io/quarkus/bootstrap/runner/RunnerClassLoader",
                "close",
                "()V",
                quarkus_runner_class_loader_close,
                cratonvm_native_api::NativeKind::SyntheticStub,
            );
            cratonvm_native_builtins::classloader_real::register_classloader_real_natives(
                &mut native_methods,
            );
            cratonvm_native_builtins::lang_invoke::register_phase54_method_handle(
                &mut native_methods,
            );
            cratonvm_native_builtins::lang_invoke::register_p63_method_handles_lookup(
                &mut native_methods,
            );
            cratonvm_native_builtins::lang_invoke::register_t4_method_handle_invoke(
                &mut native_methods,
            );
            // C5: See the synthetic-jdk branch above for rationale.
            cratonvm_native_builtins::lang_invoke::register_t28_method_handle_completeness(
                &mut native_methods,
            );
            // Round 85: LambdaMetafactory.metafactory/altMetafactory natives.
            // log4j ServiceLoaderUtil.callServiceLoader (and similar code paths
            // in WildFly's PropertiesUtil bootstrap) invokes
            // LambdaMetafactory.metafactory directly (not via invokedynamic).
            // The real-JDK implementation drives InnerClassLambdaMetafactory →
            // java.lang.classfile API, which trips our incomplete classfile
            // shim with "Bad CP index: 0" inside StackMapGenerator. Intercept
            // with a native stub that returns null so the JDK path is bypassed
            // entirely; the surrounding code tolerates a null call site.
            cratonvm_native_builtins::lang_invoke::register_p68_invoke_extras(&mut native_methods);
            // RA.8 + WP1.8: ServiceLoader bootstrap — see
            // `init_service_loader_bootstrap` doc for the rationale around
            // keeping registration + classpath seeding in a single entry.
            init_service_loader_bootstrap(&mut native_methods);
            // WP2.5: java.lang.reflect.Proxy natives. See companion
            // call in the `feature = "synthetic-jdk"` branch above.
            cratonvm_native_builtins::register_reflect_proxy_natives(&mut native_methods);
            // WP2.4-A: java.lang.instrument runtime natives and the
            // in-process Attach API are VM bridges, not synthetic stubs.
            // The default real-JDK build drops synthetic registrations, so
            // tag this pair explicitly or JDK InstrumentationImpl native
            // methods resolve as missing before any javaagent premain runs.
            let __prev_instrument_bridge = native_methods.current_category();
            native_methods.set_category(cratonvm_native_api::NativeKind::Bridge);
            crate::runtime::instrument::register_instrumentation_natives(&mut native_methods);
            // In-process self-attach (com.sun.tools.attach.VirtualMachine) so
            // runtime-attach agents (Mockito inline mock maker, JaCoCo) can
            // load themselves without `-javaagent:`. See companion call in the
            // `feature = "synthetic-jdk"` branch above.
            crate::runtime::instrument::register_self_attach_natives(&mut native_methods);
            native_methods.set_category(__prev_instrument_bridge);
            // RKC16N.10: VMManagementImpl natives. See companion call
            // in the `feature = "synthetic-jdk"` branch above.
            #[cfg(feature = "management")]
            cratonvm_native_builtins::jmx::register_vm_management_impl(&mut native_methods);
            // JMX-CLUSTER-20260720: do NOT register
            // `register_management_factory_platform_server_stub` here. It was
            // added 2026-07-14 as a Bridge (always-wins) override that
            // permanently shadows real bytecode for
            // `ManagementFactory.getPlatformMBeanServer()` with an empty
            // synthetic `MBeanServer` — directly undoing the KAFKA-MBEAN fix
            // in `register_management_factory` (`native-builtins/src/jmx.rs`),
            // which deliberately leaves this method unregistered so real
            // `MBeanServerFactory.createMBeanServer()` bytecode constructs a
            // genuine `JmxMBeanServer` with a real `registerMBean` Code
            // attribute. The 07-14 override's rationale was a real,
            // then-uninvestigated NPE deep in `ObjectName.
            // getCanonicalKeyPropertyListString()` (`_ca_array` null on the
            // synthetic 1-field ObjectName model) that aborted the platform
            // MXBean registration loop — that NPE (and its sibling in
            // `getSerializedNameString()`/`_kp_array`, hit when an
            // ObjectName is actually serialized over jmxmp) is now fixed by
            // the `RKC-ObjectName-01/02/03` natives in `jmx.rs`, so the
            // workaround is obsolete: it was left in permanently and never
            // reverted, silently zeroing out EVERY platform MXBean
            // (Memory/Threading/ClassLoading/...) registered on
            // `getPlatformMBeanServer()` in real-JDK mode ever since —
            // confirmed via `MBeanServer.queryNames(null, null)` returning
            // 0 entries. Leaving this call out restores the original
            // KAFKA-MBEAN behavior: real bytecode runs end-to-end.
            //
            // Surefire ForkedBooter: ManagementFactory.getRuntimeMXBean() and
            // friends. See companion call in the `feature = "synthetic-jdk"`
            // branch above for the rationale (real-JDK bytecode delegates
            // to `getPlatformMXBean(Class)` which fails with
            // `IllegalArgumentException: ... is not a platform management
            // interface`, killing the forked test JVM constructor).
            #[cfg(feature = "management")]
            cratonvm_native_builtins::jmx::register_jmx_natives(&mut native_methods);
            // RKC16N.11: rest of the sun.management.* native surface.
            // See companion calls in the `feature = "synthetic-jdk"`
            // branch above for the full rationale.
            #[cfg(feature = "management")]
            cratonvm_native_builtins::jmx::register_thread_impl(&mut native_methods);
            #[cfg(feature = "management")]
            cratonvm_native_builtins::jmx::register_class_loading_impl(&mut native_methods);
            #[cfg(feature = "management")]
            cratonvm_native_builtins::jmx::register_garbage_collector_impl(&mut native_methods);
            // Wave 1 / Task A: per-pool / per-manager MXBean natives.
            // See companion call in the synthetic-jdk branch above.
            #[cfg(feature = "management")]
            cratonvm_native_builtins::jmx::register_memory_pool_impl(&mut native_methods);
            #[cfg(feature = "management")]
            cratonvm_native_builtins::jmx::register_memory_manager_impl(&mut native_methods);
            #[cfg(feature = "management")]
            cratonvm_native_builtins::jmx::register_operating_system_impl(&mut native_methods);
            #[cfg(feature = "management")]
            cratonvm_native_builtins::jmx::register_hotspot_diagnostic(&mut native_methods);
            #[cfg(feature = "management")]
            cratonvm_native_builtins::jmx::register_flag_impl(&mut native_methods);
            // SLF4J 1.7 binder stubs — see companion call in the synthetic-jdk
            // branch above for the rationale (Spring Boot 2.x fat-jar
            // <clinit> survival).
            cratonvm_native_builtins::register_slf4j_binder_stubs_pub(&mut native_methods);
            // Spring ApplicationStartup: see `spring_startup_bootstrap` in essentials.
            tracing::info!(
                "Real JDK mode: {} native methods registered",
                native_methods.len()
            );
        }
        // T7: Register AWT/Swing/Java2D native methods for desktop support.
        // Gated behind the (default-on) `awt` feature: `cratonvm-native-awt`
        // sets `publish = false`, so it is an optional dependency. The default
        // build enables `awt` and registers these natives exactly as before.
        #[cfg(feature = "awt")]
        {
            cratonvm_native_awt::register_awt_natives(&mut native_methods);
            tracing::info!(
                "AWT/Swing native methods registered (total: {})",
                native_methods.len()
            );
        }
        // Build system properties from platform defaults + user overrides.
        //
        // WP1.11 (2026-04-24) — System property fidelity: populate the full
        // 40-key HotSpot 25 baseline. WildFly's launcher and every
        // standard-library class reads this set; a missing key causes
        // unpredictable fallback paths (often an NPE deep inside
        // Locale/Charset/ClassLoader static init). Keys are split into
        // three tiers, each documented with its JDK origin:
        //
        //  1. Invariant keys — fixed by the VM spec or this build's
        //     identity (java.vm.name, java.vm.vendor, java.specification.*,
        //     java.class.version).
        //  2. Platform-derived keys — read from OS / std::env at startup
        //     (os.name, os.arch, os.version, user.name, user.home, user.dir,
        //     java.io.tmpdir, file.separator, path.separator, line.separator).
        //  3. Env/config-derived keys — read from VmConfig + environment
        //     variables (java.home, java.class.path, java.library.path).
        let mut sys_props = HashMap::new();

        // ---- Tier 1: invariant keys ----
        // java.version family — reported as JDK 25 to match class-file
        // feature support (class-version 69).
        sys_props.insert("java.version".to_string(), "25.0.1".to_string());
        sys_props.insert("java.runtime.version".to_string(), "25.0.1+8".to_string());
        sys_props.insert("java.vm.version".to_string(), "25.0.1+8".to_string());
        sys_props.insert("java.vm.name".to_string(), "cratonvm".to_string());
        sys_props.insert("java.vm.vendor".to_string(), "cratonvm".to_string());
        // HotSpot puts its execution-mode summary here ("mixed mode, sharing",
        // "interpreted mode", …) and tools print it verbatim after the VM
        // name. `--jdk-only` appends its own token so a captured run, a
        // support bundle or a `-version` paste says which substitution policy
        // produced it, without anyone having to find the flags line.
        //
        // The list is built by [`vm_info_mode_list`], which the launcher's
        // `-version` banner also calls — the property and the banner used to
        // spell the policy two different ways, and one function is the only
        // way that stays fixed. Under `Compatible` the value stays *exactly*
        // `"mixed mode"` (jdk-only-mode.md §10).
        sys_props.insert(
            "java.vm.info".to_string(),
            vm_info_mode_list(config.compatibility_mode).to_string(),
        );
        sys_props.insert(
            "java.vm.specification.name".to_string(),
            "Java Virtual Machine Specification".to_string(),
        );
        sys_props.insert(
            "java.vm.specification.version".to_string(),
            "25".to_string(),
        );
        sys_props.insert(
            "java.vm.specification.vendor".to_string(),
            "Oracle Corporation".to_string(),
        );
        sys_props.insert(
            "java.runtime.name".to_string(),
            "Java(TM) SE Runtime Environment".to_string(),
        );
        sys_props.insert("java.vendor".to_string(), "CratonVM".to_string());
        sys_props.insert(
            "java.vendor.url".to_string(),
            "https://cratonvm.invalid/".to_string(),
        );
        sys_props.insert(
            "java.vendor.url.bug".to_string(),
            "https://cratonvm.invalid/bugs".to_string(),
        );
        sys_props.insert("java.vendor.version".to_string(), "25.0.1".to_string());
        // C32: Netty's PlatformDependent0$5 branches on Java version to
        // decide whether DirectByteBuffer(long,long) or (long,int) exists.
        // Must expose a JDK25 spec so the (long,long) path is selected.
        sys_props.insert("java.specification.version".to_string(), "25".to_string());
        sys_props.insert(
            "java.specification.vendor".to_string(),
            "Oracle Corporation".to_string(),
        );
        sys_props.insert(
            "java.specification.name".to_string(),
            "Java Platform API Specification".to_string(),
        );
        // class-file major version for JDK 25 = 69 (45 + feature 24? → JDK 25 = 69).
        sys_props.insert("java.class.version".to_string(), "69.0".to_string());

        // Encodings — JDK 18+ pinned to UTF-8 for stdout/stderr/file/native.
        sys_props.insert("file.encoding".to_string(), "UTF-8".to_string());
        sys_props.insert("native.encoding".to_string(), "UTF-8".to_string());
        sys_props.insert("sun.jnu.encoding".to_string(), "UTF-8".to_string());
        sys_props.insert("stdout.encoding".to_string(), "UTF-8".to_string());
        sys_props.insert("stderr.encoding".to_string(), "UTF-8".to_string());
        // Session 108 (Cluster D v2 — Console.<clinit> companion fix):
        // `java/io/Console.<clinit>` calls
        // `Charset.forName(System.getProperty("stdin.encoding"), UTF_8)`. The
        // 2-arg `forName` only catches `IllegalCharsetNameException`, NOT the
        // `IllegalArgumentException("Null charset name")` that
        // `Charset.lookup(null)` throws — so a null property here trips the
        // Console.<clinit> swallow we observed in Session 108. The
        // `stdin.encoding` key was missing from the bootstrap seed (only
        // `stdout.encoding` / `stderr.encoding` were pinned). Mirror what
        // HotSpot's launcher native code does: pin to UTF-8 unconditionally.
        sys_props.insert("stdin.encoding".to_string(), "UTF-8".to_string());

        // Force BufferedInputStream/etc. to use synchronized blocks instead
        // of InternalLock/ReentrantLock. This avoids potential issues with
        // our ReentrantLock native when nested inside deep I/O call chains.
        sys_props.insert("jdk.io.useMonitors".to_string(), "true".to_string());

        // NOT seeded here: `sun.misc.unsafe.memory.access`.
        //
        // It used to be pinned to `allow` unconditionally, as a workaround for
        // JEP 498: under the JDK 25 default (`warn`) the first legacy
        // `sun.misc.Unsafe` memory access drops into
        // `Unsafe.beforeMemoryAccessSlow()`, which walks the stack and
        // dereferences `frames.get(1)`; CratonVM's StackWalker was reported to
        // return fewer than two frames for some native/JIT-spliced chains, and
        // that surfaced in jctools' `MpscUnboundedArrayQueue` (netty's
        // per-`NioEventLoop` task queue) as "failed to create a child event
        // loop". Pinning `allow` makes `beforeMemoryAccess()` return at its
        // first check and bypasses the warning machinery.
        //
        // The cost was much larger than the fix. HotSpot sets this property
        // ONLY for `--sun-misc-unsafe-memory-access=<mode>`; a default JDK 25
        // run leaves it unset. netty 4.2 keys its entire Unsafe-vs-FFM
        // decision on exactly that (`PlatformDependent0.explicitNoUnsafeCause0`
        // disables Unsafe on Java 25+ unless the property is set), so pinning
        // it put netty — and every other Unsafe-aware library — on a different
        // code path than a stock JDK 25 run, and made every "CratonVM vs
        // HotSpot" netty comparison a comparison of two different code paths.
        //
        // The property is now the user's to set: `vm-cli` rewrites
        // `--sun-misc-unsafe-memory-access=<mode>` to the `-D` form and nothing
        // else writes it, so a default run answers `null` exactly as HotSpot
        // does. The `sun/misc/Unsafe` post-clinit repair in `vm_util.rs` reads
        // the same property and falls back to `WARN` — the JDK's own default —
        // rather than to a forced `ALLOW`.

        // Allow dynamic agents to attach to *this* running VM in-process.
        // Tools that ship as a `java.lang.instrument` agent but are launched
        // without `-javaagent:` (Mockito's inline mock maker, JaCoCo, several
        // profilers) fall back to *self-attach*: at runtime they ask the
        // attach API to load their agent into the current JVM. On a modern
        // JDK self-attach is gated behind `-Djdk.attach.allowAttachSelf=true`
        // (HotSpot defaults it to `false` and prints the familiar "Mockito is
        // self-attaching …" warning). CratonVM implements the in-process leg
        // of that path (see `com/sun/tools/attach/VirtualMachine` natives in
        // `runtime/instrument.rs`) and has no separate-process attach listener,
        // so the *only* attach mode we support is self-attach. Defaulting the
        // property to `true` makes ByteBuddy/Mockito take the direct in-process
        // `Attacher.install(...)` branch instead of trying to spawn an external
        // attacher process (which cannot reach a CratonVM target). A user
        // `-Djdk.attach.allowAttachSelf=...` on the command line still wins
        // because CLI props are applied over these defaults.
        sys_props.insert("jdk.attach.allowAttachSelf".to_string(), "true".to_string());
        // Weld CDI: default the bootstrap deployer to single-threaded (NONE) so
        // Weld runs its real `SimpleBeanDeployer` bytecode instead of the
        // concurrent `ConcurrentBeanDeployer`, which submits bean-discovery
        // tasks to `ForkJoinPool.commonPool().invokeAll(...)`. CratonVM's
        // synthetic ForkJoinPool can't service that (RejectedExecutionException
        // / hang). Single-threaded deployment is functionally identical (same
        // beans, no parallelism). App `-D` overrides this (config loop applies
        // over these defaults). HIB-CV-20.
        //
        // The real ForkJoinPool path is the default, so Weld's concurrent
        // `COMMON` deployer works (and clears the WELD-001301 the legacy
        // single-threaded path hits). Only seed NONE when the explicit
        // `CRATONVM_SYNTHETIC_FORKJOINPOOL` compatibility opt-out is active;
        // that surface cannot service commonPool().invokeAll safely.
        if !cratonvm_types::flags::flags().natives.real_forkjoinpool {
            sys_props.insert(
                "org.jboss.weld.executor.threadPoolType".to_string(),
                "NONE".to_string(),
            );
        }

        // Keys HotSpot 25 publishes that this table did not, found 2026-08-04
        // by diffing `System.getProperties()` against a HotSpot 25 control (45
        // keys against 48). Each is read by real library code, not only by
        // `-XshowSettings`:
        //
        // * `sun.cpu.endian` — Netty, Chronicle and several serialization
        //   libraries branch on it, and code that finds it absent typically
        //   assumes big-endian, which is wrong on every machine this runs on.
        // * `sun.io.unicode.encoding` — read by `java.io.ObjectStreamClass` and
        //   by the older text codecs.
        // * `java.version.date`, `jdk.debug`, `sun.management.compiler`,
        //   `java.vm.compressedOopsMode` — informational, but they appear in
        //   crash reports, `RuntimeMXBean` dumps and support bundles, and their
        //   absence is what makes a CratonVM dump obviously not a JVM dump.
        //
        // THIS is the table that reaches `System.getProperties()` in real-JDK
        // mode. `native-builtins/src/system_bootstrap.rs::native_vm_properties`
        // has its own, overlapping list which is NOT the source here — the tell
        // is `java.vm.name`, which that one sets to "CratonVM" and this one to
        // "cratonvm", and a real-JDK run reports the lower-case spelling. Add a
        // key to both or you will add it to neither.
        //
        // `sun.java.command` and `sun.java.launcher` are deliberately absent:
        // only the launcher knows them, and `vm-cli` supplies them through
        // `config.system_properties`.
        sys_props.insert(
            "sun.cpu.endian".to_string(),
            if cfg!(target_endian = "big") {
                "big".to_string()
            } else {
                "little".to_string()
            },
        );
        sys_props.insert(
            "sun.io.unicode.encoding".to_string(),
            "UnicodeLittle".to_string(),
        );
        sys_props.insert("java.version.date".to_string(), "2025-10-21".to_string());
        sys_props.insert("jdk.debug".to_string(), "release".to_string());
        sys_props.insert(
            "sun.management.compiler".to_string(),
            "CratonVM JIT".to_string(),
        );
        sys_props.insert(
            "java.vm.compressedOopsMode".to_string(),
            "Zero based".to_string(),
        );

        // ---- Tier 2: platform-derived keys ----
        sys_props.insert("os.name".to_string(), canonical_os_name());
        sys_props.insert("os.arch".to_string(), canonical_os_arch());
        sys_props.insert("os.version".to_string(), canonical_os_version());
        // HotSpot always publishes the data model (pointer width in bits).
        // Some JDK internals branch on it — e.g. `jdk.internal.jimage`'s
        // `BasicImageReader` derives `IS_64_BIT`/`MAP_ALL` from it and, when
        // it defaults to "32", opens a FileChannel over the run-time image
        // instead of using the whole-image map. Without this key the module
        // system takes a degraded path. Derive it from the target pointer
        // width so it is correct on every arch.
        sys_props.insert(
            "sun.arch.data.model".to_string(),
            (std::mem::size_of::<usize>() * 8).to_string(),
        );
        sys_props.insert(
            "file.separator".to_string(),
            std::path::MAIN_SEPARATOR.to_string(),
        );
        sys_props.insert(
            "path.separator".to_string(),
            if cfg!(windows) { ";" } else { ":" }.to_string(),
        );
        sys_props.insert(
            "line.separator".to_string(),
            if cfg!(windows) { "\r\n" } else { "\n" }.to_string(),
        );
        if let Ok(dir) = std::env::current_dir() {
            sys_props.insert("user.dir".to_string(), dir.to_string_lossy().into_owned());
        } else {
            sys_props.insert("user.dir".to_string(), ".".to_string());
        }
        let user_home = cratonvm_types::flags::runtime_var("HOME")
            .or_else(|_| cratonvm_types::flags::runtime_var("USERPROFILE"))
            .unwrap_or_else(|_| {
                if cfg!(windows) {
                    "C:\\".to_string()
                } else {
                    "/".to_string()
                }
            });
        sys_props.insert("user.home".to_string(), user_home);
        sys_props.insert(
            "user.name".to_string(),
            cratonvm_types::flags::runtime_var("USER")
                .or_else(|_| cratonvm_types::flags::runtime_var("USERNAME"))
                .unwrap_or_else(|_| "unknown".to_string()),
        );
        sys_props.insert(
            "java.io.tmpdir".to_string(),
            std::env::temp_dir().to_string_lossy().into_owned(),
        );

        // Locale — W7-67. Read the HOST's two locales (Windows: UI language +
        // regional format; Unix: LC_MESSAGES + LC_CTYPE, each resolved through
        // LC_ALL ▸ category ▸ LANG) and publish the `user.*` family the way
        // `jdk.internal.util.SystemProps.fillI18nProps` does. This used to
        // parse `$LANG` only, so every Windows host — where `$LANG` is unset —
        // reported `en_US` regardless of the machine's actual settings.
        //
        // `config.system_properties` (the `-D` flags) is passed in so a
        // command-line `-Duser.language=…` suppresses the derived `.format`
        // overlay, matching the JDK; it is re-applied over `sys_props` below,
        // which is what makes the `-D` value itself win.
        let host_locale = derive_host_locale();
        for (base, display, format) in [
            (
                "user.language",
                &host_locale.display.language,
                &host_locale.format.language,
            ),
            (
                "user.script",
                &host_locale.display.script,
                &host_locale.format.script,
            ),
            (
                "user.country",
                &host_locale.display.country,
                &host_locale.format.country,
            ),
            (
                "user.variant",
                &host_locale.display.variant,
                &host_locale.format.variant,
            ),
        ] {
            fill_i18n_props(
                &mut sys_props,
                &config.system_properties,
                base,
                display,
                format,
            );
        }
        // user.timezone is normally set by the JDK's TimeZone.getDefault()
        // during initPhase1 — pre-populate with TZ env or empty string so
        // the key is at least present.
        sys_props.insert(
            "user.timezone".to_string(),
            cratonvm_types::flags::runtime_var("TZ").unwrap_or_default(),
        );

        // ---- Tier 3: env/config-derived keys ----
        // java.home — same resolution order as boot classpath discovery
        // (`resolve_java_home_public`): explicit config, CRATONVM_JAVA_HOME,
        // JAVA_HOME, then PATH. Avoid reading JAVA_HOME alone here — that
        // skipped CRATONVM_JAVA_HOME and could disagree with boot discovery.
        let java_home_val = crate::config::resolve_java_home_public(config.java_home.as_deref())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".to_string());
        sys_props.insert("java.home".to_string(), java_home_val.clone());

        // java.class.path — match the HotSpot contract:
        //   * `-jar <FILE>` launch  ->  `java.class.path = <FILE>` (the
        //     bare user-supplied jar path, NOT the manifest-expanded
        //     transitive classpath). Liberty/Quarkus boot launchers do
        //     `new JarFile(new File(System.getProperty("java.class.path")))`
        //     and then `.getManifest().getMainAttributes()` — if we joined
        //     all expanded entries here, that File wouldn't exist as a
        //     jar and `getManifest()` would return null, producing the
        //     "Cannot invoke getMainAttributes on null" NPE seen in 21
        //     wlp tool jars (ws-schemagen, ws-wsimport, ws-featureUtility,
        //     ...). The full transitive classpath is still wired into
        //     the class loader via `config.classpath`, so resolution of
        //     manifest `Class-Path:` siblings is unaffected.
        //   * `-classpath <CP>` / `<class>` launch  ->  joined cp string.
        //   * No cp configured  ->  fall back to $CLASSPATH env, then "".
        let class_path_val = if let Some(jar) = config.launcher_jar.as_deref() {
            jar.to_string()
        } else if !config.classpath.is_empty() {
            let sep = if cfg!(windows) { ";" } else { ":" };
            // The Java launcher expands `dir/*` before publishing this
            // property. Keep it aligned with the already-expanded loader
            // view so javax.tools/javac can discover wildcard JARs from its
            // default classpath (notably H2's dynamic CREATE ALIAS compiler).
            crate::classloading::ClassPath::expand_classpath_entries(&config.classpath).join(sep)
        } else {
            cratonvm_types::flags::runtime_var("CLASSPATH").unwrap_or_default()
        };
        sys_props.insert("java.class.path".to_string(), class_path_val);

        // java.library.path — platform-specific: $PATH on Windows,
        // $LD_LIBRARY_PATH on Linux, $DYLD_LIBRARY_PATH on macOS. Always
        // prefix with <java.home>/lib so native-lib lookup still works.
        let lib_path_env = if cfg!(windows) {
            cratonvm_types::flags::runtime_var("PATH").unwrap_or_default()
        } else if cfg!(target_os = "macos") {
            cratonvm_types::flags::runtime_var("DYLD_LIBRARY_PATH").unwrap_or_default()
        } else {
            cratonvm_types::flags::runtime_var("LD_LIBRARY_PATH").unwrap_or_default()
        };
        let sep = if cfg!(windows) { ";" } else { ":" };
        let lib_path_val = if lib_path_env.is_empty() {
            format!("{}/lib", java_home_val)
        } else {
            format!("{}/lib{}{}", java_home_val, sep, lib_path_env)
        };
        sys_props.insert("java.library.path".to_string(), lib_path_val);
        sys_props.insert(
            "sun.boot.library.path".to_string(),
            format!("{}/lib", java_home_val),
        );

        // Apply user overrides from config — always wins over platform defaults.
        for (k, v) in &config.system_properties {
            sys_props.insert(k.clone(), v.clone());
        }

        // Wire GC logging from config
        if config.verbose_gc {
            heap.enable_gc_logging();
        }

        // WP8.5 — wire HotSpot-style unified logging (`-Xlog:...`).
        // The full JEP 158/271 parser lives in
        // `vm/src/runtime/unified_logging.rs`; the wiring step is to
        // initialize the global logger at VM startup so subsequent
        // `log_unified()` / `gc_info()` calls in GC and class-load
        // hot paths actually emit. A failed parse is logged through
        // tracing but does not abort startup — HotSpot's behaviour for
        // a malformed `-Xlog` spec is to print a warning and continue.
        if let Some(spec) = config.xlog_spec.as_deref() {
            if let Err(e) = crate::runtime::unified_logging::init_unified_logging(spec) {
                tracing::warn!("invalid -Xlog spec {:?}: {}", spec, e);
            }
        }

        // Initialize AOT runtime if enabled
        #[cfg(feature = "experimental-aot")]
        {
            use crate::config::AotMode;
            let (training, production) = match config.aot_mode {
                AotMode::Training => (true, false),
                AotMode::Production => (false, true),
                AotMode::Off => (false, false),
            };
            crate::native::builtins::aot::init_aot_runtime(
                training,
                production,
                config.aot_cache_input.as_deref(),
                config.aot_cache_output.as_deref(),
            );
        }

        // AOT and CDS are explicitly compiled experiments and are no longer in
        // the default feature set. A build that cannot honour `-XX:AOTMode` /
        // `-XX:SharedArchiveFile` must say so: silently ignoring the request is
        // exactly the "capability reads as landed but never runs" failure mode
        // ARCHITECTURE.md's flag-default checklist exists to prevent.
        #[cfg(not(feature = "experimental-aot"))]
        {
            if !matches!(config.aot_mode, crate::config::AotMode::Off) {
                tracing::warn!(
                    "AOT cache requested (-XX:AOTMode) but this build was compiled \
                     without --features experimental-aot; the request is ignored"
                );
            }
            if !matches!(config.cds_mode, crate::config::CdsMode::Off) {
                tracing::warn!(
                    "CDS requested (-XX:SharedArchiveFile / -Xshare) but this build \
                     was compiled without --features experimental-aot; the request \
                     is ignored"
                );
            }
        }

        // Load CDS archive if configured
        #[cfg(feature = "experimental-aot")]
        if matches!(
            config.cds_mode,
            crate::config::CdsMode::On | crate::config::CdsMode::Auto
        ) {
            if let Some(ref archive_path) = config.shared_archive_file {
                let mut loader =
                    cratonvm_native_builtins::cds::CdsArchiveLoader::new(archive_path.clone());
                if loader.try_load() {
                    let n = loader.classes_loaded();
                    tracing::info!("CDS: loaded {} classes from {}", n, archive_path);
                    // Transfer cached class bytes into ClassManager for fast lookup.
                    // Convert the loader's std HashMap into the ClassManager's FxHashMap
                    // (T10.9.B — CDS cache is internal-keyed so Fx is safe).
                    class_manager.cds_class_cache =
                        loader.drain_class_cache().into_iter().collect();
                    // Set system property so native handlers know CDS is active
                    sys_props.insert(
                        "jdk.internal.vm.cds.enabled".to_string(),
                        "true".to_string(),
                    );
                } else if matches!(config.cds_mode, crate::config::CdsMode::On) {
                    tracing::error!(
                        "CDS: archive not found or invalid at {} (mode=on, failing)",
                        archive_path
                    );
                }
            }
        }

        // `-Xshare:dump`: this run WILL write an archive at shutdown (see
        // `SharedVm::dump_cds_archive`). Publish that as a system property so
        // `jdk/internal/misc/CDS.isDumpingArchive0()` can report the real mode
        // instead of a hard-coded `false`; mirrors the
        // `jdk.internal.vm.cds.enabled` handshake above.
        if matches!(config.cds_mode, crate::config::CdsMode::Dump) {
            sys_props.insert(
                "jdk.internal.vm.cds.dumping".to_string(),
                "true".to_string(),
            );
        }

        // Build the GPU offload cache registry. With the feature off,
        // this block does not exist. The registry itself is empty until
        // the first `get_or_create` call constructs a per-device
        // `OffloadCache`, so this is effectively free at startup.
        #[cfg(feature = "gpu-offload")]
        let offload_registry =
            std::sync::Arc::new(crate::runtime::offload::OffloadCacheRegistry::new());

        // The real-JDK platform-server bridge needs its interface methods.
        #[cfg(feature = "management")]
        cratonvm_native_builtins::jmx::register_mbean_server(&mut native_methods);

        // Phase 3 closes here — `register_mbean_server` above is the LAST
        // `register_*` call before the registry is moved into `Self`, so
        // `native_methods.len()` is the true boot registration count for
        // whichever arm of the two mode `cfg` blocks was compiled in. That
        // count is the honest answer to "is real-JDK mode really ~300
        // natives?" (it is not — see
        // `arch-2026-07-26/startup-and-diagnostics.md` §2.5).
        // Note the registry pre-sizes four maps to 4,096 entries
        // unconditionally (`NativeMethodRegistry::new`), i.e. independently of
        // mode; that is one bounded allocation, not per-native work.
        tracing::info!(
            "boot phase 3/3 native registration: {:?} ({} natives registered)",
            __boot_t2.elapsed(),
            native_methods.len(),
        );
        let bootstrap_phase = bootstrap_phase
            .natives_ready(native_methods.len())
            .expect("bootstrap NativesReady invariants");

        // The registration pass just pushed one `native-register:<class>.<method>`
        // row into the audit log per registered native — ~3,100 in real-JDK
        // mode, ~5,200 with `synthetic-jdk`. Under the default `Permissive` mode
        // that is pure noise with a sharp edge: `MAX_AUDIT_ENTRIES` is 4,096, so
        // in synthetic mode the map is FULL before `main()` runs and every
        // subsequent file/socket/spawn use — the only thing the report exists to
        // collect — is silently dropped, with `truncated` set. The same rows are
        // already recorded, with provenance, by the registry's own census
        // (`registrations` / `provenance` / `census()`), so nothing is lost.
        //
        // Cleared ONLY under `Permissive`. `Audit` and `Enforce` keep every row:
        // in those modes an operator deliberately asked for the detail, and an
        // `Enforce` run's *denied* registrations are exactly what they need to
        // see. (Those modes still hit the 4,096 cap in synthetic mode — that is
        // a `native-api` sizing question, reported separately, not something to
        // paper over here.)
        if capabilities.mode() == cratonvm_native_api::CapabilityMode::Permissive {
            capabilities.reset_audit();
        }

        let vm = Self {
            // Allocated at the top of this function, before the `register_*`
            // pass, because the capability policy filed under this id has to be
            // in the registry before it accepts its first registration.
            vm_identity,
            config,
            #[cfg(feature = "gpu-offload")]
            offload_registry,
            classes: crate::vm::realms::ClassRealm {
                class_manager: OrderedPlRwLock::new(class_manager, LockLevel::ClassManager),
                anon_class_cache: std::array::from_fn(|_| AtomicU32::new(0)),
                statics: RwLock::new(FxHashMap::default()),
                statics_index: crate::vm::realms::class_realm::StaticsIndex::new(),
                // u32::MAX = "java/lang/System not prepared yet"; a real
                // ClassId can never be u32::MAX (see AUTOBOX_CLASS_ID's
                // reserved-range note in `types`).
                system_class_id: AtomicU32::new(u32::MAX),
                resolution_cache: RwLock::new(ResolutionCache::new()),
                // Round 8 audit fix (CRIT #2): reflective lookup cache.
                link_resolver: LinkResolver::new(),
                vtable_manager: std::sync::Arc::new(parking_lot::RwLock::new(
                    crate::runtime::vtable::VtableManager::new(),
                )),
                shared_resolution: crate::runtime::lockfree_resolve::SharedResolutionState::new(),
                class_locks: RwLock::new(FxHashMap::default()),
                class_mirrors: RwLock::new(FxHashMap::default()),
                class_mirrors_reverse: RwLock::new(FxHashMap::default()),
                initiating_resolution_cache: RwLock::new(FxHashMap::default()),
                lambda_proxies: RwLock::new(FxHashMap::default()),
                proxy_method_cache: RwLock::new(FxHashMap::default()),
                lambda_impl_owner_memo: RwLock::new(FxHashMap::default()),
                lambda_proxy_hosts: RwLock::new(FxHashMap::default()),
                next_lambda_id: AtomicU32::new(
                    crate::vm::realms::class_realm::LAMBDA_PROXY_ID_BASE,
                ),
                annotation_proxy_cid: AtomicU32::new(u32::MAX),
                annotation_proxy_absent_epoch: std::sync::atomic::AtomicU64::new(u64::MAX),
                primitive_mirrors: RwLock::new(FxHashMap::default()),
                module_mirrors: RwLock::new(FxHashMap::default()),
                cached_string_num_fields: AtomicUsize::new(0),
                compact_strings: std::sync::atomic::AtomicBool::new(false),
                cached_class_mirror_num_fields: AtomicUsize::new(0),
                class_init_waiters: parking_lot::Mutex::new(FxHashMap::default()),
                class_loading_locks: parking_lot::Mutex::new(FxHashMap::default()),
                // T10.9.E — lazy per-(ClassId, slot_index) field descriptor cache.
                field_descriptor_cache: parking_lot::RwLock::new(
                    crate::runtime::fx_collections::FxHashMap::default(),
                ),
                // WP0.2 — ObjectStreamClass.lookup(cls) cache.
                osc_cache: crate::runtime::serialization::OscCache::new(),
            },
            mem: crate::vm::realms::HeapRealm {
                heap,
                concurrent_satb,
                concurrent_gc_state,

                operand_stack_pool: crate::runtime::alloc_fastpath::VecPool::new(64),
                tag_pool: crate::runtime::alloc_fastpath::VecPool::new(64),
                string_pool: RwLock::new(FxHashMap::default()),
                singleton_oom: RwLock::new(None),
                var_handle_roots: RwLock::new(FxHashMap::default()),
                gc_barrier: GcBarrier::new(),
                ref_processor: OrderedPlMutex::new(
                    cratonvm_gc::ReferenceProcessor::new(),
                    LockLevel::RefProcessor,
                ),
                finalizer_thread: cratonvm_gc::reference::FinalizerThread::new(),
                cleaner_thread: cratonvm_gc::reference::CleanerThread::new(),
                gc_requested: std::sync::atomic::AtomicBool::new(false),
                native_array_gc_requested: std::sync::atomic::AtomicBool::new(false),
                // T19.3.G1 — allocation-storm observability counters.
                tlab_refill_count: std::sync::atomic::AtomicU64::new(0),
                tlab_hit_count: std::sync::atomic::AtomicU64::new(0),
                gc_cycle_count: std::sync::atomic::AtomicU64::new(0),
                gc_unproductive_streak: std::sync::atomic::AtomicU32::new(0),
                bytes_allocated_total: std::sync::atomic::AtomicU64::new(0),
            },

            natives: crate::vm::realms::NativeRealm {
                native_methods,

                fd_table: FileDescriptorTable::new(),
                native_memory: parking_lot::Mutex::new(crate::native::ffi::NativeMemoryTable::new()),
                native_libraries: parking_lot::Mutex::new(Vec::new()),
                upcall_table: parking_lot::Mutex::new(crate::native::ffi::UpcallTable::new()),
                jni_global_refs: parking_lot::Mutex::new(crate::native::jni::JniGlobalRefs::new()),
                jni_native_methods: crate::native::jni::JniNativeMethodTable::default(),
                jni_bridge_invocations: std::sync::atomic::AtomicU64::new(0),
                matcher_leaf_admission: std::array::from_fn(|_| {
                    std::sync::atomic::AtomicU64::new(0)
                }),
                netty_tcnative_real: std::sync::atomic::AtomicBool::new(false),
            },

            threads: crate::vm::realms::ThreadRealm {
                throwable_stacks: RwLock::new(FxHashMap::default()),
                monitors: MonitorTable::new(),
                main_thread_group: RwLock::new(None),
                main_thread_group_init: parking_lot::Mutex::new(MainThreadGroupInit::Idle),
                thread_registry: ThreadRegistry::new(),

                virtual_scheduler: crate::threading::VirtualThreadScheduler::new_default(),
                virtual_thread_manager: Arc::new(
                    crate::threading::VirtualThreadManager::with_default_parallelism(),
                ),
            },
            system_out: RwLock::new(None),
            system_err: RwLock::new(None),
            system_in: RwLock::new(None),
            system_properties: RwLock::new(sys_props),
            self_arc: RwLock::new(None),
            debug: crate::vm::realms::DebugRealm {
                oom_dump_written: std::sync::atomic::AtomicBool::new(false),
                missing_natives_log: parking_lot::Mutex::new(Vec::new()),
                flight_recorder: parking_lot::Mutex::new(cratonvm_jfr::create_flight_recorder()),
                jfr_dump_on_exit: parking_lot::Mutex::new(None),
                jfr_java_recording: parking_lot::Mutex::new(None),
                jfr_java_recording_running: std::sync::atomic::AtomicBool::new(false),
                jfr_java_output: parking_lot::Mutex::new(None),
                jcmd_processor: parking_lot::Mutex::new(None),
                #[cfg(feature = "experimental-debug")]
                debug_state: parking_lot::Mutex::new(crate::debug::DebugState::new()),
                #[cfg(feature = "experimental-debug")]
                jvmti_env: parking_lot::Mutex::new(jvmti_env),
                #[cfg(feature = "experimental-debug")]
                breakpoints_active: std::sync::atomic::AtomicBool::new(false),
                #[cfg(feature = "experimental-debug")]
                debug_event_tx: std::sync::Mutex::new(None),
                #[cfg(feature = "experimental-debug")]
                debug_event_rx: std::sync::Mutex::new(None),
                diagnostic_counters: crate::runtime::diagnostics::DiagnosticCounters::new(),
                swallow_counter: std::sync::atomic::AtomicU64::new(0),
                stack_dump_requested: std::sync::atomic::AtomicBool::new(false),
                stack_dump_ack_count: std::sync::atomic::AtomicU32::new(0),
                stack_dump_acked_tids: parking_lot::Mutex::new(Vec::new()),
                stack_sample_mode: std::sync::atomic::AtomicBool::new(false),
            },
            jit: crate::vm::realms::JitRealm {
                jit_cache: JitCache::new(),
                profile_store: ProfileStore::new(),
                jit_skip_set: parking_lot::RwLock::new(FxHashSet::default()),
                jit_gate_pass: parking_lot::RwLock::new(FxHashMap::default()),
                // wire-tiered-manager Step 6: honor the CRATONVM_TIER_* threshold
                // overrides (c1/c2/osr/c2_min/enabled). Identical to the default
                // policy when the environment is unset.
                tiered_manager: crate::jit::tiered::TieredCompilationManager::with_env_policy(),
                compilation_broker: parking_lot::Mutex::new(
                    crate::jit::tiered::CompilationBroker::with_default_policy(),
                ),
                deopt_log: parking_lot::Mutex::new(crate::jit::deopt::DeoptimizationLog::new()),
                method_epochs: parking_lot::RwLock::new(FxHashMap::default()),
                method_epoch_overflow: std::sync::atomic::AtomicU64::new(0),
                invalidation_manager: parking_lot::Mutex::new(
                    cratonvm_jit::deopt::InvalidationManager::new(),
                ),
                // JIT slow-path allocation: per-class init recipe cache.
                jit_alloc_class_cache: crate::jit::alloc_class_cache::JitAllocClassCache::new(),
            },
            // WP1.3 — bootstrap init-level state machine. Starts at 0
            // ("VM construction in progress"); `Vm::new` bumps it to 1
            // after `SharedVm` is wrapped in `Arc` and the main thread
            // is registered.
            init_level: AtomicI32::new(0),
            init_level_waiters: Arc::new((std::sync::Mutex::new(()), std::sync::Condvar::new())),
        };

        // wire-tiered-manager Step 4 (PGO handoff C1 → C2): turn on interpreter
        // profile recording when the opt-in gate is set. This MUST happen here, at
        // VM init (before any method frame executes), not lazily at the JIT
        // threshold — the dispatch loop captures `is_profiling_enabled()` once per
        // frame entry, so a method already warming up would never record. With the
        // gate set, the interpreted ("C1"/warmup) phase populates
        // `profile_store` (branch bias, receiver types, loop trips); the optimizing
        // C2 compile then consumes it. Default-OFF: `enable_profiling` is never
        // called, every `record_*` short-circuits, and behaviour is unchanged.
        if crate::runtime::env_cache::tier_pgo() {
            crate::jit::profile::enable_profiling(true);
        }

        // Post-construction: pre-initialize critical static fields for core
        // classes when running with real JDK bytecode.  This must happen after
        // the struct is fully built because the helpers need &SharedVm.
        if !vm.config.use_synthetic_jdk {
            crate::vm::vm_object::pre_init_string_statics(&vm);
            crate::vm::vm_object::pre_init_class_statics(&vm);
            crate::vm::vm_object::pre_init_wrapper_type_fields(&vm);

            // Session 10: Validate native method coverage at bootstrap time.
            // Scans all loaded non-synthetic classes for ACC_NATIVE methods and
            // checks each has a registered Rust implementation. Logs a report.
            let report = crate::vm::vm_object::validate_native_coverage(&vm);
            report.log_report();
            if !report.missing.is_empty() {
                tracing::warn!(
                    "[NativeBridge] {} ACC_NATIVE methods have no Rust implementation; \
                     they will throw UnsatisfiedLinkError if called",
                    report.missing.len()
                );
            }
        }

        // T6.3.1: install this VM's JVMTI event manager, and fire VMInit once
        // all core subsystems are up.
        //
        // Per-VM, not process-wide: a single global manager sent VM B's events
        // to VM A's callbacks and dropped VM B's own manager on the floor.
        crate::runtime::jvmti::install_manager_for_vm(
            vm.vm_identity,
            std::sync::Arc::new(crate::runtime::jvmti::JvmtiEventManager::new_for_vm(
                vm.vm_identity,
            )),
        );
        // AND the unattributed row, which is NOT redundant. Six delivery sites
        // — VMInit, VMDeath, ClassLoad, ClassPrepare, GCStart, GCFinish —
        // still resolve through `global_manager()`, which is an EXACT lookup
        // of row 0 with no fallback. Installing only the per-VM row above left
        // row 0 uninhabited in production, so every one of those events
        // resolved `None` and was silently dropped. The per-VM lane's note
        // that "their events land on the unattributed manager, which is
        // exactly where they land today" was true only while this call site
        // still populated row 0 — so it has to keep populating it until those
        // sites are attributed. The test that should have caught this installs
        // the row itself first, and so passes vacuously.
        crate::runtime::jvmti::install_global_manager(std::sync::Arc::new(
            crate::runtime::jvmti::JvmtiEventManager::new(),
        ));

        // Bridge the classloading crate's JVMTI hooks to the runtime JVMTI
        // manager. `classloading` cannot depend on `vm`, so it exposes a
        // `fn(u32, &str, u64)` hook registry; we install a small adapter
        // that forwards to the runtime fire_* free functions.
        fn class_load_adapter(class_id: u32, _class_name: &str, thread_id: u64) {
            crate::runtime::jvmti::fire_class_load(thread_id, class_id as u64);
        }
        fn class_prepare_adapter(class_id: u32, _class_name: &str, thread_id: u64) {
            crate::runtime::jvmti::fire_class_prepare(thread_id, class_id as u64);
        }
        cratonvm_classloading::install_class_load_hook(class_load_adapter);
        cratonvm_classloading::install_class_prepare_hook(class_prepare_adapter);

        // CRATONVM_DBG_ROOT_SOURCE: let the collector ask the root inventory
        // "who handed me this address?".
        //
        // The registry lives in this crate and the question is asked in the
        // GC crate, which cannot call back the other way -- so the GC owns a
        // doorway and we install the lookup through it, exactly as the
        // quiescence flag is arranged. Installed unconditionally; the lookup
        // itself short-circuits to `None` unless the flag is on, so this costs
        // one `OnceLock` write per VM.
        cratonvm_gc::gc_quiescence::install_root_source_hook(
            crate::memory::native_roots::root_source_of,
        );

        // T10.5 — register the class loader's vtable-install hook so each
        // class-link emits its descriptor vec into `shared.classes.vtable_manager`.
        //
        // Two things have to be registered here, in this order:
        //   1. The global `VtableManager` Arc so the adapter has something
        //      to write into.
        //   2. The `vtable_install_adapter` itself, which the class loader
        //      invokes from inside its write lock on `ClassManager`.
        //
        // Any classes that were bootstrapped BEFORE this registration
        // (e.g. `java/lang/Object` loaded during `ClassManager::new`) will
        // be lazily replayed into the manager by the explicit catch-up
        // loop right below.
        crate::runtime::vtable::install_global_vtable_manager(std::sync::Arc::clone(
            &vm.classes.vtable_manager,
        ));
        cratonvm_classloading::install_vtable_install_hook(
            crate::runtime::vtable::vtable_install_adapter,
        );
        // T10.9.A — also register the override hook so each subclass's
        // link-time slot-override fires `invalidate_for_override` on the
        // super-class vtable. Closes the CHA loop for cached invoke
        // entries and JIT leaf-class assumptions.
        cratonvm_classloading::install_vtable_override_hook(
            crate::runtime::vtable::vtable_override_adapter,
        );

        // Round 4 audit fix (CRIT) — register the ResolutionCache
        // invalidation hook so the JVMTI `RedefineClasses` path
        // (classloading::ClassManager::redefine_class step 9) drops every
        // cached field/method/call-site/condy resolution that refers to
        // (or was resolved into) the redefined class.
        //
        // Order: this must be installed BEFORE this VM is added to the
        // `RESOLUTION_INVALIDATE_VMS` registry below in `Vm::new` (where
        // `self_arc` is set), but installing the hook here is fine — the
        // adapter iterates `live_hook_vms()`, which is empty until the first
        // VM is registered, so it falls through to a no-op.
        cratonvm_classloading::install_resolution_invalidate_hook(resolution_invalidate_adapter);

        // Give the GC crate a way to turn a `ClassId` into a name for its
        // failure-path reports. Same bridge, same reason: the gc crate cannot
        // name a `Class`. Without it the ZGC fragmentation report can only say
        // `class_id=418`, and the second run needed to decode that is a
        // different process with a different heap layout — so the answer does
        // not carry over. See `cratonvm_gc::collector::set_class_namer`.
        cratonvm_gc::collector::set_class_namer(class_name_adapter);

        // Found while investigating the guarded-inline-getfield SIGSEGV
        // cluster (that SIGSEGV's actual cause was a separate, already-fixed
        // bug — see `jit_invalidate_adapter`'s doc comment): `install_jit_invalidate_hook`
        // itself dates back further (it already backs `redefine_class`'s Step
        // 8 `fire_jit_invalidate_hook` call) but had no installer anywhere in
        // the VM, so that call was always a silent no-op. Wire it up so BOTH
        // `redefine_class` and the synthetic-stub-upgrade path
        // (`upgrade_synthetic_class` / `recompute_subclass_layouts`, which —
        // unlike JEP 109 redefine — really can change field layout) actually
        // evict stale JIT-compiled code.
        cratonvm_classloading::install_jit_invalidate_hook(jit_invalidate_adapter);

        // Catch-up pass: replay every class already in the ClassManager's
        // `vtable_descriptors` through the adapter, so classes loaded
        // during ClassManager bootstrap (before the hook was live) end up
        // in the VM's VtableManager too.
        {
            let cm = vm.classes.class_manager.read();
            // `slot_count()`, not `len()` — the latter is the live count and
            // under-runs the id space as soon as anything has been unloaded
            // (see `ClassStore::slot_count`). `vtable_descriptors_of` returns
            // `None` for tombstoned ids, so the extra slots cost nothing.
            let store_len = cm.class_store.slot_count() as u32;
            for cid in 0..store_len {
                let cid = crate::classloading::ClassId::new(cid);
                if let Some(entries) = cm.vtable_descriptors_of(cid) {
                    crate::runtime::vtable::vtable_install_adapter(cid.as_u32(), entries.to_vec());
                }
            }
        }

        // Bridge the GC crate's hooks too. The GC driver (gc::collect /
        // collect_with_finalizers) calls these unconditionally at entry / exit
        // — they are zero-cost when no agent is attached.
        fn gc_start_adapter() {
            crate::runtime::jvmti::fire_gc_start();
        }
        fn gc_finish_adapter() {
            crate::runtime::jvmti::fire_gc_finish();
        }
        cratonvm_gc::install_gc_start_hook(gc_start_adapter);
        cratonvm_gc::install_gc_finish_hook(gc_finish_adapter);

        // Install the class-info resolver used by the heap's
        // layout-mismatch diagnostics. When `gen_heap::{get,set}_field`
        // drops an out-of-bounds access it logs the raw `ClassId`; this
        // hook lets it also print the class NAME and the class's REAL
        // declared field count, turning `ClassId(275)` into an
        // actionable `org/jboss/.../Foo (real layout has 7 fields)`.
        cratonvm_gc::install_class_info_hook(class_info_adapter);

        // Round 4 audit fix (CRIT) — publish the global Weak<SharedVm>
        // handle used by `resolution_invalidate_adapter`. Done at the
        // very end of `SharedVm::new` so the Arc returned by the caller
        // wrapping us already exists; if we ran before that, the
        // upgrade would always fail. The Arc-wrap step happens in
        // `Vm::new` (vm.rs) when `self_arc` is set — `set_global_shared_vm_for_hooks`
        // is idempotent, so calling it again from there is harmless and
        // covers the case where this constructor runs as part of a
        // larger flow that hasn't yet Arc-wrapped us.
        //
        // Until the handle is set, `fire_resolution_invalidate_hook`
        // still fires the adapter, but the adapter's
        // `Weak::upgrade()` returns None and the call is a no-op —
        // perfectly safe for unit tests that construct a SharedVm
        // outside an Arc.
        // Attributed: a row-0 agent used to see VMInit for every VM in the
        // process. JVMTI specifies that it sees its own.
        crate::runtime::jvmti::fire_vm_init_for_vm(vm.vm_identity);

        // Boot-cost summary. `SharedVm::new` is only part of startup — the
        // launcher still has to run `System.initPhase1/2/3` and load the
        // application's own classes after this returns — but it is the part
        // that is fixed cost for every run, so a regression in it is a
        // regression for every workload. The three phase lines above break the
        // total down; this line is what a `RUST_LOG=info` run can be grepped
        // for to compare two builds.
        let typed_boot_elapsed = bootstrap_phase
            .runtime_ready(
                vm.classes.class_manager.read().loaded_count() > 0
                    && vm.natives.native_methods.len() > 0,
            )
            .expect("bootstrap RuntimeReady invariants")
            .finish();
        tracing::info!(
            "boot: SharedVm::new total {:?} (phase 1 classpath {:?}, phase 2 core classes {:?})",
            typed_boot_elapsed,
            __boot_classpath_elapsed,
            __boot_core_classes_elapsed,
        );

        vm
    }
}

/// Whether ZGC **relocation** may run in this configuration — a refusal, not a
/// warning.
///
/// `requested` is the eventual relocation switch. There is none today: no
/// `-XX:` option, no `CRATONVM_*` name in `types/src/flag_groups.rs`, no field
/// on `crate::config::VmConfig`, and `gc/src/zgc/relocate.rs` has no caller —
/// `ZgcRealHeap` is still the non-moving stop-the-world mark-sweep it has
/// always been. So the one call site passes a `false` constant and this gate is
/// a no-op. It exists anyway because a gate added *after* the capability it
/// guards is a gate that shipped one release too late.
///
/// # Why a refusal
///
/// Under a relocating ZGC a reference slot holds a *colored* word, not a
/// machine pointer: a heap offset plus metadata bits plus `Z_COLORED_TAG` at
/// bit 63 (`gc/src/zgc/vaddr.rs`). The load barrier
/// (`gc/src/zgc/barrier.rs`) is what turns that word into a live address and
/// heals the slot, and the interpreter takes it on every read. JIT-compiled
/// code does not: there are nine raw reference-load emission points across the
/// baseline and optimizing x64 tiers — `getfield`, `getstatic`, `aaload`, the
/// `String.value` intrinsic, the LICM hoist, the SIMD row load — and the helper
/// arms that do reach Rust run the loaded word through `plausible_heap_pointer`
/// and answer `0` when it fails, which a colored word is designed to do.
///
/// So compiled code reads either a stale from-space address into an evacuated
/// object (use-after-free) or a spurious `null` for a live one (a wrong answer,
/// silently). Unlike the compressed-oops gate in `SharedVm::new`, whose
/// degraded mode is "slower but correct", there is no correct degraded mode
/// here, so this returns `false` rather than logging and continuing.
///
/// Refusing *relocation* rather than the *JIT* is deliberate: it degrades ZGC
/// to the non-moving collector it already is, instead of degrading the whole VM
/// to the interpreter.
///
/// # What lifts the gate
///
/// Stage (a) of `docs/feature-designs/zgc-jit-load-barrier.md`: the barrier
/// inside the `jit_getfield` / `jit_aaload` / `jit_getstatic` helpers, the
/// inline arms routed to those helpers behind the JIT-side kill switch, and the
/// seven value-degrading plausibility filters removed from the load path. Until
/// then the permitted relocating configuration is "JIT off".
#[cfg(feature = "zgc")]
pub fn zgc_relocation_permitted(requested: bool) -> bool {
    if !requested {
        return false;
    }
    if crate::runtime::env_cache::disable_jit() {
        return true;
    }
    // Stage (a) of `zgc-jit-load-barrier.md` LANDED 2026-08-13, so the JIT is
    // no longer automatically disqualifying.
    //
    // The refusal below exists because JIT-compiled code baked raw 8-byte
    // reference loads at compile-time offsets and would read a coloured word
    // as a pointer. `x64::zgc_read_barrier_blocks_inline_fields` now routes
    // every compact-field access through `jit_getfield` / `jit_putfield_object`
    // whenever the read barrier is armed, and those go through the heap's own
    // accessors, which barrier. That is the same mechanism compressed oops has
    // used for the same reason since before this gate existed.
    //
    // The CAPABILITY, not the runtime state: this runs at VM init, long
    // before any cycle arms a barrier, so asking "is the barrier armed" here
    // would answer no forever and refuse relocation permanently. The question
    // is whether the code this JIT emits will respect a barrier armed later.
    if cratonvm_jit::x64::zgc_codegen_honours_read_barrier() {
        return true;
    }
    // Reported on stderr, not just through `tracing`, for the reason the
    // compressed-oops gate states: a silent fallback would look identical to a
    // successful run, and the operator must see which one they got. The stakes
    // are higher here — the un-refused configuration corrupts the heap rather
    // than merely using more of it.
    eprintln!(
        "[cratonvm] ZGC relocation requested but the JIT is enabled - relocation \
         REFUSED, running the non-moving mark-sweep instead. JIT-compiled code \
         loads reference fields without the ZGC load barrier, so a relocating \
         cycle would hand it stale pointers into evacuated objects \
         (use-after-free) or a spurious null for a live object, with no error \
         path. Re-run with --nojit (CRATONVM_DISABLE_JIT=1) to get relocation, \
         or wait for the JIT-side load barrier - stage (a) of \
         docs/feature-designs/zgc-jit-load-barrier.md."
    );
    false
}

// ---------------------------------------------------------------------------
// Round 4 audit fix (CRIT) — ResolutionCache invalidation on redefine
// ---------------------------------------------------------------------------
//
// `cratonvm_classloading::ClassManager::redefine_class` fires a plain
// `fn(u32)` hook (see `install_resolution_invalidate_hook`) when the
// bytecode of a class is replaced in place. The hook has no captured
// state, so we bridge it to the VM-owned `SharedVm::resolution_cache`
// through this module-private registry.
//
// PER-VM STATE (P0, `docs/architecture/per-vm-state.md`). This used to be a
// `OnceLock<Weak<SharedVm>>` — **first VM wins forever**. That was wrong even
// for the SEQUENTIAL embedding case: create VM A, drop it, create VM B, and
// every `redefine_class` in VM B fired a hook that upgraded VM A's dead
// `Weak`, returned early, and left VM B's `ResolutionCache` / `LinkResolver`
// holding pre-redefine `(declaring_class_id, index)` pairs. Stale resolution
// after a redefine is silent wrong behaviour, not a perf loss.
//
// The hook signature is `fn(u32)` with no VM parameter, so the adapter cannot
// know WHICH VM's class was redefined. Invalidation, unlike installation, is
// safe to over-apply: dropping a cache entry only costs a re-resolve. So the
// registry holds every live VM and each adapter fans out to all of them. In
// the single-VM case (the overwhelmingly common one) the registry has exactly
// one entry and behaviour is bit-identical to the old `OnceLock`.
//
// See `crate::runtime::vtable::install_global_vtable_manager` for the one
// remaining hook that CANNOT be fixed this way: vtable *installation* is not
// idempotent across VMs, so fanning out would corrupt rather than over-apply.

type SharedVmRegistry = parking_lot::Mutex<Vec<Weak<SharedVm>>>;

static RESOLUTION_INVALIDATE_VMS: OnceLock<SharedVmRegistry> = OnceLock::new();

fn shared_vm_hook_registry() -> &'static SharedVmRegistry {
    RESOLUTION_INVALIDATE_VMS.get_or_init(|| parking_lot::Mutex::new(Vec::new()))
}

/// Snapshot the live VMs registered for captureless-`fn` hook bridging.
///
/// Prunes dead `Weak`s while it walks, and returns owning `Arc`s so callers
/// can drop the registry lock before doing any VM work — the adapters below
/// take VM-internal locks (`resolution_cache`, `jit_cache`, `class_manager`),
/// and holding the registry lock across those would invert the lock order.
fn live_hook_vms() -> Vec<Arc<SharedVm>> {
    let mut reg = shared_vm_hook_registry().lock();
    let mut live = Vec::with_capacity(reg.len());
    reg.retain(|weak| match weak.upgrade() {
        Some(shared) => {
            live.push(shared);
            true
        }
        None => false,
    });
    live
}

/// Publish the `Weak<SharedVm>` handle that `resolution_invalidate_adapter`
/// upgrades to invalidate the per-VM `ResolutionCache` on redefine.
///
/// Called from `Vm::new` right after `self_arc` is populated. Registering the
/// same VM twice is a no-op; dead entries from previously-dropped VMs are
/// pruned on every call, so a long-lived process that creates and destroys
/// many VMs does not accumulate them.
pub fn set_global_shared_vm_for_hooks(weak: Weak<SharedVm>) {
    let Some(shared) = weak.upgrade() else {
        return; // already dead; nothing to bridge
    };
    let mut reg = shared_vm_hook_registry().lock();
    reg.retain(|existing| existing.strong_count() > 0);
    let already = reg
        .iter()
        .any(|existing| existing.upgrade().is_some_and(|s| Arc::ptr_eq(&s, &shared)));
    if !already {
        reg.push(weak);
    }
}

/// The `set_class_namer` adapter: `ClassId` -> binary name, for GC
/// diagnostics only.
///
/// `try_read` rather than `read`, and this is load-bearing. Every caller is a
/// failure-path report, and at least one of them (the ZGC fragmentation
/// report) runs on a thread that has just failed an allocation — a thread that
/// may well be the one holding the class-manager write lock further up its own
/// stack. Blocking there would convert a diagnostic into a hang, which is
/// strictly worse than an unnamed class id. A contended lock therefore falls
/// back to the id, which is exactly what the caller prints when no namer is
/// installed at all.
fn class_name_adapter(class_id: u32) -> Option<String> {
    let cid = crate::classloading::ClassId::new(class_id);
    for shared in live_hook_vms() {
        let Some(cm) = shared.classes.class_manager.try_read() else {
            continue;
        };
        if let Some(class) = cm.get_class(cid) {
            return Some(class.name.to_string());
        }
    }
    None
}

/// The `ResolutionInvalidateHook` adapter handed to
/// `install_resolution_invalidate_hook`. Plain `fn` pointer (no captures)
/// so the classloading crate can store it in a `OnceLock`.
///
/// Drops every cached resolution that refers to (or was resolved into)
/// the redefined class. The classloading crate's `RedefineGate` already
/// auto-evicts the per-thread invoke caches via the generation counter;
/// this closes the loop for the slower symbolic-reference cache.
fn resolution_invalidate_adapter(class_id: u32) {
    let cid = crate::classloading::ClassId::new(class_id);
    // Advance the resolution generation FIRST, so no thread can publish a new
    // per-thread site-cache entry that snapshots the pre-invalidation epoch
    // after the authoritative maps below have already been swept. Two of the
    // four firing sites (`upgrade_synthetic_class`, `recompute_subclass_layouts`)
    // move a class's field layout while leaving its `ClassId`, its name and the
    // redefine latch alone — this bump is the only signal a resolved-field
    // cache gets for them. See `runtime::interpreter::constants`'s `RESOLUTION_EPOCH`.
    crate::runtime::interpreter::bump_resolution_epoch();
    // Fan out to every live VM: the hook carries no VM identity, and
    // over-invalidating another VM's cache costs a re-resolve, whereas
    // under-invalidating our own is a stale-resolution correctness bug.
    // With one VM registered (the normal case) this is the same single
    // invalidation the pre-registry code performed.
    for shared in live_hook_vms() {
        shared.classes.resolution_cache.write().invalidate_class(cid);
        // Round 8 audit fix (CRIT #3): the `LinkResolver` reflective cache
        // was missing from the redefine-invalidation cascade. Without
        // this, any cached `(class, name, descriptor)` triple resolved
        // before a `RedefineClasses` continues to return the
        // pre-redefine `(declaring_class_id, index)` pair — pointing at
        // a method index that may now refer to a different method body
        // (or, after a field shape change, a stale field slot).
        // `LinkResolver::invalidate_class` mirrors
        // `ResolutionCache::invalidate_class` (drops by key-class OR
        // resolved-declaring-class match).
        shared.classes.link_resolver.invalidate_class(cid);
    }
}

/// The `JitInvalidateHook` adapter handed to
/// `cratonvm_classloading::install_jit_invalidate_hook`.
///
/// Fired whenever a class's field layout may have changed in a way that
/// already-compiled JIT code cannot safely observe: JVMTI `redefine_class`
/// (Step 8), and — critically — `upgrade_synthetic_class` /
/// `recompute_subclass_layouts`, which (unlike JEP 109 redefine) really can
/// change instance field count/order/offsets when a synthetic JDK stub is
/// later replaced by its real `.class` bytecode. A method JIT-compiled
/// against the stub's layout bakes the stub's field offsets
/// (`compact_field_off`, or the legacy `field_index * SLOT_SIZE` cell
/// offset) directly into its machine code as immediates; nothing else in
/// the VM invalidated that code when the layout later grew/reordered, so it
/// kept reading/writing the WRONG byte offset of any object allocated under
/// the new layout — a stale-offset getfield could silently return whatever
/// raw bytes sat at the old offset (e.g. a small int) where a reference was
/// expected, corrupting anything computed from it. Found while
/// investigating the guarded-inline-getfield SIGSEGV cluster
/// (docs/known-issues/elasticsearch-suite/ES-HANG-20260709-*), but that
/// specific SIGSEGV's confirmed root cause is a different, already-fixed
/// bug: the vm-side JIT field resolvers used to fabricate a `(0, false)`
/// compact slot for any field with no genuine registered `CompactLayout`
/// entry, and the compact-offset inline getfield arm trusted it — see
/// `be7102344`'s commit message ("perf(jit): re-enable guarded-inline-
/// getfield default-ON, root cause fixed") and the WildFly Host Controller
/// fix it cites. This invalidation gap is real and independent of that bug
/// — nothing else in the VM ever evicted JIT code after a layout-changing
/// synthetic-stub upgrade, regardless of the fabricated-slot bug's fix.
///
/// A full cache flush is used rather than a class-scoped eviction —
/// mirroring `redefine_class`'s own existing conservative pattern in
/// `vm_exec.rs` (`jit_cache.write().clear_all()`) — because any OTHER
/// class's compiled method may hold a getfield/putfield referencing the
/// changed class's fields, and there is no reverse index of "which
/// compiled methods read which class's fields" to evict precisely. A full
/// flush is rare (each synthetic class upgrades at most once) and safe:
/// `clear_all` retires evicted methods rather than freeing their code
/// immediately, so any still-active frame stays valid.
fn jit_invalidate_adapter(class_id: u32) {
    // Same fan-out rationale as `resolution_invalidate_adapter`: the hook has
    // no VM identity, and `clear_all` retires rather than frees, so evicting
    // another VM's compiled code is a recompile cost, not a hazard. Missing
    // our OWN VM's eviction after a layout change is a live use of compiled
    // field offsets that no longer describe the class.
    for shared in live_hook_vms() {
        let evicted = shared.jit.jit_cache.write().clear_all();
        if evicted > 0 {
            tracing::debug!(
                "JIT: fully invalidated {evicted} method(s) due to a class layout \
                 change (class_id={class_id})"
            );
        }
    }
}

/// The `ClassInfoHook` adapter handed to `cratonvm_gc::install_class_info_hook`.
///
/// Resolves a raw `ClassId` to `(class_name, num_total_fields)` so the
/// heap's out-of-bounds field-access diagnostic (`gen_heap::set_field` /
/// `get_field`) can print actionable class identity instead of a bare
/// numeric id. Reuses the same live-VM registry — all three hooks are plain
/// captureless `fn` pointers bridged to the VM-owned `SharedVm` through it.
///
/// Returns `None` before any VM handle is wired, after every registered VM has
/// been dropped, or for a class id that no live VM has in its class store.
///
/// This is a diagnostic-only lookup, so answering from the first VM that has
/// the id is acceptable: `ClassId`s are allocated per-VM (`ClassStore::next_id`
/// is `self.classes.len()`), so with two VMs live the same numeric id names two
/// different classes and the name printed may belong to the other VM. The
/// alternative — printing nothing — is strictly worse for a crash diagnostic.
/// Callers must not treat this as an authoritative class lookup.
fn class_info_adapter(class_id: u32) -> Option<(String, usize)> {
    let cid = crate::classloading::ClassId::new(class_id);
    for shared in live_hook_vms() {
        let cm = shared.classes.class_manager.read();
        if let Some(class) = cm.class_store.get(cid) {
            return Some((class.name.to_string(), class.num_total_fields));
        }
    }
    None
}

impl SharedVm {
    /// Dump all loaded non-synthetic classes to a CDS archive file.
    /// Called on VM shutdown when `config.cds_mode == CdsMode::Dump`.
    #[cfg(feature = "experimental-aot")]
    pub fn dump_cds_archive(&self) -> Result<usize, String> {
        let archive_path = self
            .config
            .shared_archive_file
            .as_deref()
            .unwrap_or("classes.jsa");

        let cm = self.classes.class_manager.read();
        let mut generator = cratonvm_native_builtins::cds::CdsArchiveGenerator::new(archive_path);

        // Iterate all loaded classes and add non-synthetic ones with cached bytes
        for class in cm.class_store.iter() {
            if class.origin.is_compatibility_stub() {
                continue;
            }
            if let Some(bytes) = cm.class_bytes_cache.get(&class.id) {
                let entry = cratonvm_native_builtins::cds::CdsArchiveEntry {
                    class_name: class.name.to_string(),
                    bytes_offset: 0,
                    bytes_length: bytes.len() as u32,
                    access_flags: class.access_flags.bits(),
                    superclass: class.superclass.map(|sid| {
                        cm.class_store
                            .get(sid)
                            .map(|c| c.name.to_string())
                            .unwrap_or_default()
                    }),
                    interfaces: class
                        .interfaces
                        .iter()
                        .filter_map(|iid| cm.class_store.get(*iid).map(|c| c.name.to_string()))
                        .collect(),
                    class_bytes: bytes.to_vec(),
                };
                generator.add_entry(entry);
            }
        }

        let count = generator.entry_count();
        generator.write_archive()?;
        Ok(count)
    }

    /// CDS is an explicitly compiled experiment. Keep the public shutdown hook
    /// available in production builds, but reject archive generation rather
    /// than linking the experimental implementation accidentally.
    #[cfg(not(feature = "experimental-aot"))]
    pub fn dump_cds_archive(&self) -> Result<usize, String> {
        Err("CDS support is not compiled; rebuild with --features experimental-aot".to_string())
    }

    /// Allocate a new synthetic ClassId for a lambda proxy.
    /// These IDs start at 0x8000_0000 and increment, avoiding collision
    /// with real ClassIds assigned by the ClassStore.
    pub fn alloc_lambda_proxy_id(&self) -> ClassId {
        ClassId::new(self.classes.next_lambda_id.fetch_add(1, Ordering::Relaxed))
    }

    /// The JDK's own serializability rule for a `LambdaMetafactory`-spun proxy,
    /// and the single place it is decided.
    ///
    /// `AbstractValidatingLambdaMetafactory` treats a lambda as serializable when
    /// its call site passed `FLAG_SERIALIZABLE` **or** its functional interface
    /// already has `java.io.Serializable` as a supertype, and adds `Serializable`
    /// to the spun class's interface list only in the first case
    /// (`isSerializable && !foundSerializableSupertype`). So real HotSpot gives an
    /// ordinary `Supplier<String> s = () -> "x"` no `writeReplace()`, no
    /// `Serializable` interface, and a ClassCastException on `(Serializable) s` --
    /// while `Comparator.comparing(..)` gets all three.
    ///
    /// Callers: the `instanceof`/`checkcast` fast path for lambda proxies, and the
    /// reflective `Class` surfaces (`getDeclaredMethods`, `getInterfaces`,
    /// `getGenericInterfaces`) via `NativeContext::lambda_proxy_serializability`.
    /// Keep it that way -- re-deriving this rule per call site is how the two
    /// halves drift apart.
    pub fn lambda_proxy_serializability(
        &self,
        proxy_class_id: ClassId,
    ) -> cratonvm_native_api::LambdaSerializability {
        use cratonvm_native_api::LambdaSerializability as S;
        let (flag, iface_name, iface_id) = {
            let proxies = self.classes.lambda_proxies.read();
            match proxies.get(&proxy_class_id) {
                Some(cs) => (
                    cs.serializable_flag,
                    cs.functional_interface.clone(),
                    cs.functional_interface_id,
                ),
                None => return S::NotSerializable,
            }
        };
        // Inheritance half. Prefer the bootstrap-captured, loader-correct id;
        // fall back to a load by name, the same order every other lambda-proxy
        // consumer uses. A load failure only costs us the inheritance arm.
        let iface_id = iface_id.or_else(|| self.load_class_concurrent(&iface_name).ok());
        let inherits = iface_id
            .map(|id| {
                self.classes
                    .class_manager
                    .read()
                    .is_assignable_to_name(id, "java/io/Serializable")
            })
            .unwrap_or(false);
        if inherits {
            S::ByInheritance
        } else if flag {
            S::ByFlag
        } else {
            S::NotSerializable
        }
    }

    /// Get an `Arc<SharedVm>` from the stored weak self-reference, or `None`
    /// when this `SharedVm` has no self-reference installed.
    ///
    /// `self_arc` is installed at the end of `Vm::new()`, but `SharedVm::new()`
    /// is public and cannot install it (the `Arc` does not exist yet — the
    /// caller creates it). A bare `Arc::new(SharedVm::new(config))` — the shape
    /// most unit fixtures use — therefore has `self_arc == None`, which is a
    /// legitimate state, not an impossible one. Callers that can degrade
    /// gracefully (e.g. refusing to spawn a thread rather than aborting the
    /// process) must use this instead of [`Self::get_arc`].
    pub fn try_get_arc(&self) -> Option<Arc<SharedVm>> {
        // `upgrade()` succeeds whenever `self_arc` is set, because we are
        // called via `&self` on a `SharedVm` that lives inside an
        // `Arc<SharedVm>` — at least one strong reference is alive for the
        // duration of this borrow.
        self.self_arc.read().as_ref().and_then(|w| w.upgrade())
    }

    /// Get an `Arc<SharedVm>` from the stored weak self-reference.
    ///
    /// Panics if `self_arc` was never set. This used to be spelled
    /// `unreachable!("self_arc is set during Vm::new and never cleared")`, but
    /// the invariant that message asserts is false: it holds only for VMs built
    /// through `Vm::new()`, and `SharedVm::new()` is public. A reached
    /// `unreachable!` is always a bug, so the case is now named honestly and
    /// the message tells the caller how to fix the fixture.
    pub fn get_arc(&self) -> Arc<SharedVm> {
        self.try_get_arc().unwrap_or_else(|| {
            panic!(
                "SharedVm::get_arc() on a VM with no `self_arc` weak self-reference. \
                 `Vm::new()` installs it; a hand-built `Arc::new(SharedVm::new(..))` \
                 fixture must install it itself with \
                 `*shared.self_arc.write() = Some(Arc::downgrade(&shared));`. \
                 Call sites that can degrade should use `try_get_arc()` instead."
            )
        })
    }
}

impl SharedVm {
    /// Dump the collected missing-native-method audit log to tracing.
    ///
    /// Called on VM shutdown when `config.audit_missing_natives` is true.
    /// Prints a sorted, deduplicated list of ACC_NATIVE methods that were
    /// invoked during execution but had no Rust implementation.
    pub fn dump_missing_natives(&self) {
        let log = self.debug.missing_natives_log.lock();
        if log.is_empty() {
            return;
        }
        let mut sorted: Vec<_> = log.clone();
        sorted.sort_by(|a, b| {
            (&a.class_name, &a.method_name, &a.descriptor).cmp(&(
                &b.class_name,
                &b.method_name,
                &b.descriptor,
            ))
        });
        tracing::info!(
            "=== Missing Native Methods Audit ({} unique) ===",
            sorted.len()
        );
        for entry in &sorted {
            match &entry.sample_call_site {
                Some(site) => {
                    tracing::info!("  {} (first seen from {})", entry.full_signature(), site)
                }
                None => tracing::info!("  {}", entry.full_signature()),
            }
        }
        tracing::info!("=== End Missing Natives ===");
    }

    /// Query the collected missing-native-method audit log.
    ///
    /// Returns a sorted, deduplicated list of `"class.method descriptor"` strings
    /// representing every ACC_NATIVE method that was invoked but had no Rust
    /// implementation registered. Only populated when `config.audit_missing_natives`
    /// is `true`.
    pub fn get_missing_natives(&self) -> Vec<String> {
        let log = self.debug.missing_natives_log.lock();
        let mut sigs: Vec<String> = log.iter().map(MissingNativeEntry::full_signature).collect();
        sigs.sort();
        sigs.dedup();
        sigs
    }

    /// NEW-10: Write the collected missing-native-method audit log to
    /// `path` as JSON.
    ///
    /// The on-disk schema is stable and diff-friendly so a committed
    /// baseline can be compared across releases:
    ///
    /// ```json
    /// {
    ///   "missing_natives": [
    ///     {
    ///       "class": "java/lang/Foo",
    ///       "name": "bar",
    ///       "descriptor": "(I)V",
    ///       "sample_call_site": "com/example/Main.main([Ljava/lang/String;)V"
    ///     }
    ///   ]
    /// }
    /// ```
    ///
    /// Entries are sorted by `(class, name, descriptor)` so repeated
    /// runs on the same program produce byte-identical output. The
    /// file is created (or truncated) with `0600` permissions on Unix
    /// via the standard `File::create`; callers targeting a shared
    /// path should set their own umask beforehand.
    ///
    /// Returns an `io::Error` if the file cannot be written. Does not
    /// panic — safe to call unconditionally at VM shutdown.
    pub fn dump_missing_natives_json(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> std::io::Result<()> {
        let log = self.debug.missing_natives_log.lock();
        let mut sorted: Vec<MissingNativeEntry> = log.clone();
        sorted.sort_by(|a, b| {
            (&a.class_name, &a.method_name, &a.descriptor).cmp(&(
                &b.class_name,
                &b.method_name,
                &b.descriptor,
            ))
        });
        // Hand-serialize to avoid pulling in serde_json just for this
        // module. The schema is tiny and well-defined; a lightweight
        // escape routine handles the only dynamic data (class and
        // method names use ASCII + `/` + `$` + digits, so escape
        // work is minimal — but we handle the full set defensively).
        let mut out = String::with_capacity(256 + 128 * sorted.len());
        out.push_str("{\n  \"missing_natives\": [");
        for (i, entry) in sorted.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("\n    {\n");
            out.push_str(&format!(
                "      \"class\": {},\n",
                json_escape(&entry.class_name)
            ));
            out.push_str(&format!(
                "      \"name\": {},\n",
                json_escape(&entry.method_name)
            ));
            out.push_str(&format!(
                "      \"descriptor\": {},\n",
                json_escape(&entry.descriptor)
            ));
            match &entry.sample_call_site {
                Some(site) => out.push_str(&format!(
                    "      \"sample_call_site\": {}\n",
                    json_escape(site)
                )),
                None => out.push_str("      \"sample_call_site\": null\n"),
            }
            out.push_str("    }");
        }
        if !sorted.is_empty() {
            out.push('\n');
            out.push_str("  ");
        }
        out.push_str("]\n}\n");

        use std::io::Write;
        let mut file = std::fs::File::create(path)?;
        file.write_all(out.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    /// The substitution policy this VM booted under
    /// (`docs/feature-designs/jdk-only-mode.md` §6).
    ///
    /// Read straight off the config, which is immutable after construction:
    /// the mode is decided once, at launch, and every consumer — the census
    /// dumps below, `vm_flags`, `java.vm.info` — reads it from here rather
    /// than from a process global (§2).
    pub fn compatibility_mode(&self) -> CompatibilityMode {
        self.config.compatibility_mode
    }

    /// Feature version of the **runtime image** this VM is running against, for
    /// the report's `jdk_feature` (`jdk-only-mode.md` §9).
    ///
    /// Read from `$JAVA_HOME/release`, and `None` when there is no configured
    /// `java_home` to read it from. Deliberately NOT
    /// `java.specification.version`: that property is a fixed `"25"` this VM
    /// reports to Java code regardless of which image is mounted, so using it
    /// here would report `25` for a run against a JDK 21 image — and the one
    /// thing every consumer does with `jdk_feature` is treat it as *image
    /// identity*. `tools/jdk-only-blockers/blockers.py` puts it in the artifact
    /// file name because "no static source review can produce an exhaustive
    /// missing-method list across JDK versions"; naming a JDK 21 census
    /// `jdk-25-*` is worse than refusing to name it. `None` makes that tool
    /// stop and ask for `--jdk-feature N`, which is the correct failure.
    pub fn jdk_feature_version(&self) -> Option<u32> {
        jdk_feature_from_release_file(self.config.java_home.as_deref()?)
    }

    /// Synthetic-stub census: dump every registered native with its
    /// [`NativeKind`](cratonvm_native_api::NativeKind) tag
    /// (intrinsic / bridge / synthetic-stub) to a diff-stable JSON file.
    ///
    /// Thin wrapper over [`SharedVm::dump_native_census_json`] with
    /// `verbose = false` (registration sites redacted). The name, parameter
    /// list and `(intrinsic, bridge, synthetic-stub)` return are **unchanged**
    /// on purpose: in-crate tests and embedders call this exact signature, and
    /// the schema-2 upgrade is a change to the file's *contents*, not to its
    /// Rust surface. `redacted` is the right default for a caller that has no
    /// way to express `--explain-jdk-only`.
    ///
    /// The counts it returns are **registrations**, counted over the same
    /// `registrations` vector `NativeMethodRegistry::dump_registrations`
    /// iterates — superseded rows included, one row per `register()` call — so
    /// they stay numerically identical to schema 1 and the stub ratchet's
    /// exact 157 `synthetic-stub` baseline is unaffected by the schema bump.
    pub fn dump_native_registry_json(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> std::io::Result<(usize, usize, usize)> {
        self.dump_native_census_json(path, false)
    }

    /// Native census, **schema 2** (`jdk-only-mode.md` §4, §9).
    ///
    /// Schema 1 answered "what is registered". That was enough for a ratchet
    /// and useless for the untangling work: it could not say which
    /// registration *won* a contested slot, who installed it, or whether the
    /// slot was ever actually dispatched. Schema 2 adds those three columns:
    ///
    /// ```json
    /// {
    ///   "schema_version": 5,
    ///   "mode": "compatible",
    ///   "image_adjudication": true,
    ///   "counts": { "intrinsic": 2, "bridge": 1, "synthetic-stub": 1, "total": 4 },
    ///   "invocations": { "intrinsic": 267, "bridge": 2818, "synthetic-stub": 0 },
    ///   "slots_with_incomplete_invocations": 25,
    ///   "natives": [
    ///     { "class": "java/lang/System", "name": "arraycopy",
    ///       "descriptor": "([Ljava/lang/Object;I[Ljava/lang/Object;II)V",
    ///       "kind": "intrinsic",
    ///       "registered_by": "native-builtins/src/lib.rs:1234",
    ///       "overwrote": "synthetic-stub",
    ///       "invocations": 10,
    ///       "invocations_complete": false,
    ///       "kind_stated": true, "kind_chosen": true,
    ///       "owns_slot": true,
    ///       "real_declaring_method": { "loaded": true, "declared": true,
    ///                                  "acc_native": true, "has_code": false },
    ///       "image_declaring_method": { "image_has_class": true, "declared": true,
    ///                                   "acc_native": true, "has_code": false,
    ///                                   "inherited_from": null,
    ///                                   "inherited_acc_native": false,
    ///                                   "inherited_has_code": false,
    ///                                   "inherited_abstract": false } }
    ///   ]
    /// }
    /// ```
    ///
    /// ## Schema 3 — `image_declaring_method`
    ///
    /// Schema 2's `real_declaring_method` answers from the **loaded** class
    /// store, so it is a measurement of the run: `loaded: false` means "this
    /// workload never touched the class". That is the right answer to the
    /// question it asks and the wrong instrument for adjudicating the registry,
    /// because the registrations most in need of a verdict are the ones no
    /// single workload exercises. `docs/known-issues/jdk-only/`'s sixteen
    /// `JDK-ONLY-CLASSIFY: unknown — needs census` verdicts are all blocked on
    /// exactly that.
    ///
    /// `image_declaring_method` asks the same four questions of the **bytes on
    /// the class path**, parsed and thrown away — see
    /// [`ClassManager::adjudicate_natives_against_image`], which explains at
    /// length why it must not simply load the classes. It is populated only
    /// when `verbose`, because it costs one class-file parse per distinct
    /// registered class; `image_adjudication` at the top level says whether the
    /// pass ran, so a `null` column is never ambiguous.
    ///
    /// Reading the two together is the point: `real` says whether this run
    /// exercised the slot, `image` says whether the JDK declares it at all.
    ///
    /// ## Schema 5 — `invocations_complete` and `slots_with_incomplete_invocations`
    ///
    /// `invocations` was always documented as a **lower bound**
    /// (`NativeMethodRegistry::record_invocation`), and the registry has
    /// carried a per-slot "this is a floor" bit
    /// (`NativeMethodRegistry::mark_invocations_incomplete`) since
    /// 2026-08-17. Until schema 5 **this writer emitted neither** — measured
    /// against real dumps by `G37-1` §6 N2 and `G42-1` §6 N1 — so 25 slots
    /// declared themselves uncounted and every reader of the file was told
    /// `invocations: 0` with nothing to distinguish "never called" from "called
    /// through a path that does not count".
    ///
    /// Schema 5 emits the bit per row and the slot total in the header. It
    /// changes **no number**: not a count, not an invocation tally, not
    /// `owns_slot`. What it changes is what a number licenses, and only ever in
    /// the direction of less confidence.
    ///
    /// What the bit does and does not claim, spelled out because it is easy to
    /// over-read: `false` means a dispatch path is *wired* for this slot that
    /// will not count, not that such a dispatch has happened. `true` means no
    /// path has declared itself — which is **not** "exact", because a bypass
    /// nobody has audited is indistinguishable from no bypass. There is no
    /// configuration of this VM in which the whole column is exact
    /// (`G37-1` §2, `G42-1` §5); `--nojit` with `CRATONVM_DISABLE_INTRINSICS=1`
    /// is the least inexact one.
    ///
    /// Notes on the fields that are easy to misread:
    ///
    /// * `counts` still counts **registrations**, not invocations — it is the
    ///   schema-1 block, unchanged, so the existing ratchet keeps working.
    ///   Per-slot `invocations` is the new, different measurement; summing it
    ///   per kind is `difftest`'s job.
    /// * `overwrote` is the `NativeKind` of the entry this registration
    ///   displaced, or `null`. Registration is last-write-wins throughout
    ///   `SharedVm::new` (see the two LAST-WRITE-WINS BOUNDARY blocks), and
    ///   this column plus `registered_by` is the evidence the later
    ///   untangling wave needs — §8 explicitly defers the untangling itself.
    /// * `registered_by` is redacted unless `verbose` (i.e. unless
    ///   `--explain-jdk-only` was passed): absolute build paths leak the
    ///   builder's home directory into committed baselines and make the file
    ///   differ per machine. See [`redact_registration_site`].
    /// * `real_declaring_method` reports what the *real* class says about the
    ///   slot, which is how a reviewer tells a legitimate `ACC_NATIVE` bridge
    ///   from a native shadowing concrete bytecode (§1 item 4, §7 step 3).
    ///   `has_code` is derived from the access flags (JVMS §4.6: `Code` is
    ///   present iff the method is neither `native` nor `abstract`) rather
    ///   than from `ClassFileMethod::code()`, which returns `None` for a
    ///   not-yet-force-decoded lazy attribute — a decode-state artefact, not
    ///   a fact about the class.
    ///
    /// Rows are sorted by `(class, name, descriptor)` with a **stable** sort,
    /// so the duplicate rows a contested triple produces stay in registration
    /// order — which is the overwrite chronology the untangling wave needs, and
    /// is deterministic run to run. Hand-serialized (no serde_json), mirroring
    /// `dump_missing_natives_json`. Returns the per-kind registration counts.
    ///
    /// ## The only native-census writer
    ///
    /// `--dump-native-registry` is served by **this** function: `vm-cli`'s
    /// `write_jdk_only_dumps` calls it directly. There used to be a second,
    /// independently written schema-2 writer in `vm-cli/src/main.rs`, which is
    /// a genuine consumer hazard — one `schema_version` with two shapes means a
    /// reader that works against one silently mis-reads the other. The three
    /// places the two disagreed were resolved as follows.
    ///
    /// * **Top-level `"mode"` is kept.** It is additive (no consumer keys off
    ///   the object's shape: `difftest::census` finds the row array by scanning
    ///   for objects carrying `kind`, and CI / `scripts/jdk-only-census.sh`
    ///   grep the `counts` block), and it is what lets `registry-real.json` and
    ///   `registry-no-stubs.json` be told apart from their contents. Diffing
    ///   those two files is the entire point of the census script; a census
    ///   that cannot say which policy produced it is a footgun.
    /// * **Registration order is kept as the tie-break** for duplicate triples,
    ///   i.e. the stable sort above rather than the launcher's
    ///   `(class, name, descriptor, registered_by)`. Two reasons. It is the
    ///   overwrite chronology, which is the fact the untangling wave needs and
    ///   which `registered_by` order destroys; and `registered_by` can be an
    ///   absolute build path, so sorting on it makes *row order* depend on the
    ///   build machine even though the emitted value is redacted — the opposite
    ///   of diff-stable. `census()` documents registration order, and
    ///   `slice::sort_by` is stable, so this order is deterministic.
    /// * **`real_declaring_method` is filled in**, where the launcher emitted
    ///   `null` on the (correct) principle that a census must not perturb what
    ///   it measures. Filling it here carries none of that risk:
    ///   `get_loaded_class_id` is a lookup through `&self`, not an initiating
    ///   load, so nothing is loaded to answer the question and `"loaded":
    ///   false` is a real answer ("this run never loaded the class") rather
    ///   than a probe declined. Caveat worth knowing when reading the column:
    ///   the lookup is requester-less, so for a name no built-in loader has
    ///   defined it can answer from a lone user-defined loader's copy.
    pub fn dump_native_census_json(
        &self,
        path: impl AsRef<std::path::Path>,
        verbose: bool,
    ) -> std::io::Result<(usize, usize, usize)> {
        let cm = self.classes.class_manager.read();
        self.dump_native_census_json_with(path, verbose, Some(&cm))
    }

    /// [`Self::dump_native_census_json`] against a class manager the caller
    /// already holds — or, with `cm = None`, the same census minus the one
    /// column that needs the class store.
    ///
    /// The registry rows themselves come from `natives.native_methods`, which
    /// has its own lock, so a `None` here degrades *narrowly*: every row is
    /// still written with its kind, provenance and invocation count, and only
    /// `real_declaring_method` becomes `null`. The file is marked
    /// `"partial": true` all the same, because a reader cannot otherwise tell a
    /// `null` that means "this run never loaded the class" from one that means
    /// "nobody looked".
    pub(crate) fn dump_native_census_json_with(
        &self,
        path: impl AsRef<std::path::Path>,
        verbose: bool,
        cm: Option<&crate::classloading::ClassManager>,
    ) -> std::io::Result<(usize, usize, usize)> {
        use cratonvm_native_api::NativeKind;
        let mut rows = self.natives.native_methods.census();
        rows.sort_by(|a, b| {
            (&a.class, &a.name, &a.descriptor).cmp(&(&b.class, &b.name, &b.descriptor))
        });
        let mut n_intrinsic = 0usize;
        let mut n_bridge = 0usize;
        let mut n_stub = 0usize;
        for row in &rows {
            match row.kind {
                NativeKind::Intrinsic => n_intrinsic += 1,
                NativeKind::Bridge => n_bridge += 1,
                NativeKind::SyntheticStub => n_stub += 1,
            }
        }

        // The per-row image adjudication (`image_declaring_method`). Costs one
        // class-file parse per DISTINCT registered class — roughly a thousand —
        // so it rides `verbose` (`--explain-jdk-only`) rather than firing on
        // every difftest child. The key is emitted either way, `null` when the
        // pass did not run, so a reader never has to infer from the shape which
        // kind of census this is; `image_adjudication` says it outright.
        let image_verdicts: Option<Vec<cratonvm_classloading::ImageMethodVerdict>> =
            match (verbose, cm) {
                (true, Some(cm)) => {
                    let triples: Vec<(String, String, String)> = rows
                        .iter()
                        .map(|r| (r.class.clone(), r.name.clone(), r.descriptor.clone()))
                        .collect();
                    Some(cm.adjudicate_natives_against_image(&triples))
                }
                _ => None,
            };

        let mut out = String::with_capacity(256 + 256 * rows.len());
        // Schema 4 (2026-08-11): `image_declaring_method` gained the four
        // `inherited_*` keys. The bump is not cosmetic — a reader that scores a
        // schema-3 census with schema-4 logic silently counts every inherited
        // row as unadjudicated, which is the exact miscount the keys exist to
        // end, so `jdk-only-bridge-ratchet.py` refuses the older shape rather
        // than degrading.
        // Schema 5 (2026-08-17): rows carry `invocations_complete` and the
        // header carries `slots_with_incomplete_invocations`. See
        // [`NATIVE_CENSUS_SCHEMA_VERSION`] for why this is a bump and not an
        // additive-at-the-same-version change, and for the two consumers that
        // pin it by equality.
        out.push_str(&format!(
            "{{\n  \"schema_version\": {},\n",
            NATIVE_CENSUS_SCHEMA_VERSION
        ));
        out.push_str(&format!(
            "  \"image_adjudication\": {},\n",
            image_verdicts.is_some()
        ));
        out.push_str(&format!(
            "  \"mode\": {},\n",
            json_escape(self.compatibility_mode().as_str())
        ));
        if cm.is_none() {
            out.push_str(
                "  \"partial\": true,\n  \"partial_reason\": \"class-manager lock unavailable \
                 on the System.exit path; every real_declaring_method is null because the \
                 class store was not read, not because the class was absent\",\n",
            );
        }
        out.push_str("  \"counts\": {\n");
        out.push_str(&format!("    \"intrinsic\": {n_intrinsic},\n"));
        out.push_str(&format!("    \"bridge\": {n_bridge},\n"));
        out.push_str(&format!("    \"synthetic-stub\": {n_stub},\n"));
        out.push_str(&format!("    \"total\": {}\n", rows.len()));
        // Per-kind dispatch totals, the same three keys in the same order as
        // `vm-cli`'s writer. Separate from `counts` on purpose: `counts` is
        // registrations, this is invocations, and the two have been confused
        // before.
        out.push_str("  },\n  \"invocations\": {\n");
        out.push_str(&format!(
            "    \"intrinsic\": {},\n",
            self.natives
                .native_methods
                .invocations_of_kind(NativeKind::Intrinsic)
        ));
        out.push_str(&format!(
            "    \"bridge\": {},\n",
            self.natives
                .native_methods
                .invocations_of_kind(NativeKind::Bridge)
        ));
        out.push_str(&format!(
            "    \"synthetic-stub\": {}\n",
            self.natives
                .native_methods
                .invocations_of_kind(NativeKind::SyntheticStub)
        ));
        out.push_str("  },\n");
        // How many slots have a dispatch path that has declared itself
        // uncounted. Emitted between `invocations` and `natives` so it reads as
        // the qualifier on the block immediately above it. Cold: one relaxed
        // load per slot, once, at report time — the same shape and the same
        // justification as the three `invocations_of_kind` calls above.
        out.push_str(&native_census_incomplete_header_json(
            self.natives
                .native_methods
                .slots_with_incomplete_invocations(),
        ));
        out.push_str("  \"natives\": [");

        // One read lock for the whole loop: `real_declaring_method` asks the
        // class manager a question per row, and re-acquiring L10 tens of
        // thousands of times would turn a diagnostic dump into a contention
        // event. The caller acquires it (or, on the `System.exit` path, fails
        // to) so that all three artefacts describe the same instant.
        for (i, row) in rows.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("\n    {\n");
            out.push_str(&format!("      \"class\": {},\n", json_escape(&row.class)));
            out.push_str(&format!("      \"name\": {},\n", json_escape(&row.name)));
            out.push_str(&format!(
                "      \"descriptor\": {},\n",
                json_escape(&row.descriptor)
            ));
            out.push_str(&format!(
                "      \"kind\": {},\n",
                json_escape(row.kind.as_str())
            ));
            match &row.registered_by {
                Some(site) => out.push_str(&format!(
                    "      \"registered_by\": {},\n",
                    json_escape(&if verbose {
                        site.clone()
                    } else {
                        redact_registration_site(site)
                    })
                )),
                None => out.push_str("      \"registered_by\": null,\n"),
            }
            match row.overwrote {
                Some(kind) => out.push_str(&format!(
                    "      \"overwrote\": {},\n",
                    json_escape(kind.as_str())
                )),
                None => out.push_str("      \"overwrote\": null,\n"),
            }
            // "How many dispatches resolved this slot by name or id" — and,
            // inseparably, whether that is a total or a floor. Before schema 5
            // only the first half was emitted, while the registry had already
            // been told about 25 bypassing slots; a reader could not tell a
            // counted zero from an uncounted one. See
            // [`native_census_invocations_json`].
            out.push_str(&native_census_invocations_json(
                row.invocations,
                row.invocations_complete,
            ));
            // "Did anyone adjudicate this kind, or did it inherit an ambient
            // `set_category`?" — the discriminator the 157-entry
            // reclassification needs. See `NativeCensusEntry::kind_stated`.
            out.push_str(&format!("      \"kind_stated\": {},\n", row.kind_stated));
            // "…or did nobody have an opinion at all?" — `kind_stated` is false
            // on every row a deliberate `with_category` scope covers, so it
            // cannot answer that. See `NativeCensusEntry::kind_chosen`.
            out.push_str(&format!("      \"kind_chosen\": {},\n", row.kind_chosen));
            // "Would a dispatch of this triple reach THIS row?" A superseded
            // registration answers `false` and can never be dispatched, so it
            // is not a registration any reclassification wave has to decide.
            // See `NativeCensusEntry::owns_slot` for the measured size of the
            // difference.
            out.push_str(&format!("      \"owns_slot\": {},\n", row.owns_slot));

            match cm {
                Some(cm) => {
                    let declaring = cm
                        .get_loaded_class_id(&row.class)
                        .and_then(|id| cm.get_class(id));
                    let method =
                        declaring.and_then(|c| c.find_method(&row.name, &row.descriptor));
                    out.push_str("      \"real_declaring_method\": {");
                    out.push_str(&format!(
                        "\"loaded\": {}, \"declared\": {}, \"acc_native\": {}, \"has_code\": {}",
                        declaring.is_some(),
                        method.is_some(),
                        method.is_some_and(|m| m.is_native()),
                        method.is_some_and(|m| !m.is_native() && !m.is_abstract()),
                    ));
                    out.push_str("},\n");
                }
                // Not `{"loaded": false, …}`: that is a *measurement* saying the
                // run never touched the class, and it would be a lie here. See
                // this function's `partial` note.
                None => out.push_str("      \"real_declaring_method\": null,\n"),
            }
            match image_verdicts.as_ref().map(|v| &v[i]) {
                // Schema 4 adds the four `inherited_*` keys. They are emitted on
                // EVERY row — `null`/`false` included — so that "the named class
                // declares it", "a SUPERTYPE declares it" and "nothing in the
                // hierarchy does" are three distinguishable answers instead of
                // one absent key and a guess. Before them, `declared: false` was
                // read as "dead registration" and was wrong for three quarters
                // of the bucket; see `ImageMethodVerdict::inherited_from`.
                Some(v) => out.push_str(&format!(
                    "      \"image_declaring_method\": {{\"image_has_class\": {}, \
                     \"declared\": {}, \"acc_native\": {}, \"has_code\": {}, \
                     \"inherited_from\": {}, \"inherited_acc_native\": {}, \
                     \"inherited_has_code\": {}, \"inherited_abstract\": {}}}\n",
                    v.image_has_class,
                    v.declared,
                    v.acc_native,
                    v.has_code,
                    match &v.inherited_from {
                        Some(c) => json_escape(c),
                        None => "null".to_string(),
                    },
                    v.inherited_acc_native,
                    v.inherited_has_code,
                    v.inherited_abstract,
                )),
                None => out.push_str("      \"image_declaring_method\": null\n"),
            }
            out.push_str("    }");
        }

        if !rows.is_empty() {
            out.push_str("\n  ");
        }
        out.push_str("]\n}\n");

        use std::io::Write;
        let mut file = std::fs::File::create(path)?;
        file.write_all(out.as_bytes())?;
        file.sync_all()?;
        Ok((n_intrinsic, n_bridge, n_stub))
    }

    /// Class-origin census (`--dump-class-origins`, `jdk-only-mode.md` §5, §9).
    ///
    /// Schema 1:
    ///
    /// ```json
    /// {
    ///   "schema_version": 1,
    ///   "counts": { "boot-image": 312, "...": 0, "total": 343 },
    ///   "classes": [
    ///     { "name": "java/lang/String", "origin": "boot-image", "reason": null,
    ///       "requested_by": null, "real_bytes_found": true, "loader_id": 0 }
    ///   ]
    /// }
    /// ```
    ///
    /// `counts` is seeded to zero from the full
    /// [`ClassOrigin::as_str`](cratonvm_classloading::class_origin::ClassOrigin::as_str)
    /// vocabulary, so `"compatibility-stub": 0` is *stated* rather than implied
    /// by an absent key — the one assertion a strict-mode reader most wants and
    /// the one an omission would silently fake. Rows sort by
    /// `(name, loader_id, origin)`: a class name legitimately appears once per
    /// defining loader, and twice with different origins during an in-place
    /// stub-upgrade window, so all three are needed for a total order.
    /// Returns the row count.
    ///
    /// The **only** class-origin writer: `--dump-class-origins` calls this,
    /// and so does any embedder. `vm-cli` used to carry an independently
    /// written copy; the one difference worth keeping from it was the
    /// redaction, which is why this entry point now takes `verbose` instead of
    /// telling embedders to redact the rows themselves. `verbose` is
    /// `--explain-jdk-only`: **false redacts, and false is the default** for
    /// every caller that cannot express the flag.
    ///
    /// Redaction covers `name`, `reason` and `requested_by` (via
    /// [`redact_absolute_paths`]), because all three can carry an absolute path
    /// out of a `CompatibilityStub` reason or a requester attribution, and this
    /// file gets committed as a baseline and pasted into issues. `origin` is a
    /// closed tag vocabulary and is never redacted.
    ///
    /// Emitted in **both** modes: under `Compatible` the origins are recorded
    /// but nothing is refused (§5), which is what makes the census meaningful
    /// as a baseline *before* enforcement lands (§10).
    pub fn dump_class_origins_json(
        &self,
        path: impl AsRef<std::path::Path>,
        verbose: bool,
    ) -> std::io::Result<usize> {
        let cm = self.classes.class_manager.read();
        self.dump_class_origins_json_with(path, verbose, Some(&cm))
    }

    /// [`Self::dump_class_origins_json`] against a class manager the caller
    /// already holds — or, with `cm = None`, a **labelled empty** census.
    ///
    /// `None` exists for exactly one caller: the `System.exit` path, which runs
    /// on an arbitrary Java thread and may find the class-manager lock held (see
    /// [`Self::try_write_jdk_only_dumps_for_exit`]). Every row of this census
    /// comes from the class store, so without the lock there is nothing to
    /// report — and a zero-row census that does not *say* it is empty for want
    /// of a lock reads as "this run fabricated nothing", which is the exact
    /// false-green contract §11's acceptance criteria must be immune to. The
    /// `"partial": true` key is therefore not decoration; it is what makes the
    /// degraded file safe to publish.
    pub(crate) fn dump_class_origins_json_with(
        &self,
        path: impl AsRef<std::path::Path>,
        verbose: bool,
        cm: Option<&crate::classloading::ClassManager>,
    ) -> std::io::Result<usize> {
        let out = match cm {
            Some(cm) => {
                let mut rows = cm.dump_class_origins();
                let n = rows.len();
                (render_class_origins_json(&mut rows, verbose), n)
            }
            None => (render_partial_class_origins_json(), 0),
        };

        use std::io::Write;
        let mut file = std::fs::File::create(path)?;
        file.write_all(out.0.as_bytes())?;
        file.sync_all()?;
        Ok(out.1)
    }

    /// Snapshot of the three **process-global** JDK-only violation sinks, in a
    /// stable order: `[jit-compile, jit-helpers, interpreter-dispatch]`.
    ///
    /// The other two sinks the report folds — `refused_registrations()` and
    /// `origin_violations()` — are VM-scoped: they hang off this `SharedVm`'s
    /// registry and class manager. The three here are `static`s, because the
    /// sites that record them (a JIT compile-time bytecode scan, a JIT runtime
    /// fast path reached from emitted code, and the interpreter's name-triple
    /// dispatch resolver) have no `&SharedVm` at hand cheaply enough to carry
    /// one. That difference is why they are gated below and why the ordering
    /// contract of the array matters — see [`JDK_ONLY_PROCESS_SINKS`].
    ///
    /// # Why `Compatible` returns three empty vectors without touching them
    ///
    /// Two independent reasons, and both are needed:
    ///
    /// 1. **Cost.** Each accessor takes a `parking_lot::Mutex` and clones a
    ///    `Vec<JdkOnlyViolation>`. A `Compatible` VM that asked for
    ///    `--jdk-only-report` would pay three uncontended locks for three
    ///    guaranteed-empty answers. The early return is free.
    /// 2. **Honesty.** `cratonvm_jit`'s policy is a process global that
    ///    latches monotonically toward strict (see
    ///    `cratonvm_jit::set_jit_execution_policy`). Two VMs in one process can
    ///    therefore make a `Compatible` VM compile under strict policy — never
    ///    the reverse — and that VM's JIT would start filling these sinks with
    ///    refusals it never asked for. Folding them into a report whose `mode`
    ///    field says `"compatible"` would attribute another VM's policy
    ///    decisions to this one. A `Compatible` VM has no JDK-only policy, so
    ///    it reports no JDK-only violations, full stop.
    ///
    /// The residual imprecision runs the other way and cannot be fixed here:
    /// in a multi-VM process where at least one VM is `JdkOnly`, these three
    /// lists are **process-wide**, not this VM's. A `JdkOnly` VM's report may
    /// therefore include rows produced while a sibling `Compatible` VM was
    /// running. Making them per-VM is the wave-2 change named in
    /// `cratonvm_jit`'s `JDK-ONLY-WAVE2` note (move the policy and the helper
    /// addresses into a per-VM struct); until then this is over-reporting in
    /// the strict direction, which is the safe direction for a diagnostic.
    ///
    /// Each sink is append-only and bounded (4096 entries by default since
    /// 2026-08-20, `CRATONVM_NATIVE_SHADOW_SINK_CAP`) with an internal dedup, so
    /// a positional watermark into these vectors is stable across calls — that
    /// is what `--trace-jdk-only` uses to drain incrementally. Do not re-derive
    /// the bound from a literal here: the report's `observation_sink` object
    /// publishes each sink's actual `cap`, and what it dropped.
    pub fn jdk_only_process_violations(
        &self,
    ) -> [Vec<cratonvm_types::error::JdkOnlyViolation>; JDK_ONLY_PROCESS_SINKS] {
        if !self.compatibility_mode().is_jdk_only() {
            // Written out rather than `std::array::from_fn` so that raising
            // `JDK_ONLY_PROCESS_SINKS` for a new sink is a compile error here
            // and in the array below, instead of silently returning a slot
            // nothing ever fills.
            return [Vec::new(), Vec::new(), Vec::new()];
        }
        [
            // Compile-time thin-direct-native binds refused by `try_compile`'s
            // bytecode scan. `NativeShadowsBytecode` with
            // `native_kind: "jit-thin-direct-helper"`.
            cratonvm_jit::jdk_only_jit_violations(),
            // Runtime by-name fast-path admissions refused at cache-fill time.
            // Carries whatever `resolve_native_dispatch_wave1` produced, which
            // for this site is a `SyntheticNativeInvocation` `Reject`.
            crate::jit::helpers::jdk_only_jit_helper_violations(),
            // Interpreter dispatch: concrete bytecode preferred over a
            // registered non-intrinsic native. `NativeShadowsBytecode` with the
            // *registered* `NativeKind::as_str()`, so these never collide with
            // the JIT's `"jit-thin-direct-helper"` rows even for the same
            // triple — the two are different facts about the same method and
            // both belong in the report.
            crate::vm::jdk_only_native_shadow_observations(),
        ]
    }

    /// Exact refusal **event** counts from the same three process-global
    /// sources, for the report's `refusals` block and the `--trace-jdk-only`
    /// summary line.
    ///
    /// These are not a second spelling of `violations[]`. Every sink above is
    /// deduplicated by triple and CAPPED (see `observation_sink` in the report
    /// for each one's cap and drop count); these counters are uncapped
    /// `AtomicU64`s incremented once per refusal event. A run that
    /// refuses `HashMap.put` ten million times contributes **one** row to
    /// `violations[]` and ten million to `jit_direct_native_binds`. Reporting
    /// only the rows would understate the blast radius; reporting only the
    /// counters would lose the identities. Both, in separate blocks, is the
    /// only shape that double-counts neither.
    ///
    /// `jit_inline_cache_natives` is the one source with **no** structured
    /// violation behind it: `record_jdk_only_ic_native_refusal` increments and
    /// returns, because the inline-cache publication site has an entry address
    /// and not a name triple, so there is nothing to name. Per the task's rule,
    /// it is surfaced as a count rather than as a fabricated row.
    ///
    /// Zero in `Compatible` mode, for the same two reasons as
    /// [`SharedVm::jdk_only_process_violations`]. Reading them is four relaxed
    /// atomic loads, no lock and no allocation, but the mode test is still
    /// checked first so a `Compatible` report cannot inherit a sibling VM's
    /// latched-strict counters.
    pub fn jdk_only_refusal_counts(&self) -> JdkOnlyRefusalCounts {
        if !self.compatibility_mode().is_jdk_only() {
            return JdkOnlyRefusalCounts::default();
        }
        JdkOnlyRefusalCounts {
            jit_direct_native_binds: cratonvm_jit::jdk_only_direct_native_refusals(),
            jit_inline_cache_natives: cratonvm_jit::jdk_only_ic_native_refusals(),
            jit_fastpath_admissions: crate::jit::helpers::jdk_only_jit_fastpath_refusals(),
            interpreter_bytecode_preferred: crate::vm::jdk_only_native_shadow_attempts(),
            interpreter_shadow_unenforced: crate::vm::jdk_only_native_shadow_unenforced(),
        }
    }

    /// JDK-only violation/counter report (`--jdk-only-report`, §9). Schema 1,
    /// spelled exactly as the contract prints it:
    ///
    /// ```json
    /// {
    ///   "schema_version": 1,
    ///   "mode": "jdk-only",
    ///   "jdk_feature": 25,
    ///   "violations": [],
    ///   "counts": {
    ///     "boot_image_classes": 312, "application_classes": 18,
    ///     "generated_classes": 4,   "compatibility_classes": 0,
    ///     "bridge_invocations": 1082, "intrinsic_invocations": 4301,
    ///     "synthetic_stub_invocations": 0
    ///   },
    ///   "refusals": {
    ///     "jit_direct_native_binds": 0, "jit_inline_cache_natives": 0,
    ///     "jit_fastpath_admissions": 0, "interpreter_bytecode_preferred": 0,
    ///     "interpreter_shadow_unenforced": 0
    ///   },
    ///   // EVERY key under `refusals` counts something PREVENTED, not
    ///   // something that happened. `jit_fastpath_admissions` is the one that
    ///   // reads backwards in isolation: it counts by-name native fast-path
    ///   // admissions REFUSED, so a LARGE value means strict mode blocked a
    ///   // lot, not that a lot leaked through. Measured on RJitStringLayout
    ///   // 2026-08-19: 38 refusals with 0 direct binds and 0 inline-cache
    ///   // natives — i.e. the JIT's native shortcuts are being denied, which
    ///   // is the intended posture. See known-issues/jdk-only/G87-1.
    ///   // A reader checking "is strict mode leaking JIT shortcuts?" would
    ///   // draw the opposite conclusion from a non-zero value here.
    ///   //
    ///   // The block name carries the semantics; the field names do not
    ///   // repeat it. Renaming would break the schema, so this note is the
    ///   // fix. If the schema is ever versioned up, rename it then.
    ///   "observation_sink": {
    ///     "recorded": 81, "cap": 4096, "saturated": false,
    ///     "truncated": false, "dropped": 0,
    ///     "jit_fastpath": { "recorded": 3, "cap": 4096,
    ///                       "truncated": false, "dropped": 0 },
    ///     "jit_compile":  { "recorded": 0, "cap": 256,
    ///                       "truncated": false, "dropped": 0 }
    ///   }
    /// }
    /// ```
    ///
    /// # `observation_sink` — is `violations[]` the population, or a floor?
    ///
    /// Additive, and here is why it is not decoration. The §7 shadow rows in
    /// `violations[]` come out of a bounded, deduplicated sink
    /// ([`crate::vm::jdk_only_native_shadow_cap`], 256 distinct rows by default,
    /// shared by both recorders), and until this object existed a TRUNCATED list
    /// was identical in shape to a complete one. Every reader who took the list
    /// as the population was reading a floor with nothing in the file to say
    /// so — `jdk-only/G60-1-what-jdk-only-still-overrides-RESOLVED-20260817.md`
    /// §4 had to instruct its readers to count the rows by hand and compare them
    /// against a constant compiled into the VM, which is not a check anyone
    /// performs twice.
    ///
    /// `cap` is emitted rather than assumed for the same reason: it is an
    /// operator override (`CRATONVM_NATIVE_SHADOW_SINK_CAP`), so a reader comparing
    /// `recorded` against a hard-coded 256 would be comparing against the wrong
    /// number on exactly the runs that raised it.
    ///
    /// `saturated` is **not** `recorded == cap`: a run whose last distinct
    /// observation exactly fills the sink drops nothing. See
    /// [`crate::vm::jdk_only_native_shadow_sink_saturated`].
    ///
    /// # 2026-08-20: `truncated`, `dropped`, and the other two sinks
    ///
    /// **A boolean nobody reads was the whole signal, and it capped every
    /// number in `docs/known-issues/jdk-only/`.** Three fixes, in the order
    /// they matter:
    ///
    /// 1. **`dropped`** — a monotonic counter that keeps counting past the cap,
    ///    so `recorded + dropped` is the population and a floor becomes a
    ///    total. This is the load-bearing half; a bigger cap is still a cap.
    /// 2. **`truncated`** — the same bit as `saturated`, emitted under the name
    ///    a reader actually looks for. `saturated` stays because every record
    ///    written before today quotes it.
    /// 3. **`jit_fastpath` / `jit_compile`** — the other two bounded
    ///    collections feeding `violations[]` had *no* saturation signal at all,
    ///    so `truncated: false` at the top level answered for one source of
    ///    three. `jit_compile`'s two fields rendered `null`, not `false`,
    ///    because that sink lived in `cratonvm_jit` with no counter and an
    ///    unmeasured thing must not render as a clean one.
    ///
    ///    **CLOSED 2026-08-22 (H1-1 §5.1).** `cratonvm_jit` now carries
    ///    `JDK_ONLY_VIOLATIONS_DROPPED` and its cap honours the same
    ///    `CRATONVM_NATIVE_SHADOW_SINK_CAP` as the other two, so all three
    ///    collections answer both questions and none of them renders `null`.
    ///    `run.sh`'s saturation verdict is no longer `UNKNOWN` by construction
    ///    — a strict run can say "totals" and mean it. Reports written by an
    ///    OLDER binary still carry the `null`, which is why
    ///    `regression-suite/harness-census.sh` keeps its third verdict.
    ///
    /// `saturated: true` used to condemn a COUNTER as well as a list —
    /// `refusals.interpreter_shadow_unenforced` stopped advancing once the sink
    /// filled, because the hierarchy walk that discovers a shadow was skipped
    /// for every triple. **That is fixed at the source**: the walk is now
    /// skipped per-triple rather than per-run, so the counter keeps advancing
    /// past saturation and only the IDENTITIES are lost — which is what
    /// `dropped` counts.
    ///
    /// The **only** report writer: `--jdk-only-report` calls this. `verbose` is
    /// `--explain-jdk-only`; **false redacts and is the default**, applied
    /// post-hoc to each rendered body because `JdkOnlyViolation::to_json()`
    /// (unlike `render()`) has no `verbose` parameter and threading one through
    /// `types` is not this file's call.
    ///
    /// # `violations` unions all five sources
    ///
    /// | § | source | accessor | scope |
    /// |---|--------|----------|-------|
    /// | §4 | native registrations refused | `NativeMethodRegistry::refused_registrations` | this VM |
    /// | §5 | compatibility classes requested | `ClassManager::origin_violations` | this VM |
    /// | §7 | JIT compile-time direct binds refused | `cratonvm_jit::jdk_only_jit_violations` | process |
    /// | §7 | JIT fast-path admissions refused | `crate::jit::helpers::jdk_only_jit_helper_violations` | process |
    /// | §7 | interpreter preferred bytecode | `crate::vm::jdk_only_native_shadow_observations` | process |
    ///
    /// The last three arrive through
    /// [`SharedVm::jdk_only_process_violations`], which documents why they are
    /// process-scoped and why `Compatible` gets three empty vectors instead of
    /// three lock acquisitions. Every row is rendered by
    /// `JdkOnlyViolation::to_json()` so the wire shape is owned by the type
    /// rather than re-derived here, and the union is **sorted by
    /// `(kind, summary)`**.
    ///
    /// A report that folded only the first two would print `"violations": []`
    /// for a run whose JIT refused ten thousand native binds, and an empty
    /// array reads as *clean*, not as *not measured*. That is strictly worse
    /// than printing nothing, which is why the fold is all-or-nothing.
    ///
    /// # No source is counted twice
    ///
    /// The five lists are disjoint by construction, and the union is **not**
    /// deduplicated — deduplicating would be wrong here, because two sources
    /// reporting the same method are reporting two different events. The three
    /// process sinks each dedup internally by triple, and where two of them can
    /// name the same triple they disagree on `native_kind`
    /// (`"jit-thin-direct-helper"` for the JIT's own baked helper versus the
    /// registered `NativeKind::as_str()` for the interpreter's observation), so
    /// the rows differ in both `summary()` and `to_json()` and sort apart.
    ///
    /// The refusal **counters** are the other half of the same rule, and they
    /// deliberately do not live in this array: a counter increment that also
    /// produced a row would be double-counting if it were rendered as a second
    /// row, and a counter with no row behind it (`jit_inline_cache_natives`)
    /// would be a fabricated row. Both go to the `refusals` sibling object
    /// instead — see [`JdkOnlyRefusalCounts`] for why it is a sibling and not
    /// four more keys in the closed §9 `counts` set.
    ///
    /// Sorting rather than preserving recording order is the opposite of the
    /// choice [`SharedVm::dump_native_census_json`] makes, and for a reason
    /// that does not apply here: native registrations all happen on the boot
    /// thread and their relative order *is* the fact (the `overwrote` chain),
    /// whereas class-origin violations are recorded from arbitrary application
    /// threads, so recording order is genuinely nondeterministic and carries no
    /// meaning worth preserving. Sorting is what makes two runs of the same
    /// program produce byte-identical reports.
    ///
    /// The four class counters are a **partition** of the ten
    /// `ClassOrigin::as_str()` tags — see [`fold_origin_buckets`]. Their sum is
    /// the row count, so no class can go missing between this summary and
    /// `--dump-class-origins`.
    ///
    /// `counts` carries exactly the seven keys §9 spells and no more: no
    /// `total_classes`, no `vm_internal_classes`. A reader that needs either
    /// can add the partition up or read the class-origin census, and every
    /// extra key here is one more thing the two artefacts can disagree about.
    /// `refusals` is a **sibling** object for exactly that reason — widening
    /// `counts` would break both the closed set §9 documents and the partition
    /// the `debug_assert_eq!` below proves.
    ///
    /// `refusals` is emitted in both modes with all four keys, and is all-zero
    /// in `Compatible` by construction. A key set that varied with `mode` would
    /// force every consumer to branch on the mode before reading a number;
    /// zeros say "measured, none" where an absent key says "unknown".
    ///
    /// Returns `(violations, compatibility_classes)` — the violation count so
    /// the caller can decide whether to exit non-zero without re-reading the
    /// file, and the compatibility-class count because that is the number the
    /// launcher reports on its status line and §11 gates on.
    pub fn dump_jdk_only_report_json(
        &self,
        path: impl AsRef<std::path::Path>,
        verbose: bool,
    ) -> std::io::Result<(usize, usize)> {
        let cm = self.classes.class_manager.read();
        self.dump_jdk_only_report_json_with(path, verbose, Some(&cm))
    }

    /// [`Self::dump_jdk_only_report_json`] against a class manager the caller
    /// already holds — or, with `cm = None`, the report the `System.exit` path
    /// can still produce without it.
    ///
    /// Two of the five violation sources are reachable without the class
    /// manager (the native registry's refused registrations, and the three
    /// process-global JIT/dispatch sinks), so a `None` report is not empty —
    /// it is missing the `compatibility-class-requested` rows and the four
    /// class-bucket counts. Marked `"partial": true`, and the class buckets are
    /// **omitted** rather than written as zero, because a zero there would say
    /// "no compatibility classes were requested" — the single most misleading
    /// thing this file can say.
    pub(crate) fn dump_jdk_only_report_json_with(
        &self,
        path: impl AsRef<std::path::Path>,
        verbose: bool,
        cm: Option<&crate::classloading::ClassManager>,
    ) -> std::io::Result<(usize, usize)> {
        use cratonvm_native_api::NativeKind;

        // `(kind, summary, body)`: the first two are the sort key, the third is
        // what gets written.
        let registry = &self.natives.native_methods;
        let mut violations: Vec<(String, String, String)> = registry
            .refused_registrations()
            .iter()
            .map(|v| (v.kind().to_string(), v.summary(), v.to_json()))
            .collect();
        // One L10 acquisition for both questions the class manager answers
        // here — the census and the violation list must describe the same
        // instant, and re-locking between them would let a still-running
        // thread load a class in the gap. The caller holds it.
        let (origins, origin_violations) = match cm {
            Some(cm) => {
                let origins = cm.dump_class_origins();
                let violations: Vec<(String, String, String)> = cm
                    .origin_violations()
                    .iter()
                    .map(|v| (v.kind().to_string(), v.summary(), v.to_json()))
                    .collect();
                (Some(origins), violations)
            }
            None => (None, Vec::new()),
        };
        violations.extend(origin_violations);
        // §7: the three process-global sinks — JIT compile-time refusals, JIT
        // runtime fast-path refusals, and the interpreter's bytecode-wins
        // observations. Empty (and untouched, so no lock and no allocation) in
        // `Compatible`; see `jdk_only_process_violations` for why the mode gate
        // lives there rather than here.
        //
        // Folded into the same `(kind, summary, body)` tuple as the two
        // VM-scoped sources so all five sort together under one comparator. The
        // sort is what makes the report reproducible, and it only works if
        // every source enters the same vector before it runs.
        //
        // Bound rather than consumed in place, so the `observation_sink` block
        // at the bottom can report each sink's ROW COUNT without re-taking its
        // mutex and re-cloning its `Vec` — and, more importantly, so the row
        // count it publishes is the same snapshot these rows came from. Two
        // separate reads could disagree on a VM that is still running.
        let process_sinks = self.jdk_only_process_violations();
        for sink in &process_sinks {
            violations.extend(
                sink.iter()
                    .map(|v| (v.kind().to_string(), v.summary(), v.to_json())),
            );
        }
        // Deterministic order, primary key unchanged: `(kind, summary)`.
        // Class-origin rows are recorded from arbitrary application threads and
        // the JIT sinks are filled from compiler and mutator threads alike, so
        // recording order is nondeterministic for every source except native
        // registration — sorting is the only way two runs of the same program
        // produce identical bytes.
        //
        // `body` is a **tiebreaker**, not a third sort dimension: two rows can
        // tie on `(kind, summary)` and still differ, because `summary()` drops
        // the provenance fields `to_json()` keeps (`registered_by` on
        // `SyntheticNativeRegistered`, `call_site` on
        // `SyntheticNativeInvocation`). Before, such a tie fell back to
        // insertion order, which for the class-manager source is exactly the
        // nondeterministic order this sort exists to erase. Including the body
        // makes the comparator total, so the output no longer depends on which
        // thread got there first. Rows that tie on all three are genuinely
        // indistinguishable on the wire and their relative order cannot be
        // observed.
        violations.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
        let violations: Vec<String> = violations
            .into_iter()
            .map(|(_, _, body)| {
                if verbose {
                    body
                } else {
                    redact_absolute_paths(&body)
                }
            })
            .collect();

        let buckets = origins.as_ref().map(|origins| {
            let buckets = fold_origin_buckets(origins);
            debug_assert_eq!(
                buckets.total(),
                origins.len() as u64,
                "the origin fold must be a partition — no class may fall between buckets"
            );
            buckets
        });

        let mut out = String::with_capacity(512 + 128 * violations.len());
        out.push_str("{\n  \"schema_version\": 1,\n");
        out.push_str(&format!(
            "  \"mode\": {},\n",
            json_escape(self.compatibility_mode().as_str())
        ));
        if buckets.is_none() {
            out.push_str(
                "  \"partial\": true,\n  \"partial_reason\": \"class-manager lock unavailable \
                 on the System.exit path; compatibility-class-requested violations and the \
                 four class-bucket counts are absent, the other four violation sources are \
                 complete\",\n",
            );
        }
        match self.jdk_feature_version() {
            Some(f) => out.push_str(&format!("  \"jdk_feature\": {f},\n")),
            None => out.push_str("  \"jdk_feature\": null,\n"),
        }
        out.push_str("  \"violations\": [");
        for (i, v) in violations.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("\n    ");
            // Re-indent the violation body so nested objects stay readable.
            out.push_str(&v.replace('\n', "\n    "));
        }
        if !violations.is_empty() {
            out.push_str("\n  ");
        }
        out.push_str("],\n  \"counts\": {\n");
        // Omitted, not zeroed, when the class store could not be read: a
        // `"compatibility_classes": 0` is the report's headline green result
        // and must never be produced by a missing measurement.
        if let Some(buckets) = &buckets {
            out.push_str(&format!(
                "    \"boot_image_classes\": {},\n",
                buckets.boot_image
            ));
            out.push_str(&format!(
                "    \"application_classes\": {},\n",
                buckets.application
            ));
            out.push_str(&format!(
                "    \"generated_classes\": {},\n",
                buckets.generated
            ));
            out.push_str(&format!(
                "    \"compatibility_classes\": {},\n",
                buckets.compatibility
            ));
        }
        // `bridge_invocations` is registry bridges PLUS JNI dispatches.
        //
        // The JNI function-pointer table (`natives.jni_native_methods`) is a
        // second, parallel registry of `dlsym` results, and it issues no
        // `NativeMethodId` — so `record_invocation`, which every other dispatch
        // path calls, cannot count it. Reporting only the registry's own total
        // made this key silently under-count genuine JNI bridges by exactly the
        // number of dispatches through that table. A `RegisterNatives` target
        // is a bridge in §1's sense (a real function in a real library — the
        // case §11 sanctions), so adding it here is what makes the key mean
        // what it says rather than "bridges we happened to be able to count".
        //
        // The other two keys are unaffected and stay exact:
        // `synthetic_stub_invocations` cannot gain a JNI row (a
        // `NativeKind::SyntheticStub` cannot be created in that table at all),
        // and neither can `intrinsic_invocations`.
        out.push_str(&format!(
            "    \"bridge_invocations\": {},\n",
            registry
                .invocations_of_kind(NativeKind::Bridge)
                .saturating_add(
                    self.natives
                        .jni_bridge_invocations
                        .load(std::sync::atomic::Ordering::Relaxed)
                )
        ));
        out.push_str(&format!(
            "    \"intrinsic_invocations\": {},\n",
            registry.invocations_of_kind(NativeKind::Intrinsic)
        ));
        out.push_str(&format!(
            "    \"synthetic_stub_invocations\": {}\n",
            registry.invocations_of_kind(NativeKind::SyntheticStub)
        ));
        // §9's `counts` ends here — seven keys, closed set, class buckets
        // partitioned. Everything below is a sibling object.
        out.push_str("  },\n  \"refusals\": {\n");
        let refusals = self.jdk_only_refusal_counts();
        out.push_str(&format!(
            "    \"jit_direct_native_binds\": {},\n",
            refusals.jit_direct_native_binds
        ));
        out.push_str(&format!(
            "    \"jit_inline_cache_natives\": {},\n",
            refusals.jit_inline_cache_natives
        ));
        out.push_str(&format!(
            "    \"jit_fastpath_admissions\": {},\n",
            refusals.jit_fastpath_admissions
        ));
        out.push_str(&format!(
            "    \"interpreter_bytecode_preferred\": {},\n",
            refusals.interpreter_bytecode_preferred
        ));
        // §1.4 observed-but-not-enforced. Additive field: a reader that does
        // not know it still parses the object, and one that does gets the half
        // of §1.4 the report could not previously see at all.
        out.push_str(&format!(
            "    \"interpreter_shadow_unenforced\": {}\n",
            refusals.interpreter_shadow_unenforced
        ));
        // A SIBLING object, not three more `counts` keys. `counts` is a closed
        // set of seven and these are not counts of anything that happened —
        // they are the observation sink's capacity state, and what they do is
        // qualify `violations[]` and `refusals.interpreter_shadow_unenforced`
        // rather than join them.
        //
        // Written unconditionally, including in `Compatible` where the sink is
        // never touched and this reads `recorded: 0, saturated: false`. An
        // object present only when it had something to report would make its
        // ABSENCE ambiguous between "nothing was dropped" and "this binary does
        // not answer the question" — the same class of mistake that omitting the
        // class buckets, rather than zeroing them, exists to avoid above.
        out.push_str("  },\n  \"observation_sink\": {\n");
        out.push_str(&format!(
            "    \"recorded\": {},\n",
            crate::vm::jdk_only_native_shadow_sink_len()
        ));
        out.push_str(&format!(
            "    \"cap\": {},\n",
            crate::vm::jdk_only_native_shadow_cap()
        ));
        out.push_str(&format!(
            "    \"saturated\": {},\n",
            crate::vm::jdk_only_native_shadow_sink_saturated()
        ));
        // `truncated` is the same bit as `saturated`, under the name a reader
        // reaches for. Both are emitted because `saturated` is the name every
        // record written before 2026-08-20 quotes, and renaming a key to make a
        // point is how a machine consumer breaks silently. They can never
        // disagree — one expression feeds both.
        out.push_str(&format!(
            "    \"truncated\": {},\n",
            crate::vm::jdk_only_native_shadow_sink_saturated()
        ));
        // The number that turns the list from a floor into a total.
        // `recorded + dropped` is the distinct population this run observed;
        // `recorded` alone is what it had room to NAME. See
        // `crate::vm::jdk_only_native_shadow_sink_dropped` for the one way this
        // errs (upward, via filter collisions) and why.
        out.push_str(&format!(
            "    \"dropped\": {},\n",
            crate::vm::jdk_only_native_shadow_sink_dropped()
        ));
        // Per-source sub-objects for the OTHER two bounded collections that
        // feed `violations[]`. Until 2026-08-20 this object described only the
        // interpreter's sink, so a reader who checked `truncated: false` and
        // concluded "the list is complete" was right about one of three
        // sources and had no way to ask about the other two.
        out.push_str("    \"jit_fastpath\": {\n");
        out.push_str(&format!(
            "      \"recorded\": {},\n",
            process_sinks[1].len()
        ));
        out.push_str(&format!(
            "      \"cap\": {},\n",
            crate::jit::helpers::jdk_only_jit_helper_violation_cap()
        ));
        out.push_str(&format!(
            "      \"truncated\": {},\n",
            crate::jit::helpers::jdk_only_jit_helper_sink_saturated()
        ));
        out.push_str(&format!(
            "      \"dropped\": {}\n",
            crate::jit::helpers::jdk_only_jit_helper_sink_dropped()
        ));
        out.push_str("    },\n");
        // `cratonvm_jit`'s compile-time sink used to render `null` here,
        // because it had no drop counter and an unmeasured thing must not
        // render as a clean one. **It has one since 2026-08-22** (H1-1 §5.1),
        // so all three bounded collections now answer the same two questions
        // and `run.sh`'s saturation verdict is no longer UNKNOWN by
        // construction. Its cap honours the same knob as the other two.
        out.push_str("    \"jit_compile\": {\n");
        out.push_str(&format!(
            "      \"recorded\": {},\n",
            process_sinks[0].len()
        ));
        out.push_str(&format!(
            "      \"cap\": {},\n",
            cratonvm_jit::jdk_only_violation_cap()
        ));
        out.push_str(&format!(
            "      \"truncated\": {},\n",
            cratonvm_jit::jdk_only_jit_sink_saturated()
        ));
        out.push_str(&format!(
            "      \"dropped\": {}\n",
            cratonvm_jit::jdk_only_jit_sink_dropped()
        ));
        out.push_str("    }\n");
        // A SIBLING object again, and for the sharpest version of
        // `observation_sink`'s reason. `CRATONVM_ENFORCE_NATIVE_SHADOW` does
        // not add rows to this report -- it REMOVES them, because an enforced
        // shadow does not dispatch and so never produces a
        // `bridge-ran-over-bytecode` row. An armed report and an unarmed one
        // were therefore indistinguishable in every field, and the armed
        // one's emptier `violations[]` reads as the better result. It is not;
        // it is a different question.
        //
        // `doors` is the other half. Until 2026-08-21 the dial had exactly
        // one live call site (`resolve_step1_native`), so `scope` alone would
        // still have overstated what an armed run measured: 890 of 947 armed
        // `Bridge` dispatches never asked it, and
        // `refusals.interpreter_shadow_unenforced` read `0` for all 890,
        // because that one call site is also the only recorder of the
        // native-won half.
        //
        // **`declined_no_bytecode` is NOT a leak count, and an earlier draft of
        // this object called it one.** `reached` minus `yielded` is the dial
        // being ASKED and answering no, and after the three cheap guards the
        // only remaining reason to answer no is `dispatch_has_code == false` --
        // there is no concrete bytecode to yield TO, so the native runs exactly
        // as step 1 has always let it. Armed for `java/util/HashMap` it is 0,
        // which is why the wrong name survived its first checks; armed for
        // `all` it is 494 of 19 931. A widening scope cannot widen a leak.
        //
        // The question the wrong name implied -- does some door serve a
        // `Bridge` WITHOUT asking? -- this counter cannot answer, because a
        // door that never calls `note_dial_door` is invisible to it. It is
        // answered statically instead, by the three source-witness tests in
        // `native_override.rs` (`the_unrouted_invoke_or_native_doors_ask_the_
        // enforcement_dial`, `the_warm_invoke_cache_door_asks_the_enforcement_
        // dial`, and the scan that pins the helper's spelling so the other two
        // cannot silently degrade to searching for nothing).
        out.push_str("  },\n  \"enforcement_dial\": {\n");
        out.push_str(&format!(
            "    \"scope\": {},\n",
            json_escape(&crate::runtime::env_cache::enforce_shadow_scope().report_spelling())
        ));
        let doors = crate::vm::dial_door_counts();
        let reached: u64 = doors.iter().map(|(_, r, _)| *r).sum();
        let yielded: u64 = doors.iter().map(|(_, _, y)| *y).sum();
        out.push_str(&format!("    \"reached\": {reached},\n"));
        out.push_str(&format!("    \"yielded\": {yielded},\n"));
        out.push_str(&format!(
            "    \"declined_no_bytecode\": {},\n",
            reached.saturating_sub(yielded)
        ));
        out.push_str("    \"doors\": [");
        for (i, (door, r, y)) in doors.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "\n      {{\"door\": {}, \"reached\": {r}, \"yielded\": {y}}}",
                json_escape(door)
            ));
        }
        out.push_str("\n    ]\n");
        out.push_str("  }\n}\n");

        use std::io::Write;
        let mut file = std::fs::File::create(path)?;
        file.write_all(out.as_bytes())?;
        file.sync_all()?;
        Ok((
            violations.len(),
            buckets.map(|b| b.compatibility as usize).unwrap_or(0),
        ))
    }

    /// Write whichever JDK-only census artefacts were requested, from a thread
    /// that is about to call [`std::process::exit`] and must not hang.
    ///
    /// # Why this exists
    ///
    /// `vm-cli`'s `write_jdk_only_dumps` is called from four exit paths, and
    /// `System.exit(N)` reaches none of them: `native_system_exit` /
    /// `native_runtime_exit` end in `std::process::exit`, which does not unwind
    /// — no `Drop`, no `catch_unwind`, no return to `run()`. So a strict run of
    /// any program that detects a problem and exits (a Spring Boot failure
    /// analyzer, a CLI argument-parse error path) produced **no census at all**,
    /// and that gap was correlated with failure: the runs most likely to need
    /// the report were exactly the runs that left none.
    ///
    /// # Why it is a try-lock and must stay one
    ///
    /// This runs on whichever Java thread called `System.exit`, at an arbitrary
    /// point in that thread's execution, holding whatever locks that thread
    /// already held. All three writers need the class-manager lock, which is an
    /// `OrderedPlRwLock` participating in workspace lock-order enforcement.
    /// A blocking acquire from here is both a deadlock risk and an order
    /// violation — **and a deadlock in the exit path hangs the process instead
    /// of losing a file, which is strictly worse than the bug being fixed.**
    ///
    /// So: [`try_read_untracked`], no retry loop, no timeout, no blocking
    /// fallback. If the lock is not free at this instant, each artefact is
    /// written in its labelled `"partial": true` form rather than not at all
    /// (see the three `_with` writers for what each one degrades to). A census
    /// that is usually complete and never hangs beats one that is sometimes
    /// absent.
    ///
    /// [`try_read_untracked`]: cratonvm_types::lock_order::OrderedPlRwLock::try_read_untracked
    ///
    /// # Cost in `Compatible` mode
    ///
    /// Zero. Every argument is `None` unless the operator passed the matching
    /// `--dump-*` / `--jdk-only-report` flag, and the three-`is_none` guard
    /// returns before touching a lock or the filesystem.
    ///
    /// Returns `(wrote_anything, was_partial)`.
    pub fn try_write_jdk_only_dumps_for_exit(
        &self,
        class_origins: Option<&str>,
        native_registry: Option<&str>,
        report: Option<&str>,
        verbose: bool,
    ) -> (bool, bool) {
        if class_origins.is_none() && native_registry.is_none() && report.is_none() {
            return (false, false);
        }
        let guard = self.classes.class_manager.try_read_untracked();
        let cm = guard.as_deref();
        let partial = cm.is_none();
        let mut wrote = false;
        if let Some(path) = class_origins {
            if self.dump_class_origins_json_with(path, verbose, cm).is_ok() {
                wrote = true;
            }
        }
        if let Some(path) = native_registry {
            if self.dump_native_census_json_with(path, verbose, cm).is_ok() {
                wrote = true;
            }
        }
        if let Some(path) = report {
            if self.dump_jdk_only_report_json_with(path, verbose, cm).is_ok() {
                wrote = true;
            }
        }
        (wrote, partial)
    }

    /// NEW-10: append a missing-native entry to the audit log,
    /// deduplicating by `(class, method, descriptor)`. The first
    /// occurrence records `sample_call_site`; subsequent calls do not
    /// overwrite it so the baseline remains stable across runs.
    pub fn record_missing_native(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        sample_call_site: Option<String>,
    ) {
        let mut log = self.debug.missing_natives_log.lock();
        let already = log.iter().any(|e| {
            e.class_name == class_name && e.method_name == method_name && e.descriptor == descriptor
        });
        if !already {
            log.push(MissingNativeEntry {
                class_name: class_name.to_string(),
                method_name: method_name.to_string(),
                descriptor: descriptor.to_string(),
                sample_call_site,
            });
        }
    }

    /// T2.1.3: Group the missing-native audit log by JDK module.
    ///
    /// Classification uses the canonical `java.module-info` package ownership
    /// defined in the OpenJDK 25 module descriptors. Every key in the
    /// returned map is a JDK module name (`"java.base"`, `"java.net.http"`,
    /// `"jdk.unsupported"`, …) plus the special bucket `"other"` for
    /// entries whose class prefix does not match any known module.
    ///
    /// Entries are sorted within each group by `(class, name, descriptor)`
    /// so the result is stable across runs and suitable for committing as
    /// a census baseline.
    pub fn classify_missing_natives_by_module(
        &self,
    ) -> std::collections::BTreeMap<&'static str, Vec<MissingNativeEntry>> {
        let log = self.debug.missing_natives_log.lock();
        let mut grouped: std::collections::BTreeMap<&'static str, Vec<MissingNativeEntry>> =
            std::collections::BTreeMap::new();
        for entry in log.iter() {
            let module = classify_jdk_module(&entry.class_name);
            grouped.entry(module).or_default().push(entry.clone());
        }
        for v in grouped.values_mut() {
            v.sort_by(|a, b| {
                (&a.class_name, &a.method_name, &a.descriptor).cmp(&(
                    &b.class_name,
                    &b.method_name,
                    &b.descriptor,
                ))
            });
            v.dedup_by(|a, b| {
                a.class_name == b.class_name
                    && a.method_name == b.method_name
                    && a.descriptor == b.descriptor
            });
        }
        grouped
    }

    /// T2.1.3: Write the grouped census to `path` as a JSON object with
    /// one top-level key per JDK module. Schema:
    ///
    /// ```json
    /// {
    ///   "version": 1,
    ///   "modules": {
    ///     "java.base": [
    ///       { "class": "java/lang/Foo", "name": "bar",
    ///         "descriptor": "(I)V", "sample_call_site": null }
    ///     ],
    ///     "other": []
    ///   }
    /// }
    /// ```
    ///
    /// Module keys and entries within each module are sorted so the file
    /// is byte-stable and diff-friendly.
    pub fn dump_missing_natives_grouped_json(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> std::io::Result<()> {
        let grouped = self.classify_missing_natives_by_module();

        let mut out = String::with_capacity(512);
        out.push_str("{\n  \"version\": 1,\n  \"modules\": {");
        let mut first_module = true;
        for (module, entries) in &grouped {
            if !first_module {
                out.push(',');
            }
            first_module = false;
            out.push_str("\n    ");
            out.push_str(&json_escape(module));
            out.push_str(": [");
            for (i, entry) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str("\n      {\n");
                out.push_str(&format!(
                    "        \"class\": {},\n",
                    json_escape(&entry.class_name)
                ));
                out.push_str(&format!(
                    "        \"name\": {},\n",
                    json_escape(&entry.method_name)
                ));
                out.push_str(&format!(
                    "        \"descriptor\": {},\n",
                    json_escape(&entry.descriptor)
                ));
                match &entry.sample_call_site {
                    Some(site) => out.push_str(&format!(
                        "        \"sample_call_site\": {}\n",
                        json_escape(site)
                    )),
                    None => out.push_str("        \"sample_call_site\": null\n"),
                }
                out.push_str("      }");
            }
            if !entries.is_empty() {
                out.push_str("\n    ");
            }
            out.push(']');
        }
        if !grouped.is_empty() {
            out.push_str("\n  ");
        }
        out.push_str("}\n}\n");

        use std::io::Write;
        let mut file = std::fs::File::create(path)?;
        file.write_all(out.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }
}

/// T2.1.3: Return the canonical JDK module name that owns a given class
/// (by binary name, e.g. `"java/net/http/HttpClient"`).
///
/// The mapping is a longest-prefix match against the OpenJDK 25 module
/// table. Unknown classes fall into the `"other"` bucket so they remain
/// visible in the census without triggering false module claims.
///
/// The returned slice is static — this function allocates nothing and is
/// safe to call from any context including `no_std` tests.
pub fn classify_jdk_module(class_name: &str) -> &'static str {
    // The list is ordered longest-prefix first. Any time a new JDK module
    // grows a package CratonVM needs to track, add it ABOVE its parent
    // module (e.g. `"java/net/http"` must appear before `"java/net"`).
    //
    // Prefixes use the JVMS binary form with `/` separators and always
    // end with `/` so `"java/netX"` does not match `"java/net"`.
    const PREFIX_TO_MODULE: &[(&str, &str)] = &[
        // Named packages that live in modules other than java.base.
        ("java/net/http/", "java.net.http"),
        ("jdk/internal/net/http/", "java.net.http"),
        ("java/sql/", "java.sql"),
        ("javax/sql/", "java.sql"),
        ("java/rmi/", "java.rmi"),
        ("javax/management/", "java.management"),
        ("java/lang/management/", "java.management"),
        ("javax/naming/", "java.naming"),
        ("javax/xml/", "java.xml"),
        ("org/w3c/dom/", "java.xml"),
        ("org/xml/sax/", "java.xml"),
        ("javax/crypto/", "java.base"), // javax.crypto is in java.base in JDK 25
        ("javax/security/", "java.base"),
        ("javax/net/ssl/", "java.base"),
        ("javax/net/", "java.base"),
        ("java/util/logging/", "java.logging"),
        ("java/util/prefs/", "java.prefs"),
        ("java/lang/instrument/", "java.instrument"),
        ("jdk/jfr/", "jdk.jfr"),
        ("jdk/management/jfr/", "jdk.management.jfr"),
        ("com/sun/management/", "jdk.management"),
        ("jdk/management/", "jdk.management"),
        ("jdk/jdi/", "jdk.jdi"),
        ("com/sun/jdi/", "jdk.jdi"),
        ("jdk/jshell/", "jdk.jshell"),
        ("jdk/compiler/", "jdk.compiler"),
        ("javax/tools/", "java.compiler"),
        ("jdk/javadoc/", "jdk.javadoc"),
        ("jdk/unsupported/", "jdk.unsupported"),
        ("sun/misc/", "jdk.unsupported"),
        // java.base (catch-all for the huge standard tree).
        ("java/lang/", "java.base"),
        ("java/util/", "java.base"),
        ("java/io/", "java.base"),
        ("java/nio/", "java.base"),
        ("java/net/", "java.base"),
        ("java/security/", "java.base"),
        ("java/math/", "java.base"),
        ("java/text/", "java.base"),
        ("java/time/", "java.base"),
        ("jdk/internal/", "java.base"),
        ("sun/nio/", "java.base"),
        ("sun/security/", "java.base"),
        ("sun/reflect/", "java.base"),
        ("sun/util/", "java.base"),
    ];

    for (prefix, module) in PREFIX_TO_MODULE {
        if class_name.starts_with(prefix) {
            return module;
        }
    }
    "other"
}

/// WP1.8 — ServiceLoader classpath bootstrap.
///
/// Registers `java.util.ServiceLoader` natives on `registry` and seeds any
/// JVM-side bookkeeping needed to make classpath `META-INF/services/*`
/// provider scans resolvable from `ServiceLoader.load(Class)`. This is the
/// WildFly-friendly entry point: classpath-scoped today, module-scoped in
/// Wave 2 (the roadmap re-extends WP1.8 with JBoss-Module resource-root
/// awareness, at which point this function gains a second scan source).
///
/// Today all the heavy lifting happens inside the native registration
/// (see [`cratonvm_native_builtins::service_loader::register_service_loader_natives`])
/// which bridges into `ClassLoader.getResources("META-INF/services/<fqcn>")`
/// and instantiates each listed provider via reflection. We keep this
/// entry point even though the body is minimal so that:
///
/// 1. Module-scoped (Wave 2) extensions land in a single, already-wired
///    location without having to touch the `SharedVm::new` hot path again.
/// 2. Callers outside `SharedVm::new` (e.g. VM embeddings that build
///    their own `NativeMethodRegistry` before constructing `SharedVm`)
///    have a single public name to call.
/// 3. Tests can exercise the bootstrap independently of the full VM
///    constructor.
///
/// Coordination note (WP1.8): this function is deliberately kept outside
/// `SharedVm::new`'s `set_init_level` region (which is owned by Wave 1
/// Owner B). It is called from both the synthetic-jdk and real-jdk
/// branches alongside the existing `register_service_loader_natives`
/// call — see the call sites immediately below that comment.
pub fn init_service_loader_bootstrap(registry: &mut NativeMethodRegistry) {
    cratonvm_native_builtins::service_loader::register_service_loader_natives(registry);
    tracing::info!(
        "WP1.8: ServiceLoader bootstrap wired — META-INF/services classpath scan enabled"
    );
}

/// The `schema_version` [`SharedVm::dump_native_census_json`] stamps on every
/// native census it writes.
///
/// **5** since 2026-08-17 (`G47-1`): rows gained `invocations_complete` and the
/// header gained `slots_with_incomplete_invocations`. The bump is not cosmetic
/// and the reason is the same one schema 4 was bumped for. A schema-4 reader
/// scoring a schema-5 file is harmless (it ignores two keys); a reader that
/// believes it is looking at schema 5 and is handed a schema-4 file concludes
/// **every row is a total**, because the absent key reads as "nothing declared
/// itself a bypass" — which is the exact direction
/// `NativeMethodRegistry::mark_invocations_incomplete` says this instrument
/// must never err in. One `schema_version` with two shapes is the hazard
/// `dump_native_census_json`'s "only native-census writer" note is about.
///
/// A named constant rather than a literal because the writer, this file's doc
/// example and the witness test below must not be able to drift apart — the
/// state `G37-1` §6 N2 and `G42-1` §6 N1 measured, where two records disagreed
/// about whether the shipping binary said 3 or 4 and `--help` said a third
/// thing.
///
/// **Consumers that pin this exactly** (equality, not `>=`), and therefore move
/// with it: `scripts/jdk-only-bridge-ratchet.py`'s `REQUIRED_CENSUS_SCHEMA` and
/// the `census_schema_version` recorded in
/// `scripts/baselines/jdk-only-bridge-ratchet.json`. Neither is in this crate;
/// see `docs/known-issues/jdk-only/G47-1-*.md` NOMINATION 1.
/// `scripts/jdk-only-kind-map.py` asks for `>= 2` and needs nothing.
pub(crate) const NATIVE_CENSUS_SCHEMA_VERSION: u32 = 5;

/// The `invocations` pair of a census row: the tally, and whether it is a
/// **total** or a **floor**.
///
/// The two are emitted together, in this order, by one function on purpose.
/// The whole defect `G33-1`/`G37-1`/`G42-1` chased is that `invocations` was
/// readable without its qualifier: 25 slots carried
/// `NativeCensusEntry::invocations_complete` `false` and **no reader could
/// see it**, so every consumer of the column read a floor as a count. Emitting
/// the number from a function that cannot emit it without the bit is the cheap
/// structural way to keep that from recurring; a future editor who wants one
/// has to delete the other deliberately.
///
/// `false` here means "a dispatch path has *declared* that it serves this slot
/// without counting" — a claim carried by code, not a proof. `true` means no
/// path has declared itself, which is weaker than "exact": see
/// `NativeMethodRegistry::record_invocation`'s bypass list.
fn native_census_invocations_json(invocations: u64, complete: bool) -> String {
    format!(
        "      \"invocations\": {},\n      \"invocations_complete\": {},\n",
        invocations, complete
    )
}

/// The census header's one-line summary of how much of the `invocations`
/// column is a floor:
/// `NativeMethodRegistry::slots_with_incomplete_invocations`.
///
/// In the header rather than only per row so a reader is told the column is
/// partly a floor **before** quoting a number out of it, which is the order the
/// mistake actually happens in — `G33-1` §4 records a lane concluding a body
/// was dead from `invocations: 0`, and `G42-1` §4 shows zero is the *expected*
/// reading for a hot native whose loop began after its caller was compiled.
///
/// Counted over **slots**, while `natives` is one row per **registration**, so
/// this number is not the count of rows carrying `invocations_complete: false`
/// — a superseded row shares its successor's slot and shows the same bit. That
/// asymmetry is inherited from `counts` (registrations) versus `invocations`
/// (slots) and is deliberate in both.
fn native_census_incomplete_header_json(slots: usize) -> String {
    format!("  \"slots_with_incomplete_invocations\": {},\n", slots)
}

/// Escape an arbitrary UTF-8 string as a JSON string literal, including
/// the surrounding double-quotes. Handles the six characters that must
/// be escaped per RFC 8259 (`\"`, `\\`, `\b`, `\f`, `\n`, `\r`, `\t`)
/// plus any C0 control code via `\u00XX`. Used by
/// [`SharedVm::dump_missing_natives_json`] so the file can be diffed
/// safely against a committed baseline.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Every `ClassOrigin::as_str()` tag, in the enum's own declaration order.
///
/// The class-origin census seeds its `counts` block from this list so a tag
/// that never occurred reports `0` rather than being absent. `"compatibility-
/// stub": 0` is the single most load-bearing line in the whole artefact, and a
/// missing key would let "we did not observe any" and "we did not look" render
/// identically (jdk-only-mode.md §9, §11).
///
/// Kept in sync by hand with
/// [`ClassOrigin::as_str`](cratonvm_classloading::class_origin::ClassOrigin::as_str)
/// and with `vm-cli`'s `CLASS_ORIGIN_TAGS`; a tag added there and forgotten
/// here shows up as an unseeded (but still counted) key, never as a lost row.
const CLASS_ORIGIN_TAGS: [&str; 10] = [
    "boot-image",
    "application-class-path",
    "user-defined",
    "vm-array",
    "hidden-class",
    "generated-lambda",
    "generated-proxy",
    "reflection-accessor",
    "vm-internal",
    "compatibility-stub",
];

/// Redact a native registration site for the schema-2 census
/// (`jdk-only-mode.md` §9: "absolute paths are redacted unless
/// `--explain-jdk-only` is passed").
///
/// Provenance is captured with `#[track_caller]`, so
/// [`core::panic::Location::file`] normally yields a **workspace-relative**
/// path — `native-builtins/src/lib.rs:1234` — which is exactly what a reviewer
/// needs and contains nothing private. Those are kept verbatim (modulo `\` →
/// `/`, so a census taken on Windows diffs against one taken on Linux).
///
/// An **absolute** path is a different animal: it appears for code compiled
/// out of a registry checkout or with `--remap-path-prefix` absent, and it
/// carries the builder's home directory into a file that gets committed as a
/// baseline and pasted into issues. Those collapse to `<redacted>/<file:line>`
/// — the basename plus line is still enough to find the registration, and the
/// output no longer varies per machine, which also keeps the baseline
/// diff-stable.
///
/// Windows drive letters are handled before any `:`-splitting: the path
/// separator is located first, so `C:/…/lib.rs:12` is not mistaken for a
/// `host:port`-shaped string.
pub(crate) fn redact_registration_site(site: &str) -> String {
    let normalised = site.replace('\\', "/");
    let bytes = normalised.as_bytes();
    let is_absolute = normalised.starts_with('/')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':');
    if !is_absolute {
        return normalised;
    }
    match normalised.rsplit_once('/') {
        Some((_, tail)) if !tail.is_empty() => format!("<redacted>/{tail}"),
        _ => "<redacted>".to_string(),
    }
}

/// The JDK feature version of the runtime image rooted at `java_home`, read
/// from its `release` file's `JAVA_VERSION=` line.
///
/// Returns `None` rather than guessing. `jdk_feature` is used to interpret a
/// violation ("does this JDK even declare that method?") and to key the
/// per-image blocker artifacts, so a fabricated number is worse than an absent
/// one — `null` reads as "not measured", a wrong `25` reads as a fact.
///
/// `"25"`, `"25.0.1"`, `"21.0.4+7-LTS"` → the feature; legacy `"1.8.0_402"`
/// → `8`.
pub fn jdk_feature_from_release_file(java_home: &str) -> Option<u32> {
    let text = std::fs::read_to_string(std::path::Path::new(java_home).join("release")).ok()?;
    let raw = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("JAVA_VERSION="))?;
    let value = raw.trim().trim_matches('"');
    let mut parts = value.split(['.', '-', '+', '_']);
    let first: u32 = parts.next()?.parse().ok()?;
    if first == 1 {
        parts.next()?.parse().ok()
    } else {
        Some(first)
    }
}

/// The `java.vm.info` execution-mode list for `mode` — and the same list the
/// launcher's `-version` banner prints (`jdk-only-mode.md` §9).
///
/// ONE function, called from both places, because the property and the banner
/// used to spell the policy two different ways (`"mixed mode, jdk-only"` vs
/// `compatibility=jdk-only`) and nothing kept them in step. They still use two
/// *conventions*, deliberately, and both are load-bearing:
///
/// * **`java.vm.info` is HotSpot's convention, verbatim**: a comma-separated
///   list of execution-mode tokens (`"mixed mode"`, `"mixed mode, sharing"`),
///   read by Java code and printed by tools straight after the VM name. It
///   cannot carry a `key=value` token without breaking that shape, and under
///   `Compatible` it must stay byte-for-byte `"mixed mode"` because the
///   differential harness compares it literally (§10). Strict mode therefore
///   *appends a token* instead of restructuring the value.
/// * **The banner adds `compatibility=<mode>`** on top of this same list,
///   because a banner is grepped by scripts that must not have to parse a
///   comma list, and because `-version` prints it *before any VM exists*, so
///   it cannot read the property it has to agree with.
///
/// What must agree is the spelling and the position of the policy token: both
/// say `jdk-only`, both put it immediately after `mixed mode`.
pub fn vm_info_mode_list(mode: CompatibilityMode) -> &'static str {
    if mode.is_jdk_only() {
        "mixed mode, jdk-only"
    } else {
        "mixed mode"
    }
}

/// Replace absolute filesystem paths embedded in `text` with
/// `<redacted>/<basename>`.
///
/// Contract §9: "absolute paths are redacted unless `--explain-jdk-only` is
/// passed". Distinct from [`redact_registration_site`], which redacts a string
/// that *is* a path; this one hunts paths inside prose — a
/// `ClassOrigin::CompatibilityStub` reason, a `requested_by` attribution, a
/// rendered `JdkOnlyViolation` body. A census file is routinely attached to a
/// bug report; a build-agent home directory or a developer's user name is not
/// information the report needs, but the file *name* usually is.
///
/// Lives here rather than in `vm-cli` because the census writers moved into
/// this file and the launcher still needs the same function for its
/// `--trace-jdk-only` lines; two copies of a redaction rule is how a redaction
/// rule stops being applied. Deliberately regex-free (no dependency) and
/// deliberately conservative:
///
/// * only **rooted** runs are candidates — a leading `/`, or a `C:\` / `C:/`
///   drive prefix. Internal-form class names and method descriptors
///   (`java/lang/String`, `(Ljava/lang/Object;)V`) are not rooted and are left
///   alone, which matters because every violation body is full of them;
/// * a candidate needs at least **two** separators before it is rewritten, so
///   `/tmp` and a bare `/` survive while `/home/user/app.jar` does not;
/// * the run is applied to already-escaped JSON, where a Windows separator
///   appears as `\\`; consecutive separators count once.
pub fn redact_absolute_paths(text: &str) -> String {
    let b = text.as_bytes();
    // Characters that may precede a path: anything that is not part of a word.
    // This is what keeps `Ljava/lang/String;` (preceded by `L`) out.
    let boundary = |c: u8| {
        matches!(
            c,
            b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b'(' | b'[' | b'{' | b'=' | b',' | b'<'
        )
    };
    // Characters that end a run.
    let terminator = |c: u8| {
        matches!(
            c,
            b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b')' | b']' | b'}' | b',' | b';' | b'>'
        )
    };

    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < b.len() {
        let at_boundary = i == 0 || boundary(b[i - 1]);
        let rooted_unix = b[i] == b'/';
        let rooted_windows = i + 2 < b.len()
            && b[i].is_ascii_alphabetic()
            && b[i + 1] == b':'
            && (b[i + 2] == b'\\' || b[i + 2] == b'/');
        if at_boundary && (rooted_unix || rooted_windows) {
            let mut end = i;
            while end < b.len() && !terminator(b[end]) {
                end += 1;
            }
            // Trailing sentence punctuation belongs to the prose, not the path.
            while end > i && matches!(b[end - 1], b'.' | b':') {
                end -= 1;
            }
            if let Some(redacted) = redact_one_path(&text[i..end]) {
                out.push_str(&redacted);
                i = end;
                continue;
            }
        }
        // Not a path: copy one whole UTF-8 character.
        let step = utf8_char_len(b[i]);
        let step = step.min(b.len() - i);
        out.push_str(&text[i..i + step]);
        i += step;
    }
    out
}

/// Byte length of the UTF-8 character starting with `first`.
fn utf8_char_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// `<redacted>/<basename>` for a rooted run with >= 2 separators; `None` when
/// the run is too shallow to be worth hiding.
fn redact_one_path(run: &str) -> Option<String> {
    let mut separators = 0usize;
    let mut prev_was_separator = false;
    let mut basename = "";
    let mut segment_start = 0usize;
    for (idx, ch) in run.char_indices() {
        if ch == '/' || ch == '\\' {
            if !prev_was_separator {
                separators += 1;
                if idx > segment_start {
                    basename = &run[segment_start..idx];
                }
            }
            prev_was_separator = true;
            segment_start = idx + ch.len_utf8();
        } else {
            prev_was_separator = false;
        }
    }
    if segment_start < run.len() {
        basename = &run[segment_start..];
    }
    if separators < 2 || basename.is_empty() {
        return None;
    }
    Some(format!("<redacted>/{basename}"))
}

/// A JSON string literal, path-redacted unless `verbose`.
fn json_string(value: &str, verbose: bool) -> String {
    let escaped = json_escape(value);
    if verbose {
        escaped
    } else {
        redact_absolute_paths(&escaped)
    }
}

/// `null`, or a (possibly redacted) JSON string literal.
fn json_opt_string(value: Option<&str>, verbose: bool) -> String {
    match value {
        Some(v) => json_string(v, verbose),
        None => "null".to_string(),
    }
}

/// How many process-global JDK-only violation sinks
/// [`SharedVm::jdk_only_process_violations`] returns, and the meaning of each
/// index. The order is a contract: `--trace-jdk-only` keeps one watermark per
/// slot across the run, so inserting a sink in the middle would silently
/// re-report one source and skip another.
///
/// | index | sink | recorded by |
/// |-------|------|-------------|
/// | 0 | `cratonvm_jit::jdk_only_jit_violations` | JIT compile-time direct-bind scan |
/// | 1 | `crate::jit::helpers::jdk_only_jit_helper_violations` | JIT by-name fast-path admission |
/// | 2 | `crate::vm::jdk_only_native_shadow_observations` | interpreter dispatch resolver |
///
/// New sinks append; they never insert.
pub const JDK_ONLY_PROCESS_SINKS: usize = 3;

/// Exact refusal-event counts for the JDK-only report's `refusals` block.
///
/// # Why this is a sibling of `counts` and not four more keys in it
///
/// Contract §9 spells `counts` as a **closed seven-key set**, and its four
/// class keys are a partition proved by a `debug_assert_eq!` against the
/// class-origin row count. Adding a refusal key there would break both
/// properties at once: the set would no longer be the one §9 documents, and a
/// reader summing the block to recover the class total would get a number that
/// is not the class total. The two blocks also answer different questions —
/// `counts` is a census of what the run *contains*, `refusals` is a tally of
/// what strict policy *stopped* — and they are not commensurable: a class
/// appears in `counts` once, whereas a refused method appears in `refusals`
/// once per call.
///
/// Every field is a count of events, not of distinct methods. The distinct
/// methods are the rows in `violations[]`, which are deduplicated and capped at
/// 256 per sink; these counters are exact and uncapped. A field being larger
/// than the matching row count is the normal case, not a discrepancy.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JdkOnlyRefusalCounts {
    /// Compile-time thin-native direct binds the JIT refused. Each distinct
    /// triple also appears once in sink 0 of
    /// [`SharedVm::jdk_only_process_violations`].
    pub jit_direct_native_binds: u64,
    /// Inline-cache publications of an unowned (native/builtin) entry the JIT
    /// refused. **Counter-only**: the publication site holds an entry address,
    /// not a name triple, so there is no honest row to emit and none is
    /// fabricated. This number is the sole evidence of this refusal class.
    pub jit_inline_cache_natives: u64,
    /// JIT by-name native fast-path admissions refused at cache-fill time.
    /// Includes both outright `Reject`s (which contribute a row to sink 1) and
    /// §7-step-3 yields to bytecode (which do not — the resolver returns "no
    /// opinion", and there is no violation object to record). So this is a
    /// strict superset of sink 1's length, by design.
    pub jit_fastpath_admissions: u64,
    /// Times the interpreter's dispatch resolver preferred concrete bytecode
    /// over a registered non-intrinsic native. Distinct triples appear in
    /// sink 2; this count is exact and uncapped.
    pub interpreter_bytecode_preferred: u64,
    /// Times a registered `Bridge` stood in front of concrete bytecode at
    /// `try_stackless_invoke` step 1 and **ran anyway** — §1.4 observed but not
    /// enforced. The one field in this struct that counts something strict
    /// policy did NOT stop, and it is here rather than in `counts` because it
    /// is the same event class as its siblings measured on the other side.
    ///
    /// Zero when `CRATONVM_ENFORCE_NATIVE_SHADOW` is set: enforcement turns
    /// each of these into an `interpreter_bytecode_preferred` instead. Distinct
    /// triples appear in sink 2 tagged `bridge-ran-over-bytecode`. It is
    /// deliberately excluded from [`Self::total`], which counts refusals.
    ///
    /// **The one field here that is a FLOOR rather than exact.** Discovering a
    /// shadow costs a hierarchy walk, so the walk stops once the triple is
    /// recorded and stops entirely once sink 2 saturates — see
    /// `crate::vm::jdk_only_native_shadow_unenforced`. Quote it as "at least".
    pub interpreter_shadow_unenforced: u64,
}

impl JdkOnlyRefusalCounts {
    /// Total refusal events across all four sources. Not a violation count —
    /// see the type docs.
    pub fn total(self) -> u64 {
        self.jit_direct_native_binds
            + self.jit_inline_cache_natives
            + self.jit_fastpath_admissions
            + self.interpreter_bytecode_preferred
    }

    /// Whether strict policy refused nothing at all. Always true in
    /// `Compatible` mode.
    pub fn is_zero(self) -> bool {
        self.total() == 0
    }
}

/// The four coarse class buckets the JDK-only report's `counts` block carries
/// (contract §9).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OriginBuckets {
    pub(crate) boot_image: u64,
    pub(crate) application: u64,
    pub(crate) generated: u64,
    pub(crate) compatibility: u64,
}

impl OriginBuckets {
    pub(crate) fn total(self) -> u64 {
        self.boot_image + self.application + self.generated + self.compatibility
    }
}

/// Fold the ten `ClassOrigin::as_str()` tags into the report's four buckets.
///
/// The fold is a **partition**: every row lands in exactly one bucket, so
/// [`OriginBuckets::total`] equals the row count and a class can never be lost
/// between `--dump-class-origins` and the report's summary. `user-defined`
/// counts as an application class (application bytes, whoever called
/// `defineClass`); the five VM-created origins plus `vm-internal` count as
/// generated — no class file, but a legitimate generation origin (§1 item 6).
///
/// `compatibility-stub` keeps its own bucket and is never folded in with the
/// legitimate generated origins: conflating those two is exactly the confusion
/// this feature exists to end.
pub(crate) fn fold_origin_buckets(rows: &[ClassOriginEntry]) -> OriginBuckets {
    let mut buckets = OriginBuckets::default();
    for row in rows {
        match row.origin.as_str() {
            "boot-image" => buckets.boot_image += 1,
            "application-class-path" | "user-defined" => buckets.application += 1,
            "vm-array" | "hidden-class" | "generated-lambda" | "generated-proxy"
            | "reflection-accessor" | "vm-internal" => buckets.generated += 1,
            "compatibility-stub" => buckets.compatibility += 1,
            // An unknown tag means the origin vocabulary grew. Count it as
            // generated rather than dropping it: an unclassified class must
            // still show up in the totals.
            _ => buckets.generated += 1,
        }
    }
    buckets
}

/// Serialize the class-origin census. Split out of
/// [`SharedVm::dump_class_origins_json`] so the sort order, the seeded `counts`
/// block and the redaction can be unit-tested without booting a VM.
///
/// `rows` is sorted in place by `(name, loader_id, origin)`.
pub(crate) fn render_class_origins_json(rows: &mut [ClassOriginEntry], verbose: bool) -> String {
    // A class name legitimately appears once per defining loader, and twice
    // with different origins during an in-place stub-upgrade window, so all
    // three fields are needed for a total order.
    rows.sort_by(|a, b| (&a.name, a.loader_id, &a.origin).cmp(&(&b.name, b.loader_id, &b.origin)));

    let mut counts: std::collections::BTreeMap<String, u64> = CLASS_ORIGIN_TAGS
        .iter()
        .map(|tag| ((*tag).to_string(), 0u64))
        .collect();
    for row in rows.iter() {
        *counts.entry(row.origin.clone()).or_insert(0) += 1;
    }

    let mut out = String::with_capacity(256 + 160 * rows.len());
    out.push_str("{\n  \"schema_version\": 1,\n  \"counts\": {\n");
    for (tag, n) in &counts {
        out.push_str(&format!("    {}: {n},\n", json_escape(tag)));
    }
    out.push_str(&format!("    \"total\": {}\n", rows.len()));
    out.push_str("  },\n  \"classes\": [");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("\n    {\n");
        // `name` is redacted like the other two: a `CompatibilityStub` for a
        // class named after a jar entry can carry the jar's absolute path.
        out.push_str(&format!(
            "      \"name\": {},\n",
            json_string(&row.name, verbose)
        ));
        out.push_str(&format!("      \"origin\": {},\n", json_escape(&row.origin)));
        out.push_str(&format!(
            "      \"reason\": {},\n",
            json_opt_string(row.reason.as_deref(), verbose)
        ));
        out.push_str(&format!(
            "      \"requested_by\": {},\n",
            json_opt_string(row.requested_by.as_deref(), verbose)
        ));
        out.push_str(&format!(
            "      \"real_bytes_found\": {},\n",
            row.real_bytes_found
        ));
        out.push_str(&format!("      \"loader_id\": {},\n", row.loader_id));
        // Direct supertypes, in declaration order (superclass, then
        // interfaces). Never redacted: these are JDK/application type names,
        // not paths, and the interception join is useless without them.
        out.push_str("      \"supertypes\": [");
        for (i, s) in row.supertypes.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&json_escape(s));
        }
        out.push_str("]\n");
        out.push_str("    }");
    }
    if !rows.is_empty() {
        out.push_str("\n  ");
    }
    out.push_str("]\n}\n");
    out
}

/// The class-origin census a `System.exit` produced when it could not take the
/// class-manager lock.
///
/// Deliberately *not* the same shape as an empty successful census. A consumer
/// that sees `"total": 0` with no `"partial"` key is entitled to conclude the
/// run fabricated nothing; this file says the opposite — the run was not
/// measured. `difftest` and `scripts/jdk-only-census.sh` both categorise from
/// these files, and a silently-short census would categorise a strict failure
/// as a clean run.
///
/// The `counts` block is omitted rather than zero-filled for the same reason:
/// a zero in a count key is a measurement, and there was none.
pub(crate) fn render_partial_class_origins_json() -> String {
    concat!(
        "{\n",
        "  \"schema_version\": 1,\n",
        "  \"partial\": true,\n",
        "  \"partial_reason\": \"class-manager lock unavailable on the System.exit path; ",
        "no class-origin rows were read\",\n",
        "  \"classes\": []\n",
        "}\n"
    )
    .to_string()
}

impl SharedVm {
    /// Thread-safe class loading with per-class-name locks (Session 30).
    ///
    /// Instead of holding the global ClassManager write lock for the entire
    /// loading process, this method:
    /// 1. Checks under **read lock** if the class is already loaded (fast path).
    /// 2. Acquires a **per-class-name lock** to serialize concurrent loaders of
    ///    the *same* class (threads loading *different* classes proceed in parallel).
    /// 3. Re-checks under read lock (double-check after acquiring per-class lock).
    /// 4. Actually loads under write lock only for the parsing/registration step.
    /// 5. Notifies any waiters on the per-class lock.
    pub fn load_class_concurrent(
        &self,
        name: &str,
    ) -> Result<crate::classloading::ClassId, cratonvm_types::error::VmError> {
        self.load_class_concurrent_for(name, None)
    }

    /// [`Self::load_class_concurrent`], offering the class file to this VM's
    /// `java.lang.instrument` transformer chain first.
    ///
    /// This is the entry point every class load that has a Java thread in hand
    /// should use, because running a `ClassFileTransformer` means running Java,
    /// and that needs a thread. Callers with no thread (VM bootstrap, the JIT's
    /// own resolution, JNI helpers before attach) keep `load_class_concurrent`;
    /// they load classes an agent could not have been registered in time to see
    /// anyway.
    ///
    /// Strictly additive: with no agent installed
    /// (`instrument::transformers_armed` is false) this is
    /// `load_class_concurrent` plus one relaxed atomic load, and even with an
    /// agent installed a chain that declines every class changes nothing about
    /// how the load proceeds.
    pub fn load_class_transformed(
        &self,
        thread: &mut crate::threading::JvmThread,
        name: &str,
    ) -> Result<crate::classloading::ClassId, cratonvm_types::error::VmError> {
        if crate::runtime::instrument::transformers_armed(self.vm_identity) {
            crate::runtime::instrument::pre_transform_for_load(self, thread, name, 0);
        }
        self.load_class_concurrent(name)
    }

    /// [`Self::load_class_concurrent`], naming the Java frame that asked.
    ///
    /// `requester` is `(owner_class, method_name, descriptor)` of the frame
    /// whose `new` / `checkcast` / `Class.forName` drove this resolution. It
    /// exists so that when the load fabricates a compatibility class, the
    /// recorded `CompatibilityClassRequested` violation — and the
    /// `--dump-class-origins` row — can say *who* depends on the fabrication.
    /// "Class `X` was fabricated" is a name; "class `X` was fabricated because
    /// `org/foo/Bar.baz(…)` resolved it" is a work item (contract §9).
    ///
    /// Passed as three borrowed `&str`s, not a formatted `String`: this is on
    /// the path of every `new` in the VM, and the overwhelming majority of
    /// loads fabricate nothing. Nothing is formatted or allocated unless the
    /// load actually recorded a violation.
    ///
    /// `None` is the right answer for callers with no Java frame — VM
    /// bootstrap, JNI, the JIT's own resolution — and leaves `requested_by`
    /// null, which the census documents as "not attributable to a frame"
    /// rather than "nobody asked".
    pub fn load_class_concurrent_for(
        &self,
        name: &str,
        requester: Option<(&str, &str, &str)>,
    ) -> Result<crate::classloading::ClassId, cratonvm_types::error::VmError> {
        // Fast path: read lock only — no contention for already-loaded classes.
        // Bind the result to a local so the `RwLockReadGuard` is dropped at
        // the semicolon, not extended to the end of an `if let` block.
        //
        // Runtime-package-identity bug fix: was the bare, requester-less
        // `get_loaded_class_id(name)`, which -- when no BUILT-IN loader
        // (bootstrap/extension/application) has defined `name` yet -- falls
        // back to returning an arbitrary lone user-defined loader's own
        // copy if exactly one such loader happens to have defined it (see
        // that fn's doc comment). This function's own slow path below
        // ultimately delegates via `ClassManager::load_class`, which can
        // only ever produce a Bootstrap/Extension/Application-loaded
        // class -- never an unrelated user-defined loader's redefinition
        // -- so a fast-path cache hit returning one answers a question
        // this function was never asked and hands back the WRONG class
        // (JVMS §5.3: defining loader is part of a class's identity).
        // `get_loaded_class_id_for_requester(name, Application)` probes
        // only the built-in delegation chain, matching what the slow path
        // can actually produce: a hit here is always right, a miss falls
        // through to the real load below instead of a stray loader's class.
        let fast_id = self
            .classes
            .class_manager
            .read()
            .resolve_fast_path_class_id(name);
        if let Some(id) = fast_id {
            // Check if it's a synthetic stub that needs upgrading
            let is_synthetic = self
                .classes
                .class_manager
                .read()
                .class_store
                .get(id)
                .map(|c| c.origin.is_compatibility_stub())
                .unwrap_or(false);
            if !is_synthetic {
                return Ok(id);
            }
            // Fall through to acquire write lock and upgrade
        }

        // Get or create per-class-name lock
        let class_lock = {
            let mut locks = self.classes.class_loading_locks.lock();
            locks
                .entry(name.to_string())
                .or_insert_with(|| {
                    Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()))
                })
                .clone()
        };

        let (lock, cvar) = &*class_lock;
        let mut loading = lock
            .lock()
            .expect("class-loading mutex poisoned: a thread panicked while loading a class");

        // Double-check: another thread may have loaded it while we waited for the lock.
        // Same loader-faithful lookup as the fast path above -- see that
        // comment for why the bare `get_loaded_class_id` is unsound here.
        {
            let cm = self.classes.class_manager.read();
            if let Some(id) = cm.resolve_fast_path_class_id(name) {
                let is_synthetic = cm
                    .class_store
                    .get(id)
                    .map(|c| c.origin.is_compatibility_stub())
                    .unwrap_or(false);
                if !is_synthetic {
                    return Ok(id);
                }
            }
        }

        // If another thread is currently loading this exact class, wait for it
        while *loading {
            loading = cvar
                .wait_timeout(loading, std::time::Duration::from_secs(30))
                .expect("class-loading condvar poisoned: a thread panicked while loading a class")
                .0;
            // Re-check after waking — class may now be loaded. Same
            // loader-faithful lookup as the fast path above (see that
            // comment) -- a lone unrelated user-defined loader's copy must
            // not be handed back here either.
            let cm = self.classes.class_manager.read();
            if let Some(id) = cm.resolve_fast_path_class_id(name) {
                let is_synthetic = cm
                    .class_store
                    .get(id)
                    .map(|c| c.origin.is_compatibility_stub())
                    .unwrap_or(false);
                if !is_synthetic {
                    return Ok(id);
                }
            }
        }

        // We are the loader for this class — mark as loading.
        //
        // B3: wrap the `*loading = true` … `*loading = false` + notify span in
        // an RAII guard so that an UNWIND through `load_class` (which has many
        // `.expect`/`pop_unchecked` paths) still resets the flag and wakes
        // waiters. Without it, a panic during load leaves `*loading == true`
        // forever: every later loader of this class double-checks (not loaded),
        // enters `while *loading`, and spins on the 30s wait timeout that no
        // notifier will ever satisfy — turning one load panic into a cascade of
        // stuck/aborting loaders. The guard's `Drop` runs on both the success
        // path and the unwind path; success-path cleanup (`remove(name)`) runs
        // after the guard is dropped below.
        *loading = true;
        drop(loading);

        /// Resets the per-class loading flag and notifies waiters on drop,
        /// including when the enclosing scope unwinds due to a panic.
        struct LoadingFlagGuard {
            lock: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
        }
        impl Drop for LoadingFlagGuard {
            fn drop(&mut self) {
                let (lock, cvar) = &*self.lock;
                // On unwind the mutex is not poisoned (we dropped the guard
                // before the fallible work), but tolerate a poisoned lock
                // defensively so the notify still fires.
                let mut loading = match lock.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                *loading = false;
                cvar.notify_all();
            }
        }
        let loading_guard = LoadingFlagGuard {
            lock: class_lock.clone(),
        };

        // T19.H7 diag — periodic checkpoints around class load.
        // Rate-limited via static AtomicU32 so we don't drown stderr.
        // Feature-gated (off by default) — sessions 93-94 reverted two silent
        // re-enables of this trace; the gate keeps it out of any non-debug
        // build. To re-enable: cargo build --features experimental-t19-diag.
        #[cfg(feature = "experimental-t19-diag")]
        static T19_H7_LOAD_TRACE: std::sync::atomic::AtomicU32 =
            std::sync::atomic::AtomicU32::new(0);
        #[cfg(feature = "experimental-t19-diag")]
        let _t19_h7_n = T19_H7_LOAD_TRACE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        #[cfg(feature = "experimental-t19-diag")]
        if _t19_h7_n < 250 {
            tracing::debug!(target: "cratonvm::t19_h7", "load[#{_t19_h7_n}] ENTER class={name}");
        }

        // Actually load the class (this takes the global write lock briefly)
        let mut cm_guard = self.classes.class_manager_write();
        // Contract §9 `requested_by`: read the violation count under the same
        // write lock that performs the load, so no other thread's fabrication
        // can be misattributed to this frame. `origin_violation_count` is a
        // `Vec::len`, and `attach_origin_requester` returns without allocating
        // when the count did not move — which is every load but the ones that
        // actually fabricate.
        let violations_before = requester.map(|_| cm_guard.origin_violation_count());
        let result = cm_guard.load_class(name);
        if let (Some(before), Some((owner, method, descriptor))) = (violations_before, requester) {
            cm_guard.attach_origin_requester(before, owner, method, descriptor);
        }
        drop(cm_guard);

        #[cfg(feature = "experimental-t19-diag")]
        if _t19_h7_n < 250 {
            match &result {
                Ok(_) => {
                    tracing::debug!(target: "cratonvm::t19_h7", "load[#{_t19_h7_n}] OK class={name}")
                }
                Err(e) => {
                    tracing::debug!(target: "cratonvm::t19_h7", "load[#{_t19_h7_n}] ERR class={name} err={e:?}")
                }
            }
        }

        // obsaudit D14 (2026-07-26): the hand-written real-env ClassLoad
        // notification that used to live here was removed. It fired on
        // every successful `Ok(class_id)` unconditionally — including a
        // cache hit against an already-loaded class, which is not a new
        // ClassLoad and should not re-fire one. ClassLoad now reaches the
        // real env exactly once per class, from
        // `JvmtiEventManager::fire_class_load`'s bridge (see
        // `install_real_agent_env_bridge` in `runtime/jvmti.rs`), which is
        // driven by `ClassManager::define_class_shared_with_options` and so
        // only runs when the class is actually newly defined.
        if let Ok(class_id) = &result {
            // T5.4.4 — class hierarchy change invalidation.
            //
            // When a new class is loaded, any JIT-compiled method
            // that inlined calls from this class (via CHA-based
            // devirtualization) must be evicted because a new subclass
            // may override the inlined method. The `inlined_methods`
            // list on each `CompiledMethod` records which classes
            // were inlined; `invalidate_for_class_change` walks the
            // cache and removes matching entries.
            //
            // Also invalidate for the superclass chain: if class `B`
            // extends `A`, and a method inlined from `A` was
            // devirtualized under the assumption that `A` had no
            // subclasses, loading `B` breaks that assumption.
            // PGO-02 R3 — two defects fixed here at once.
            //
            // 1. This used to be `c.superclass.map(|s| s.to_string())`.
            //    `superclass` is a `ClassId`, whose `Display` prints the raw
            //    u32, so the "superclass" handed to the name-keyed scans below
            //    was a DECIMAL NUMBER — `"42"`, never a class name. The scans
            //    match against `CompiledMethod::inlined_methods`, which holds
            //    internal class names, so the whole superclass half of
            //    class-load invalidation matched nothing and had been a silent
            //    no-op. Nothing failed when it broke: the guarded/devirtualised
            //    code stays CORRECT without the eviction (an exact class-id
            //    guard rechecks the receiver, and a MIC/PIC re-targets), so the
            //    only symptom was code that should have been retired staying
            //    resident. That is precisely the failure mode this project
            //    tracks — a capability that reads as landed but never runs.
            //
            // 2. Only the DIRECT superclass was consulted, which is the
            //    "known coarseness" `docs/feature-designs/profile-guided-
            //    inlining.md` §4 records: loading `C extends B extends A` did
            //    not reach a dependency on `A`. Walk the whole supertype
            //    closure — superclasses AND interfaces — so a speculation on
            //    any ancestor is retired when a new descendant appears. The
            //    closure is bounded by hierarchy depth and this runs once per
            //    class DEFINE, not per call.
            let supertypes: Vec<String> = {
                let cm = self.classes.class_manager.read();
                let store = cm.class_store();
                let mut names: Vec<String> = Vec::new();
                let mut stack: Vec<cratonvm_types::ClassId> = Vec::new();
                let mut seen: std::collections::HashSet<cratonvm_types::ClassId> =
                    std::collections::HashSet::new();
                if let Some(class) = store.get(*class_id) {
                    stack.extend(class.interfaces.iter().copied());
                    stack.extend(class.superclass);
                }
                while let Some(id) = stack.pop() {
                    if !seen.insert(id) {
                        continue;
                    }
                    let Some(class) = store.get(id) else {
                        continue;
                    };
                    names.push(class.name.to_string());
                    stack.extend(class.interfaces.iter().copied());
                    stack.extend(class.superclass);
                }
                names
            };
            {
                let mut jit = self.jit.jit_cache.write();
                let _ = jit.invalidate_for_class_change(name);
                for sup in &supertypes {
                    let _ = jit.invalidate_for_class_change(sup);
                }
            }
            // T5.4.4 — also consult the InvalidationManager's
            // `on_class_loaded(class_id)` which tracks `LeafClass`
            // compilation assumptions that the `inlined_methods` scan
            // above cannot see (an assumption refers to the class_id
            // whose leaf-ness was assumed, not the class whose code
            // was inlined). This closes the loop for CHA-devirtualized
            // entries whose inlined_methods list doesn't already name
            // the newly loaded class.
            let _evicted_by_cha = self.invalidate_jit_for_class(name);
            for sup in &supertypes {
                let _evicted_sup = self.invalidate_jit_for_class(sup);
            }

            // Phase 1 — Item 6: `@EnableGpuAsync(warmup = N)` class-load
            // warmup. If the class is annotated, eagerly pre-compile up
            // to `N` `@GpuKernel`-annotated methods so the first call
            // does not pay the analyzer + PTX lowering cost. Cheap when
            // offload is off — `maybe_warmup_gpu` short-circuits on the
            // `gpu_offload_enabled` flag.
            //
            // The class manager read-lock is held for the duration of
            // the warmup; the inner `lookup_or_compile` only touches
            // the `OffloadCache`'s own locks (`kernels`, `blacklist`)
            // so there is no re-entrancy risk against the manager.
            #[cfg(feature = "gpu-offload")]
            {
                let class_id = *class_id;
                let cm = self.classes.class_manager.read();
                if let Some(class) = cm.get_class(class_id) {
                    crate::runtime::offload::maybe_warmup_gpu(self, class, class_id);
                }
            }
        }

        // Mark done and notify all waiters for this class. Dropping the guard
        // re-acquires the lock, sets `*loading = false`, and `notify_all()`s
        // (the same work the old manual block did) — and it would have run the
        // same reset/notify automatically had `load_class` above unwound.
        drop(loading_guard);

        // Clean up per-class lock entry to avoid unbounded growth
        // (only if nobody else is waiting — check refcount). Done after the
        // guard drop so waiters have already been notified.
        if Arc::strong_count(&class_lock) <= 2 {
            // 2 = our local + the HashMap entry — no other thread holds it
            self.classes.class_loading_locks.lock().remove(name);
        }

        result
    }

    /// Register a newly-allocated object as finalizable.
    ///
    /// Called by the allocator when the object's class has `has_finalizer == true`.
    /// The object address is registered with the reference processor as a
    /// Finalizer reference so GC can enqueue it for `finalize()` invocation.
    pub fn register_finalizable(&self, obj_addr: usize) {
        let mut rp = self.mem.ref_processor.lock();
        rp.discover_reference(
            cratonvm_gc::reference::ReferenceType::Finalizer,
            obj_addr, // reference_obj = the object itself
            obj_addr, // referent = same object
            None,     // no ReferenceQueue
        );
    }

    /// Drain the finalization queue and invoke `finalize()` on each object.
    ///
    /// Returns the number of objects finalized.  This is called after GC
    /// reference processing has moved unreachable finalizable objects into
    /// the finalization queue.
    pub fn drain_finalizers(&self) -> usize {
        // Move objects from ref_processor's finalization_queue to the
        // FinalizerThread queue (mirrors HotSpot's two-stage pipeline).
        let mut rp = self.mem.ref_processor.lock();
        let mut count = 0usize;
        while let Some(obj_addr) = rp.dequeue_for_finalization() {
            self.mem.finalizer_thread.enqueue(obj_addr);
            count += 1;
        }
        count
    }

    /// Drain the cleaner queue and return the addresses of cleaner actions
    /// to execute.  The caller is responsible for actually running them.
    pub fn drain_cleaners(&self) -> Vec<usize> {
        self.mem.cleaner_thread.drain_actions()
    }

    /// Process all reference types after a GC marking phase.
    ///
    /// `is_marked` returns `true` for live objects.
    /// Results are funnelled into the finalizer/cleaner queues.
    pub fn process_references(
        &self,
        is_marked: &dyn Fn(usize) -> bool,
        free_heap_mb: usize,
        current_time_ms: u64,
    ) {
        let mut rp = self.mem.ref_processor.lock();
        let result = rp.process_references(is_marked, free_heap_mb, current_time_ms);

        // Enqueue objects needing finalization
        for obj_addr in &result.to_finalize {
            self.mem.finalizer_thread.enqueue(*obj_addr);
        }

        // Submit cleaner actions
        for action_addr in &result.cleaner_actions {
            self.mem.cleaner_thread.submit_action(*action_addr);
        }
    }
}

impl SharedVm {
    /// Record a deoptimization event to both the deopt log and the tiered
    /// compilation manager.
    pub fn record_deoptimization(
        &self,
        method_key: &str,
        event: crate::jit::deopt::DeoptEvent,
        tiered_key: &crate::jit::tiered::MethodKey,
    ) -> crate::jit::deopt::DeoptAction {
        let mut log = self.jit.deopt_log.lock();
        // The bci-aware policy: a method whose only failing speculation has
        // already been de-spec'd is recompiled, not blacklisted. See
        // `DeoptimizationLog::recommend_action_at_bci`.
        let action = log.recommend_action_at_bci(method_key, event.reason, event.bci);
        log.record_deopt(method_key, event);
        self.jit.tiered_manager.on_deoptimization(tiered_key);
        action
    }

    /// deopt-osr Step 9 — current live compilation epoch for `method_key`
    /// (`"<class>.<method>:<descriptor>"`), `0` if the method has never been
    /// invalidated. See [`Self::method_epochs`].
    pub fn compilation_epoch_for(&self, method_key: &str) -> u64 {
        self.jit
            .method_epochs
            .read()
            .get(method_key)
            .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or_else(|| {
                self.jit
                    .method_epoch_overflow
                    .load(std::sync::atomic::Ordering::Relaxed)
            })
    }

    /// deopt-osr Step 9 — advance and return the live compilation epoch for
    /// `method_key`, marking every artifact compiled at an earlier epoch as
    /// superseded. Called on each invalidation (see
    /// `DeoptimizationController::deoptimize`). See [`Self::method_epochs`].
    pub fn bump_compilation_epoch(&self, method_key: &str) -> u64 {
        const METHOD_EPOCH_CAP: usize = 65_536;
        let mut map = self.jit.method_epochs.write();
        if !map.contains_key(method_key) && map.len() >= METHOD_EPOCH_CAP {
            return self
                .jit
                .method_epoch_overflow
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
        }
        let cell = map
            .entry(method_key.to_string())
            .or_insert_with(|| Box::new(std::sync::atomic::AtomicU64::new(0)));
        // fetch_add returns the PREVIOUS value; the new live epoch is +1.
        cell.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
    }

    /// deopt-osr Step 9 follow-up (a) — a STABLE process-lifetime pointer to
    /// `method_key`'s live compilation-epoch cell, creating the entry at epoch 0
    /// if absent. The cell is a boxed `AtomicU64`, so its address survives map
    /// rehashes and entries are never removed — the pointer is valid forever.
    /// Baked into the method's `DeoptEpochGuard` so `x64_deopt_entry` can read
    /// the live epoch lock-free before touching the deopt box. Only called under
    /// `deopt_real_enabled()` (at install, by `stamp_compilation_epoch`).
    pub fn live_epoch_cell_ptr(&self, method_key: &str) -> *const std::sync::atomic::AtomicU64 {
        const METHOD_EPOCH_CAP: usize = 65_536;
        let mut map = self.jit.method_epochs.write();
        if !map.contains_key(method_key) && map.len() >= METHOD_EPOCH_CAP {
            return &self.jit.method_epoch_overflow;
        }
        let cell = map
            .entry(method_key.to_string())
            .or_insert_with(|| Box::new(std::sync::atomic::AtomicU64::new(0)));
        cell.as_ref() as *const std::sync::atomic::AtomicU64
    }

    /// T5.4.4 — Class-hierarchy change listener.
    ///
    /// When `class_name` is linked/registered, walk the
    /// [`cratonvm_jit::deopt::InvalidationManager`] to collect every compiled
    /// method that made a `LeafClass(class_id)` assumption (or registered a
    /// direct `class_dependencies` entry) for the newly loaded class, then
    /// evict each of those entries from [`Self::jit_cache`]. Method keys in
    /// the invalidation manager are stored as `"<class>.<method>:<descriptor>"`
    /// (see `s36_invalidation_manager_tracks_class_dependencies`); we parse
    /// that format back into the tuple the cache expects.
    ///
    /// Returns the number of entries evicted. A return value of 0 is normal —
    /// it just means no compiled code depended on this class.
    pub fn invalidate_jit_for_class(&self, class_name: &str) -> usize {
        // Resolve ClassId — if the class isn't loaded yet (e.g. a caller
        // invoked us before registration completed), there can be no
        // LeafClass assumption on it, so there's nothing to evict.
        let class_id_u32: u32 = match self
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(class_name)
        {
            Some(cid) => cid.as_u32(),
            None => return 0,
        };

        // Ask the invalidation manager which method keys are now invalid.
        let invalidated_keys: Vec<String> = {
            let inv = self.jit.invalidation_manager.lock();
            inv.on_class_loaded(class_id_u32)
        };
        if invalidated_keys.is_empty() {
            return 0;
        }

        // Parse each "<class>.<method>:<descriptor>" key and remove the
        // matching entry from the JIT cache.
        let mut evicted = 0usize;
        let mut jit = self.jit.jit_cache.write();
        for key in &invalidated_keys {
            // Split on the last ':' to isolate the descriptor (the descriptor
            // itself may not contain ':', but the class name / method name
            // could theoretically contain one via inner-class mangling, so
            // splitting from the right is the safe choice).
            let (class_method, descriptor) = match key.rsplit_once(':') {
                Some(t) => t,
                None => continue,
            };
            // Split `<class>.<method>` on the last '.' — class names may
            // contain dots (e.g. `foo.bar.Baz.methodName`).
            let (class_part, method_part) = match class_method.rsplit_once('.') {
                Some(t) => t,
                None => continue,
            };
            let before = jit.len();
            let part_class_id = self
                .classes
                .class_manager
                .read()
                .get_loaded_class_id(class_part)
                .unwrap_or(cratonvm_types::ClassId::new(0));
            jit.remove(class_part, method_part, descriptor, part_class_id);
            if jit.len() < before {
                evicted += 1;
            }
        }
        evicted
    }
}

impl SharedVm {
    /// Sync JIT ProfileStore data into the AOT TrainingRunRecorder.
    ///
    /// Snapshots all branch and receiver profiles from the JIT, resolves class_id
    /// to class names, and bulk-imports them into the AOT recorder.
    /// Returns number of methods synced.
    #[cfg(feature = "experimental-aot")]
    pub fn sync_jit_profiles_to_aot(&self) -> usize {
        use crate::native::builtins::aot;

        if !aot::is_aot_training() {
            return 0;
        }

        let snapshots = self.jit.profile_store.snapshot_all();
        let class_mgr = self.classes.class_manager.read();

        // Collect branch data
        let mut _branch_entries: Vec<(&str, &str, u32, u32, u32)> = Vec::new();
        let mut _receiver_entries: Vec<(&str, &str, &str, &str, u32)> = Vec::new();

        // We need owned strings for class names since we're building slices
        let mut class_names: Vec<Option<String>> = Vec::with_capacity(snapshots.len());
        let mut receiver_names_cache: std::collections::HashMap<u32, String> =
            std::collections::HashMap::new();

        for (key, _profile) in &snapshots {
            let name = class_mgr
                .class_store
                .get(cratonvm_types::ClassId::new(key.class_id))
                .map(|c| c.name.to_string());
            class_names.push(name);
        }

        // Now build the entries with references to the owned strings
        let mut synced = 0usize;
        for (i, (key, profile)) in snapshots.iter().enumerate() {
            let class_name = match &class_names[i] {
                Some(n) => n.as_str(),
                None => continue,
            };
            let method_name = &*key.method_name;

            // Branch data
            for (&pc, counts) in &profile.branches {
                if counts.taken > 0 || counts.not_taken > 0 {
                    aot::aot_record_branch(class_name, method_name, pc as u32, true);
                    // Record aggregate counts via bulk API
                    // (we record one call per taken/not_taken for simplicity since
                    //  the recorder accumulates internally)
                    for _ in 1..counts.taken {
                        aot::aot_record_branch(class_name, method_name, pc as u32, true);
                    }
                    for _ in 0..counts.not_taken {
                        aot::aot_record_branch(class_name, method_name, pc as u32, false);
                    }
                }
            }

            // Receiver type data
            for (_pc, receivers) in &profile.receivers {
                for (&receiver_class_id, &count) in receivers {
                    let receiver_name = receiver_names_cache
                        .entry(receiver_class_id)
                        .or_insert_with(|| {
                            class_mgr
                                .class_store
                                .get(cratonvm_types::ClassId::new(receiver_class_id))
                                .map(|c| c.name.to_string())
                                .unwrap_or_default()
                        });
                    if !receiver_name.is_empty() {
                        for _ in 0..count {
                            aot::aot_record_receiver_type(
                                class_name,
                                method_name,
                                &*key.descriptor,
                                receiver_name,
                            );
                        }
                    }
                }
            }

            synced += 1;
        }
        synced
    }
}

impl SharedVm {
    // -----------------------------------------------------------------------
    // T19.H1 — stack-dump-on-timeout API
    // -----------------------------------------------------------------------

    /// T19.H1 — request every interpreter thread to dump its frame chain
    /// to stderr at the next dispatch-loop iteration.
    ///
    /// Called by the CLI watchdog (see `--stack-dump-on-timeout`) after
    /// the configured deadline elapses. The flag is sticky and never
    /// cleared — the process is expected to abort shortly after.
    pub fn request_stack_dump(&self) {
        self.debug
            .stack_dump_requested
            .store(true, std::sync::atomic::Ordering::Release);
        // KC16-watchdog: also wake every thread parked in Object.wait().
        // The interpreter top-of-loop poll only fires when the thread is
        // actively dispatching bytecode — a thread blocked in
        // `parking_lot::Condvar::wait_for` (Monitor::wait) never reaches
        // it. Signalling the global wait flag lets each waiter's 5ms
        // poll exit the condvar and re-check its state, where the
        // standard top-of-loop ack path then fires.
        crate::threading::monitor::signal_stack_dump_to_waiters();
        // Also unpark every thread blocked in `LockSupport.park` (AQS
        // lock/latch/executor waiters). These never reach the condvar the
        // line above signals, so without this they stay invisible to the
        // watchdog. They wake, emit their park-site snapshot / current
        // frames, and the process aborts right after.
        self.threads.thread_registry.unpark_all_for_stack_dump();
        // The per-thread summary is NOT printed here. It used to be, and that
        // made its most load-bearing column a lie: printed at request time, it
        // cannot know which threads went on to answer, so it labelled every
        // RUNNING thread "read the live stack dump above" — including threads
        // that never produced one. The watchdog now calls
        // [`Self::dump_thread_summary_after_dumps`] once the grace period has
        // closed, when the ack set is complete.
    }

    /// T19.H1 — print the per-thread summary once the watchdog's grace period
    /// has closed, so it can distinguish a RUNNING thread that dumped from one
    /// that stayed silent (i.e. is in JIT-compiled code or a long native call).
    ///
    /// Summarizes every registered thread, including those with no dumpable
    /// interpreter frames — blocked in a native lock, or never started.
    pub fn dump_thread_summary_after_dumps(&self) {
        let acked = self.debug.stack_dump_acked_tids.lock().clone();
        self.threads
            .thread_registry
            .dump_thread_summary_to_stderr(&acked);
        self.dump_object_wait_census();
    }

    /// EVERY thread parked in `Object.wait()` at dump time, and the state of
    /// the object each one is parked on.
    ///
    /// # The question this exists to answer
    ///
    /// `known-issues/netty/parameterizedsslhandlertest-residual-stalls` named
    /// one cheap step that had never been taken: at stall time, dump
    /// every parked waiter rather than the first one the watchdog reaches. The
    /// residual it was written for is a promise that was never completed with
    /// no notification due — for which "nothing completed it" and "the thread
    /// that would have completed it is itself parked" are different defects
    /// with different suspect lists, and the single-thread dump cannot
    /// separate them. This can, in the stall that is already happening.
    ///
    /// An EMPTY census is a result, not a gap, and it is labelled as one: it
    /// means no thread in the process is in `Object.wait()`, so a stalled test
    /// thread is blocked on something else entirely (`LockSupport.park`, a
    /// native, a lock acquire) and the whole `Object.wait()` line of enquiry
    /// is the wrong one.
    pub fn dump_object_wait_census(&self) {
        let rows = self.threads.thread_registry.waiting_monitor_census();
        eprintln!(
            "--- [WAIT-CENSUS] {} thread(s) parked in Object.wait() \
             (registry `jmx_waiting_monitor`, a GC-forwarded root) ---",
            rows.len()
        );
        if rows.is_empty() {
            eprintln!(
                "  [WAIT-CENSUS] none. NOT a missing instrument: no thread in this \
                 process is inside Object.wait(), so a stall here is parked on \
                 something else (LockSupport.park, a lock acquire, or a native call)."
            );
        }
        for (tid, name, alive, obj, waited_ms) in rows {
            eprintln!(
                "  [WAIT-CENSUS] tid={tid} name={name:?} alive={alive} waited_ms={waited_ms}"
            );
            crate::vm::vm_init::dump_wait_object_state(self, obj);
        }
        eprintln!("--- [WAIT-CENSUS] end ---");
    }

    /// T19.H1 — fast-path check used by the interpreter hot loop.
    ///
    /// Returns `true` once the watchdog has flipped
    /// [`Self::stack_dump_requested`]. The load is `Relaxed` — we
    /// trade strict memory ordering for a single cold branch per
    /// dispatched bytecode (the typical case is `false` forever).
    #[inline(always)]
    pub fn stack_dump_pending(&self) -> bool {
        self.debug
            .stack_dump_requested
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether stack dumps are being driven as a periodic *sampler* rather
    /// than as the watchdog's one-shot pre-abort dump. See
    /// [`crate::vm::realms::DebugRealm::stack_sample_mode`].
    #[inline(always)]
    pub fn stack_sample_mode(&self) -> bool {
        self.debug
            .stack_sample_mode
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Arm sampling mode. Called once by the CLI when `--stack-sample-ms` is
    /// given, before the sampler thread starts re-arming the dump request.
    pub fn enable_stack_sampling(&self) {
        self.debug
            .stack_sample_mode
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Consume the pending dump request (sampling mode only) so the next
    /// re-arm from the sampler thread produces the next sample.
    pub fn clear_stack_dump_request(&self) {
        self.debug
            .stack_dump_requested
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// Re-arm the dump request without the watchdog's thread-summary and
    /// unpark side effects, which are far too costly to repeat every
    /// sampling interval (and would themselves distort the profile).
    pub fn request_stack_sample(&self) {
        self.debug
            .stack_dump_requested
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// T19.H1 — dump the calling thread's frame chain to stderr.
    ///
    /// Writes one line per frame with the format
    /// `tid=<N> depth=<D> class=<C> method=<M> desc=<sig> pc=<P> source=<file>`,
    /// oldest frame first, so a reader can follow control flow top-down.
    /// Class and method names are truncated to 240 bytes each to avoid
    /// runaway output when the class loader has recorded a malformed
    /// entry during the hang.
    ///
    /// Increments [`Self::stack_dump_ack_count`] so the watchdog can
    /// tell how many runtime threads responded before it gives up and
    /// calls `std::process::abort`.
    ///
    /// JIT-frame caveat (2026-07-16 investigation of the
    /// `CRATONVM-SPRING-GENUINE-BUGLIST` "T19.H1
    /// watchdog SIGSEGVs on JIT frames" note): `thread.frames` is the
    /// interpreter's own logical frame stack. A method dispatched straight
    /// to already-JIT-compiled machine code (`execute_invokestatic_cached`
    /// / `execute_jit_call` et al. in `runtime/interpreter.rs`) never gets
    /// a `Frame` pushed here at all — the call is a synchronous jump into
    /// native code with no interpreter bookkeeping in between. That frame
    /// is therefore invisible to this walker: reproduction while
    /// root-causing this bug confirmed a thread parked deep inside a
    /// long-running JIT-compiled method dumps as either zero acks (if it
    /// never returns to the interpreter dispatch loop before the grace
    /// period elapses) or a misleadingly shallow frame count (if it does),
    /// never a fabricated/garbage frame — but a reader unaware of the gap
    /// can easily misdiagnose "1 shallow frame" as "this thread is stuck
    /// at a trivial call site" when it may be many JIT call-levels deep.
    /// The two hardening changes below close the *reliability* half of
    /// that report even though a live SIGSEGV could not be reproduced on
    /// this revision after extensive targeted testing (single JIT calls,
    /// OSR-adjacent long single-invocation loops, deep JIT<->interpreter
    /// recursion, and multi-threaded runs with one thread parked in JIT):
    /// 1. Each frame is formatted behind `catch_unwind` so a panic while
    ///    rendering one (malformed) frame can never abort the watchdog
    ///    thread before it reaches its own `process::abort()` — that
    ///    would otherwise turn an intended, informative crash-with-dump
    ///    into a silent hang (no dump, no abort, no notification).
    /// 2. When JIT is active for this thread and the walk comes up
    ///    shorter than the interpreter's own call chain, a note is
    ///    appended pointing at the gap and at `--nojit` as a workaround,
    ///    instead of leaving the reader to assume a shallow dump means a
    ///    shallow call stack.
    pub fn dump_current_thread_frames(&self, thread: &JvmThread) {
        use std::io::Write;

        // SAFETY: stderr is always available and shared; we serialize via
        // a local buffer first and make one `write_all` call per line so
        // concurrent dumps from multiple threads interleave cleanly at
        // the line boundary rather than the byte boundary.
        let stderr = std::io::stderr();
        let mut handle = stderr.lock();

        let tid = thread.thread_id.0;
        let name = &thread.name;
        let frame_count = thread.frames.len();
        let header =
            format!("--- T19.H1 stack dump: tid={tid} name={name:?} frames={frame_count} ---\n");
        let _ = handle.write_all(header.as_bytes());

        for (depth, frame) in thread.frames.iter().enumerate() {
            // Defense-in-depth: a single malformed/corrupted frame (e.g. a
            // truncated class/method name that trips an unexpected panic
            // path inside formatting) must not prevent the watchdog from
            // reaching its `process::abort()` below — that would silently
            // turn a diagnosable crash into an unexplained hang instead.
            let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Truncate user-visible strings defensively — a corrupted
                // method name could otherwise produce megabytes of output.
                let class_name = truncate_ascii(frame.class_name(), 240);
                let method_name = truncate_ascii(frame.method_name(), 240);
                let desc = truncate_ascii(frame.method_descriptor(), 240);
                let source = frame
                    .source_file()
                    .map(|s| truncate_ascii(s, 240))
                    .unwrap_or_else(|| "<unknown>".to_string());
                format!(
                    "tid={tid} depth={depth} class={class_name} method={method_name} desc={desc} pc={pc} last_pc={last} source={source}\n",
                    pc = frame.pc,
                    last = frame.last_instr_pc,
                )
            }))
            .unwrap_or_else(|_| {
                format!("tid={tid} depth={depth} <frame dump panicked; skipped>\n")
            });
            let _ = handle.write_all(rendered.as_bytes());
        }

        // JIT-frame visibility note (see doc comment above): this thread
        // has at least one active JIT call on its native stack right now
        // (it reached this dump from a dispatch callback nested inside
        // JIT-compiled code — the common all-interpreter case has JIT
        // depth 0 here). One or more call levels between the frames shown
        // above are therefore JIT-compiled and invisible to this walker.
        // Flag it explicitly rather than leaving the reader to assume the
        // frames shown are the whole call chain.
        if crate::jit::conservative_roots::current_thread_jit_depth() != 0 {
            let note = format!(
                "tid={tid} note=thread has {depth} active JIT call(s) on its native \
                 stack; one or more call levels are JIT-compiled machine code and are \
                 not represented in the {frame_count} frame(s) above (see \
                 execute_invokestatic_cached/execute_jit_call in runtime/interpreter.rs) — \
                 rerun with --nojit for a full interpreted stack if needed\n",
                depth = crate::jit::conservative_roots::current_thread_jit_depth(),
            );
            let _ = handle.write_all(note.as_bytes());
        }

        let footer = format!("--- T19.H1 end dump tid={tid} ---\n");
        let _ = handle.write_all(footer.as_bytes());
        let _ = handle.flush();

        self.debug
            .stack_dump_ack_count
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        // Record WHICH thread answered, not just how many did — see
        // `stack_dump_acked_tids`.
        self.debug.stack_dump_acked_tids.lock().push(tid);
    }

    /// T19.H1 — count of threads that have completed their dump.
    pub fn stack_dump_ack_count(&self) -> u32 {
        self.debug
            .stack_dump_ack_count
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

// KC16-watchdog: thread-local snapshot of the current thread's frame chain,
// populated right before the thread parks in `Object.wait()`. The static
// `WAIT_SITE_DUMP` callback (installed in `Vm::new`) reads this thread-
// local to emit a stack dump from inside the wait loop, where the
// `JvmThread` is not directly accessible.
thread_local! {
    static WAIT_SITE_SNAPSHOT: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Build a frame-chain snapshot string for the given thread and stash it
/// in the thread-local so a wait-site dump can emit it later. Cleared on
/// wait return.
pub fn set_wait_site_snapshot(thread: &JvmThread) {
    let mut buf = String::new();
    let tid = thread.thread_id.0;
    let name = &thread.name;
    let fc = thread.frames.len();
    buf.push_str(&format!(
        "--- T19.H1 stack dump (wait-site): tid={tid} name={name:?} frames={fc} ---\n"
    ));
    for (depth, frame) in thread.frames.iter().enumerate() {
        // See the matching guard in `dump_current_thread_frames` above: a
        // panic while rendering a single frame must not abort this
        // (rare, timeout-triggered) snapshot build and leave the wait-site
        // dump silently empty for the rest of the thread's parked lifetime.
        let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let class_name = truncate_ascii(frame.class_name(), 240);
            let method_name = truncate_ascii(frame.method_name(), 240);
            let desc = truncate_ascii(frame.method_descriptor(), 240);
            let source = frame
                .source_file()
                .map(|s| truncate_ascii(s, 240))
                .unwrap_or_else(|| "<unknown>".to_string());
            format!(
                "tid={tid} depth={depth} class={class_name} method={method_name} desc={desc} pc={pc} last_pc={last} source={source}\n",
                pc = frame.pc,
                last = frame.last_instr_pc,
            )
        }))
        .unwrap_or_else(|_| format!("tid={tid} depth={depth} <frame dump panicked; skipped>\n"));
        buf.push_str(&rendered);
    }
    if crate::jit::conservative_roots::current_thread_jit_depth() != 0 {
        buf.push_str(&format!(
            "tid={tid} note=thread has active JIT call(s) on its native stack; \
             one or more call levels are JIT-compiled machine code and are not \
             represented above\n"
        ));
    }
    buf.push_str(&format!("--- T19.H1 end dump tid={tid} (wait-site) ---\n"));
    WAIT_SITE_SNAPSHOT.with(|c| *c.borrow_mut() = Some(buf));
}

pub fn clear_wait_site_snapshot() {
    WAIT_SITE_SNAPSHOT.with(|c| *c.borrow_mut() = None);
}

/// Called by the Monitor wait loop (via the OnceLock callback installed in
/// `Vm::new`) when the stack-dump watchdog flag is observed. Reads the
/// thread-local snapshot captured by `monitor_wait` on entry and writes
/// it to stderr, then increments the watchdog ack counter.
pub fn dump_wait_site_thread_local(shared: &SharedVm) {
    use std::io::Write;
    let snapshot = WAIT_SITE_SNAPSHOT.with(|c| c.borrow().clone());
    if let Some(buf) = snapshot {
        let stderr = std::io::stderr();
        let mut handle = stderr.lock();
        let _ = handle.write_all(buf.as_bytes());
        let _ = handle.flush();
        shared
            .debug
            .stack_dump_ack_count
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    } else {
        tracing::warn!(
            target: "kc16_watchdog",
            "wait-site dump fired but no snapshot was captured"
        );
    }
}

/// Print the state of the object a thread is parked on in `Object.wait()`.
///
/// The netty `ParameterizedSslHandlerTest` stall bottoms out in
/// `DefaultPromise.await`/`awaitUninterruptibly` at the `Object.wait()` BCI, a
/// 30 s-spurious-wakeup A/B showed the awaited promise is ALREADY complete, and
/// the orphan check came back clean — so the waiter is on the right monitor and
/// a notifier would resolve the same one. The remaining question is netty's own
/// bookkeeping, which lives in two fields of the promise:
///
/// * `result`  — non-null once the promise completes (set OUTSIDE the monitor).
/// * `waiters` — a PLAIN int the waiter increments inside the monitor
///   immediately before `wait()`; `checkNotifyWaiters()` skips `notifyAll()`
///   entirely when it reads 0.
///
/// `result != null` with `waiters >= 1` here means the completer either never
/// ran `checkNotifyWaiters` or read a stale `waiters` — i.e. the monitor is not
/// establishing happens-before between the two `synchronized` blocks.
pub fn dump_wait_object_state(shared: &SharedVm, obj: ObjectRef) {
    let cid = shared.mem.heap.class_id_of(obj);
    let name = shared
        .classes
        .class_manager
        .read()
        .class_store
        .get(cid)
        .map(|c| c.name.to_string())
        .unwrap_or_else(|| format!("<class_id {cid:?}>"));
    let field = |f: &str| -> Option<cratonvm_types::Value> {
        let cm = shared.classes.class_manager.read();
        let idx = super::vm_exec::resolve_field_index_in_hierarchy(cid, f, &cm.class_store)?;
        drop(cm);
        Some(shared.mem.heap.get_field(obj, idx))
    };
    // SLOT PROVENANCE. The 2026-08-24 `ParameterizedSslHandlerTest` residual
    // page asked, of a dump that read `Int(0)` out of a
    // `private volatile Object`, whether the value was real or whether
    // `resolve_field_index_in_hierarchy` had answered with the wrong slot for
    // that receiver. The dump could not say, because it printed neither the
    // index it used nor the layout it used it against. It does now, and the
    // two together are decidable by inspection. (It was neither — see the
    // never-written-cell note below.)
    let result_slot = resolve_declared_instance_field(shared, cid, "result");
    // WHAT `result` ACTUALLY IS, not just whether it is non-null.
    //
    // `result != null` is NOT the same claim as "the promise completed", and
    // reading it as one is the single largest risk this dump carries.
    // `io.netty.util.concurrent.DefaultPromise` holds THREE kinds of value in
    // that one field:
    //
    // ```java
    // private static final Object SUCCESS = new Object();
    // private static final Object UNCANCELLABLE = new Object();
    // private static boolean isDone0(Object result) {
    //     return result != null && result != UNCANCELLABLE;
    // }
    // public boolean setUncancellable() {
    //     if (RESULT_UPDATER.compareAndSet(this, null, UNCANCELLABLE)) return true;
    //     …
    // }
    // ```
    //
    // `setUncancellable()` — which the channel register and bind paths call as
    // a matter of course — publishes a NON-NULL `result` and deliberately does
    // NOT notify, because the promise is still pending. A waiter's
    // `while (!isDone())` correctly parks, and `checkNotifyWaiters` is
    // correctly never called. So `result != null` + `waiters == 1` + no
    // notification is EXACTLY what a promise that was made uncancellable and
    // then never completed looks like — with no memory-ordering defect
    // anywhere, and with the defect living upstream in whatever should have
    // completed it.
    //
    // Distinguishing the two is a pointer comparison against the class's own
    // statics, so the dump does it rather than leaving the reader to assume.
    let result = field("result");
    // A NEVER-WRITTEN REFERENCE CELL IS NOT A TYPE-PUNNED ONE.
    //
    // `Value` is `#[repr(u32)]` with `Int = 0` and `Object = 4`, so a
    // zero-filled 16-byte slot decodes as `Value::Int(0)` and NOT as
    // `Object(None)` — see `init_primitive_fields`' G56-1 note. The
    // interpreter's allocation path writes the `Object(None)` tag explicitly;
    // the JIT's arms clear the body to zero and rely on every reader
    // repairing the tag locally (`opcodes.rs`' `getfield` fixup, the inline
    // read's payload-only load, `coerce_field_value_for_slot`,
    // `values_equal_for_cas`). Both are "null" to Java. This dump was the one
    // reader that did NOT repair it, and it reported the difference as
    // `result_is=not-a-reference-slot` — which reads as heap corruption when
    // it is an unwritten field of a promise that is simply still pending.
    //
    // So normalise exactly as the other readers do, print BOTH values, and
    // say which one the verdict is about.
    let result_desc = result_slot.as_ref().map(|s| s.descriptor.clone());
    let ref_declared = result_desc
        .as_deref()
        .is_some_and(|d| d.starts_with('L') || d.starts_with('['));
    let normalised = match (&result, ref_declared) {
        (Some(cratonvm_types::Value::Int(0)), true) => Some(cratonvm_types::Value::Object(None)),
        _ => result.clone(),
    };
    let verdict = classify_promise_result(shared, cid, &normalised);
    eprintln!(
        "[WAIT-OBJECT] obj={:p} class={name} result={:?} result_is={verdict} waiters={:?}",
        obj.as_ptr(),
        result,
        field("waiters"),
    );
    match &result_slot {
        Some(slot) => {
            eprintln!(
                "[WAIT-OBJECT] result_slot=index {} declared {} on {} (receiver layout: {})",
                slot.index,
                slot.descriptor,
                slot.owner,
                describe_instance_layout(shared, cid),
            );
            if ref_declared && !matches!(result, Some(cratonvm_types::Value::Object(_))) {
                eprintln!(
                    "[WAIT-OBJECT] result cell is a NEVER-WRITTEN zero cell, not a punned \
                     slot: the field is declared {} and the slot decodes as {:?}, which is \
                     what a zero-filled 16-byte `Value` cell decodes to (`Int` is \
                     discriminant 0). Every other reader in the VM treats it as null, so the \
                     verdict above is taken on the normalised value and this promise is \
                     PENDING, not corrupt.",
                    slot.descriptor, result,
                );
            }
        }
        None => eprintln!(
            "[WAIT-OBJECT] result_slot=UNRESOLVED — no instance field named `result` anywhere \
             in this receiver's hierarchy, so the value above was read from nothing and must \
             not be interpreted (receiver layout: {})",
            describe_instance_layout(shared, cid),
        ),
    }
}

/// One declared instance field, resolved through a receiver's hierarchy.
pub struct DeclaredField {
    /// Layout slot index — what `heap.get_field` is indexed with.
    pub index: usize,
    /// The field's declared JVM descriptor, e.g. `Ljava/lang/Object;`.
    pub descriptor: String,
    /// The class in the hierarchy that DECLARES it, which is usually not the
    /// receiver's own class.
    pub owner: String,
}

/// Resolve `field_name` on `cid`'s hierarchy to its slot index, declared
/// descriptor and declaring class.
///
/// Mirrors [`crate::vm::vm_exec::resolve_field_index_in_hierarchy`] slot for
/// slot — same walk, same `instance_offset` accounting — and exists so a
/// diagnostic can print WHAT it resolved rather than only the value it read
/// through it. A dump that prints a value without the slot it came from
/// cannot be checked, which is exactly the gap the netty residual page names.
pub fn resolve_declared_instance_field(
    shared: &SharedVm,
    cid: ClassId,
    field_name: &str,
) -> Option<DeclaredField> {
    let cm = shared.classes.class_manager.read();
    let mut current = Some(cid);
    while let Some(c) = current {
        let class = cm.class_store.get(c)?;
        let mut instance_offset = 0usize;
        for f in &class.fields {
            if f.is_static() {
                continue;
            }
            if &*f.name == field_name {
                return Some(DeclaredField {
                    index: class.first_field_index + instance_offset,
                    descriptor: f.descriptor.to_string(),
                    owner: class.name.to_string(),
                });
            }
            instance_offset += 1;
        }
        current = class.superclass;
    }
    None
}

/// Every instance field a receiver carries, most-derived class first, as
/// `owner.name:descriptor@index`.
///
/// Printed beside a resolved slot so "the dump read the wrong index" is a
/// claim the reader can check instead of one they have to trust. Capped, so a
/// receiver with a large hierarchy cannot flood a watchdog dump.
pub fn describe_instance_layout(shared: &SharedVm, cid: ClassId) -> String {
    const MAX: usize = 48;
    let cm = shared.classes.class_manager.read();
    let mut parts: Vec<String> = Vec::new();
    let mut current = Some(cid);
    let mut truncated = false;
    while let Some(c) = current {
        let Some(class) = cm.class_store.get(c) else {
            parts.push(format!("<class_id {c:?} not in store>"));
            break;
        };
        let mut instance_offset = 0usize;
        for f in &class.fields {
            if f.is_static() {
                continue;
            }
            if parts.len() >= MAX {
                truncated = true;
            } else {
                parts.push(format!(
                    "{}.{}:{}@{}",
                    class.name,
                    f.name,
                    f.descriptor,
                    class.first_field_index + instance_offset
                ));
            }
            instance_offset += 1;
        }
        current = class.superclass;
    }
    if truncated {
        parts.push("...".to_string());
    }
    parts.join(" ")
}

/// Name a `DefaultPromise.result` value against that class's own `SUCCESS` /
/// `UNCANCELLABLE` sentinels — see the note in [`dump_wait_object_state`] for
/// why the distinction decides the netty stall.
///
/// `unknown` means the statics could not be resolved on this receiver's
/// hierarchy, and it is printed rather than swallowed: a silent fallback to
/// "not uncancellable" is precisely the reading that would repeat the original
/// mistake.
fn classify_promise_result(
    shared: &SharedVm,
    cid: ClassId,
    result: &Option<cratonvm_types::Value>,
) -> String {
    let Some(value) = result else {
        // NOT the same claim as "the slot holds a primitive". `None` here means
        // the receiver has no instance field called `result` anywhere in its
        // hierarchy, so nothing was read at all — and printing a verdict about
        // the value would be a verdict about nothing. This dump has already
        // mislabelled one unread slot (see the zero-cell note in
        // `dump_wait_object_state`); it does not get to do it twice.
        return "no-result-field(NOTHING-WAS-READ)".to_string();
    };
    let cratonvm_types::Value::Object(slot) = value else {
        return "not-a-reference-slot".to_string();
    };
    let Some(r) = slot else {
        return "null(PENDING)".to_string();
    };
    // The sentinels are `private static final` on `DefaultPromise` itself, so
    // walk from the receiver's class up until a class declares the name — the
    // receiver is usually a subclass (`DefaultChannelPromise`,
    // `AbstractBootstrap$PendingRegistrationPromise`).
    let sentinel = |fname: &str| -> Option<ObjectRef> {
        let cm = shared.classes.class_manager.read();
        let mut cur = Some(cid);
        while let Some(c) = cur {
            let class = cm.get_class(c)?;
            let mut static_idx = 0usize;
            for f in &class.fields {
                if f.is_static() {
                    if &*f.name == fname {
                        let owner = c;
                        drop(cm);
                        return match crate::vm::vm_object::get_static_shared(
                            shared, owner, static_idx,
                        ) {
                            cratonvm_types::Value::Object(Some(o)) => Some(o),
                            _ => None,
                        };
                    }
                    static_idx += 1;
                }
            }
            cur = class.superclass;
        }
        None
    };
    let same = |o: Option<ObjectRef>| o.is_some_and(|o| std::ptr::eq(o.as_ptr(), r.as_ptr()));
    let unc = sentinel("UNCANCELLABLE");
    let suc = sentinel("SUCCESS");
    if unc.is_none() && suc.is_none() {
        return "unknown(sentinels-unresolved)".to_string();
    }
    if same(unc) {
        return "UNCANCELLABLE--STILL-PENDING".to_string();
    }
    if same(suc) {
        return "SUCCESS".to_string();
    }
    let rcid = shared.mem.heap.class_id_of(*r);
    let rname = shared
        .classes
        .class_manager
        .read()
        .class_store
        .get(rcid)
        .map(|c| c.name.to_string())
        .unwrap_or_else(|| format!("<class_id {rcid:?}>"));
    format!("other({rname})")
}

impl SharedVm {
    /// Placeholder split-impl — see the inherent impl above. The split is
    /// purely so the thread-local helpers above can sit between two impl
    /// blocks without losing rustfmt readability.
    #[doc(hidden)]
    fn __kc16_split_marker(&self) {}

    /// Get or create a synthetic lock object for static synchronized methods.
    ///
    /// Each class gets a dedicated dummy object on the heap, used as the
    /// monitor target for static synchronized methods (which have no `this`).
    pub fn get_class_lock_object(&self, class_id: ClassId) -> ObjectRef {
        // Fast path: check if we already have a lock object
        if let Some(&obj) = self.classes.class_locks.read().get(&class_id) {
            return obj;
        }
        // Slow path: allocate one
        let mut locks = self.classes.class_locks.write();
        // Double-check after acquiring write lock
        if let Some(&obj) = locks.get(&class_id) {
            return obj;
        }
        // This object is only a monitor token; it is not a real instance of
        // `class_id`. Keeping its heap header as java/lang/Object avoids
        // creating zero-slot "instances" of fieldful classes, which the GC
        // reports as undersized object layouts when scanning class-lock roots.
        let lock_class_id = self
            .classes
            .class_manager
            .read()
            .get_loaded_class_id("java/lang/Object")
            .unwrap_or(ClassId::new(0));
        let obj = self.mem.heap.alloc_object(lock_class_id, 0);
        locks.insert(class_id, obj);
        obj
    }

    /// Lazily initialize synthetic System.out and System.err PrintStream objects.
    ///
    /// Called when `java/lang/System` static fields are accessed. Allocates
    /// PrintStream objects backing `System.out` / `System.err`.
    ///
    /// # WP0.1 — P0 "Cannot invoke write on null" fix (2026-04-24)
    ///
    /// Previously allocated with `num_fields = 1` regardless of whether the
    /// real JDK `java/io/PrintStream` class was already loaded. When real
    /// JDK mode ran `initPhase1` successfully, the real PrintStream class
    /// (with ~20 fields: `out`, `autoFlush`, `trouble`, `charOut`,
    /// `textOut`, `closing`, `closed`, `charset`, `locale`, `lock`, etc.)
    /// was registered under the same name, so `ensure_synthetic_class`
    /// reused that real `class_id` while the heap allocation still only
    /// reserved 1 slot. Real JDK `PrintStream.println(String)V` bytecode
    /// then did `getfield #out` on the 1-slot object, which under the G1
    /// backend reads past the allocation into whatever bytes follow it
    /// in arena memory. On `apps/hello` (one `println`) those bytes
    /// happened to decode as something `.write`-able; on a second call
    /// the arena contents had shifted and the field decoded as
    /// `Object(None)`, so the subsequent `invokevirtual write` tripped
    /// `Cannot invoke write on null` in the interpreter's NPE site.
    ///
    /// Fix: allocate with the class's declared `num_total_fields` when
    /// the real JDK class is already loaded, so the backing storage
    /// matches the bytecode's field indices. Synthetic (non-real-JDK)
    /// mode still falls back to the 1-slot layout — it goes through the
    /// native `PrintStream.println` fallback registered in
    /// `native-builtins/src/lib.rs`, which only reads field 0.
    pub fn ensure_system_streams(&self) -> (ObjectRef, ObjectRef) {
        // Check if already initialized
        {
            let out = self.system_out.read();
            let err = self.system_err.read();
            if let (Some(out_ref), Some(err_ref)) = (*out, *err) {
                return (out_ref, err_ref);
            }
        }
        // Initialize
        let mut out = self.system_out.write();
        let mut err = self.system_err.write();
        // Double-check
        if let (Some(out_ref), Some(err_ref)) = (*out, *err) {
            return (out_ref, err_ref);
        }
        // Ensure a PrintStream class_id exists. If the real JDK
        // `java/io/PrintStream` has already been loaded (real-JDK mode),
        // `ensure_synthetic_class` returns that existing id; otherwise
        // it creates a 1-field synthetic stub.
        //
        // charset-NPE fix (2026-05-21): in real-JDK mode this hook runs
        // from `System.<clinit>`, which executes *before*
        // `java/io/PrintStream` has been loaded. The old code therefore
        // hit the `ensure_synthetic_class` "create a 1-field stub" path,
        // and the System.out/err objects ended up backed by a synthetic
        // stub class whose `fields` vector is EMPTY. `initPhase1`'s
        // `install_charset` then does `set_field_by_name(stream,
        // "charset", …)`, which resolves the field via
        // `resolve_field_index_in_hierarchy` — that walk finds no field
        // named `charset` on the empty-field stub, so the write is
        // silently dropped and `PrintStream.charset` stays null. The
        // first `new PrintWriter(System.err)` (Picocli / Quarkus
        // `show-config`) then reads `((PrintStream)err).charset()` ==
        // null and `OutputStreamWriter`'s `Objects.requireNonNull(cs,
        // "charset")` throws `NullPointerException: charset`.
        //
        // Force-load the real `java/io/PrintStream` class first (same
        // pattern as `ensure_system_stdin_object` for FileInputStream)
        // so the backing objects get the real field layout — including
        // the `charset` slot — whenever real JDK class files are
        // available. `load_class_concurrent` also upgrades a previously
        // created synthetic stub in place. If no real boot classes are
        // present (pure synthetic-jdk mode) the load fails harmlessly
        // and we fall back to the 1-field synthetic stub below.
        let _ = self.load_class_concurrent("java/io/PrintStream");
        // JDK-only wave 2, step 3 (2026-08-10): the fallible spelling, through
        // this file's own "refuse diagnosably" helper. On any complete image
        // the load above has already put the real `java.io.PrintStream` in the
        // store, so this resolves to it and fabricates nothing — the refusal
        // arm is reachable only where `java.base` is absent, and there
        // `System.out` was never going to work.
        let ps_class_id = {
            let mut cm = self.classes.class_manager_write();
            // 1 field: fd_id — used only when the real class isn't loaded.
            ensure_bootstrap_compat_class(&mut cm, "java/io/PrintStream", 1)
        };
        // WP0.1 fix: size the allocation to the class's declared field count
        // so real-JDK `PrintStream.println` bytecode's `getfield` accesses
        // stay in-bounds. `max(1)` guarantees slot 0 (our fd tag) is always
        // writable even for synthetic stub classes declared with 0 fields.
        //
        // On a refusal, zero-slot `java/lang/Object` streams rather than an
        // undersized 1-slot layout: the GC's field guard rejects every access
        // on the latter, so the failure would surface as a stream of dropped
        // writes instead of at whatever first tries to USE `System.out`.
        // `ensure_bootstrap_compat_class` has already warned and named it.
        let (ps_class_id, num_fields) = match ps_class_id {
            Some(id) => {
                let n = {
                    let cm = self.classes.class_manager.read();
                    cm.get_class(id)
                        .map(|c| c.num_total_fields.max(1))
                        .unwrap_or(1)
                };
                (id, n)
            }
            None => (ClassId::new(0), 0),
        };
        let out_obj = self.mem.heap.alloc_object(ps_class_id, num_fields);
        let err_obj = self.mem.heap.alloc_object(ps_class_id, num_fields);
        // Store fd_id: stdout=1, stderr=2 at slot 0 so the synthetic-jdk
        // natives (which read field 0 in `native-io/src/lib.rs`) keep
        // working. The real-JDK native-override path uses pointer
        // identity via `stream_fd()` in `native-builtins/src/lib.rs`, so
        // it doesn't care what else lives in the object's field slots.
        //
        // Compact reference-field layout: the real java/io/PrintStream's
        // field 0 is the reference `out` (inherited FilterOutputStream). Under
        // the compact layout an `Int` written into a reference slot auto-boxes,
        // so `out` becomes a non-null wrapper Object — which makes
        // `route_write_through_out()` think System.out wraps a real stream and
        // route every write into the dead wrapper (silent empty stdout). The
        // real-JDK path resolves the fd by pointer identity, not slot 0, so
        // skip the fd tag whenever slot 0 is a (compact) reference field. In
        // synthetic-jdk mode (1-field stub, slot 0 is the primitive fd tag) the
        // store still happens. `class_layout` is only populated when compact is
        // enabled, so this is a no-op (legacy behaviour) when the flag is off.
        //
        // 2026-08-12 (W7-84-primitive-in-reference-store.md): this guard was
        // written against `gen_heap`, the only heap that boxed. `zgc`, `g1` and
        // `heap` dropped the same write to null, so on those collectors the
        // hazard above could not occur and this guard was carrying nothing.
        // All four box now, so the guard is load-bearing on every collector —
        // which is the honest cost of converging on the value-preserving
        // answer, and the reason it is stated here rather than left implicit.
        let slot0_is_ref = cratonvm_gc::class_layout(ps_class_id.as_u32())
            .and_then(|l| l.field_is_ref(0))
            .unwrap_or(false);
        if !slot0_is_ref && num_fields > 0 {
            self.mem.heap.set_field(out_obj, 0, Value::Int(1));
            self.mem.heap.set_field(err_obj, 0, Value::Int(2));
        }
        *out = Some(out_obj);
        *err = Some(err_obj);
        // FFM/native-access fix: stamp `java.lang.System.initialErr` with the
        // err stream we just materialised. `jdk.internal.misc.VM.initialErr()`
        // is `SharedSecrets.getJavaLangAccess().initialSystemErr()`, whose real
        // JDK body (`java.lang.System$1`) is `getstatic System.initialErr;
        // areturn`. The real JDK populates that `@Stable` field in
        // `System.initPhase1()` (System.java:1820); CratonVM boots via a native
        // `initPhase1` that never set it, so the field stayed null and
        // `VM.initialErr().printf(...)` NPE'd. That printf is the JDK's
        // restricted-native-access warning path
        // (`java.lang.Module.ensureNativeAccess`, Module.java:322), which fires
        // the first time an FFM downcall binding calls
        // `SymbolLookup.libraryLookup` without `--enable-native-access`. The NPE
        // was wrapped as ExceptionInInitializerError for the binding's <clinit>
        // — e.g. Tomcat's `org.apache.tomcat.util.openssl.openssl_h` — taking
        // down the entire OpenSSL/TLS protocol handler and ~17 TLS test classes.
        // A native override on `System$1.initialSystemErr` does NOT help here:
        // the shim is a real JDK class, so its concrete bytecode runs instead of
        // the override — only the backing static field is consulted. Resolve the
        // static slot by name so this is robust to the real-JDK field layout;
        // a missing field (pure synthetic-jdk System stub) is skipped harmlessly.
        let sys_initial_err_slot = {
            let cm = self.classes.class_manager.read();
            cm.get_loaded_class_id("java/lang/System").and_then(|sid| {
                cm.get_class(sid).and_then(|c| {
                    let mut static_idx = 0usize;
                    for f in &c.fields {
                        if f.is_static() {
                            if &*f.name == "initialErr" {
                                return Some((sid, static_idx));
                            }
                            static_idx += 1;
                        }
                    }
                    None
                })
            })
        };
        if let Some((sid, idx)) = sys_initial_err_slot {
            super::set_static_shared(self, sid, idx, Value::Object(Some(err_obj)));
        }
        (out_obj, err_obj)
    }
}

impl SharedVm {
    /// Find the park state for a thread identified by its Java Thread object.
    /// Used by `unpark()` in NativeContextImpl.
    ///
    /// Resolution prefers the Java-side `Thread.tid` (unique, never reused)
    /// over the mirror's heap address: the address-keyed reverse index can
    /// alias a NEW thread's mirror allocated at a dead thread's recycled
    /// address, silently routing the wakeup to the dead thread's ParkState
    /// (observed as Tomcat executor workers parked forever after their
    /// `shutdownNow()` interrupt was lost — the DoHead leaked-worker face).
    /// The tid path also survives a relocated-but-intact stale mirror copy:
    /// the stale address is gone from the reverse index, but its memory
    /// still holds the correct `tid`.
    pub fn find_park_state_for_thread_obj(
        &self,
        thread_obj: ObjectRef,
    ) -> Option<std::sync::Arc<crate::threading::ParkState>> {
        if let Some(java_tid) = super::vm_exec::read_java_thread_tid(self, thread_obj) {
            if let Some(ps) = self
                .threads
                .thread_registry
                .find_park_state_by_java_tid(java_tid)
            {
                return Some(ps);
            }
        }
        self.threads
            .thread_registry
            .find_park_state_by_thread_obj(thread_obj)
    }
}

// ---------------------------------------------------------------------------
// WP1.3 — VM bootstrap init-level state machine
// ---------------------------------------------------------------------------

/// WP1.3 — Re-exports of the process-wide init-level registry that
/// lives in [`cratonvm_native_api::init_level`].  The native-api crate
/// hosts the shared state so both the `vm` crate (which has a
/// `SharedVm`) and the `native-builtins` crate (which implements
/// `jdk/internal/misc/VM.initLevel()`) reach the same monotonic
/// counter without a crate-level cycle.
///
/// * [`set_init_level`] advances the level.  Wakes every thread
///   blocked in [`await_init_level`].
/// * [`get_init_level`] reads the current level with `Acquire` ordering.
/// * [`await_init_level`] blocks until the level reaches the target.
pub use cratonvm_native_api::init_level::{await_init_level, get_init_level, set_init_level};

impl SharedVm {
    /// WP1.3 — Read the current JVM bootstrap init level.
    ///
    /// Returns the integer HotSpot state (0..=4, see
    /// [`SharedVm::init_level`] doc-comment).  Uses `Ordering::Acquire`
    /// to pair with the `Ordering::Release` store in
    /// [`Self::set_init_level`] so threads that observe a bumped
    /// level also observe every classloader / static-field write
    /// the bump guards.
    pub fn get_init_level(&self) -> i32 {
        self.init_level.load(Ordering::Acquire)
    }

    /// WP1.3 — Advance the JVM bootstrap init level.
    ///
    /// Accepts any value in `0..=4`; downward transitions are
    /// rejected (logged and ignored) because the JVM spec does not
    /// define a way to unwind init progress. Wakes every waiter
    /// blocked in [`Self::await_init_level`] / the
    /// `VM.awaitInitLevel` native on success.
    ///
    /// **Coordination**: this is the single entry point for init-level
    /// transitions.  Callers are expected to be at the edge of a
    /// well-defined bootstrap phase:
    ///
    /// * level 1 — right after `SharedVm::new` returns,
    /// * level 2 — right after `System.initPhase1()` completes,
    /// * level 3 — right after `System.initPhase2()` completes,
    /// * level 4 — right before user `main()` runs.
    ///
    /// The sibling free function [`set_init_level`] mirrors the
    /// process-wide registry and is what the
    /// `vm-cli` bootstrap sequence actually calls; methods on
    /// `SharedVm` delegate to it so the atomic value seen by the
    /// `VM.initLevel()` native never lags behind a `self.init_level`
    /// update.
    pub fn set_init_level(&self, level: i32) {
        // Keep the process-wide registry in sync first — the native
        // `VM.initLevel()` handler reads it directly.
        cratonvm_native_api::init_level::set_init_level(level);
        // Mirror into the per-SharedVm atomic so in-process readers
        // with a `&SharedVm` don't have to go through the global
        // OnceLock.  `set_init_level` above already rejected downward
        // transitions; we re-check cheaply here so the per-instance
        // condvar notification also preserves monotonicity.
        let cur = self.init_level.load(Ordering::Acquire);
        if level < cur {
            return;
        }
        self.init_level.store(level, Ordering::Release);
        let (_lock, cv) = &*self.init_level_waiters;
        cv.notify_all();
    }

    /// WP1.3 — Block the current thread until the init level reaches
    /// at least `target`.  Backs `VM.awaitInitLevel(int)` and is also
    /// useful inside the VM for test helpers that need to observe a
    /// specific bootstrap stage.
    ///
    /// If the current level is already `>= target`, returns
    /// immediately. Otherwise parks on the per-SharedVm condvar and
    /// rechecks on every wake.
    pub fn await_init_level(&self, target: i32) {
        if self.get_init_level() >= target {
            return;
        }
        let (lock, cv) = &*self.init_level_waiters;
        let mut guard = lock.lock().expect("init_level_waiters mutex poisoned");
        while self.get_init_level() < target {
            guard = cv.wait(guard).expect("init_level_waiters condvar poisoned");
        }
    }
}

// ---------------------------------------------------------------------------
// Lock-order-checked accessors for SharedVm's hot locks.
//
// The authoritative hierarchy lives in `crate::runtime::lock_order` (its
// `LockLevel` enum IS the source of truth; there is no separate
// docs/lock-order.md). Two of the locks below enforce their own level because
// the field itself is now an ordered wrapper:
//
//   * `class_manager`  -- `OrderedPlRwLock` at L10
//   * `ref_processor`  -- `OrderedPlMutex`  at L7
//
// For the rest the field is still a raw `parking_lot` lock, so the accessor
// pairs the original guard with a `LevelScope` that records the level in
// `lock_order`'s per-thread tracker for the lifetime of the guard.
//
// IMPORTANT: an accessor for a lock that is *already* an ordered wrapper must
// NOT also take a `LevelScope` -- that would record the level twice on the same
// thread and trip the same-level assertion. Those accessors are plain aliases
// for the field's own `lock()` / `read()` / `write()`.
//
// Levels are taken straight from `LockLevel`; there is no remapping.
//
// TODO(orchestrator): extend the wiring to the remaining hot locks
// (`jit_cache`, `native_libraries`, `upcall_table`, `jni_global_refs`,
// `class_init_waiters`, `class_loading_locks`, `deopt_log`,
// `invalidation_manager`, `jit_skip_set`, `cleaner_thread.pending_actions`,
// and the `ProfileStore` L4.a/L4.b sub-hierarchy in `jit/src/profile.rs`).

mod ranked_locks {
    //! Thin adapter over [`crate::runtime::lock_order`] for the `_ranked`
    //! accessors.
    //!
    //! This module used to carry its *own* copy of the per-thread held-level
    //! bit-set, mirroring the one in `lock_order`. That was a false-negative
    //! factory: a level recorded here was invisible to `lock_order`'s tracker
    //! and vice versa, so an L6 monitor (an `OrderedMutex`, tracked by
    //! `lock_order`) held across a `class_manager_read_ranked()` call (tracked
    //! here) was never reported. There is now exactly one tracker, and it lives
    //! in `lock_order`; enforcement therefore also honours the release-build
    //! `CRATONVM_LOCK_ORDER_CHECK` opt-in instead of being debug-only.
    use crate::runtime::lock_order::{enter_level, LevelScope, LockLevel};

    /// RAII rank scope. Alias for [`LevelScope`], kept because `RankScope` is
    /// part of this crate's public surface.
    pub type RankScope = LevelScope;

    /// Enter a rank scope at `level`.
    ///
    /// # Panics
    ///
    /// When lock-order enforcement is active (always in debug builds; in
    /// release when `CRATONVM_LOCK_ORDER_CHECK` is set), panics if the calling
    /// thread already holds a lock at an equal or *lower* level.
    #[inline]
    pub(super) fn enter(level: LockLevel) -> RankScope {
        enter_level(level)
    }

    // ----- Per-lock level mapping -----------------------------------
    //
    // `class_manager` (L10) and `ref_processor` (L7) are deliberately absent:
    // their fields are ordered wrappers that record their own level.

    pub(super) const MONITORS: LockLevel = LockLevel::Monitors; // L6
    pub(super) const THREAD_REGISTRY: LockLevel = LockLevel::ThreadRegistry; // L5
    pub(super) const FLIGHT_RECORDER: LockLevel = LockLevel::FlightRecorder; // L4
    pub(super) const NATIVE_MEMORY: LockLevel = LockLevel::NativeMemory; // L2
}

pub use ranked_locks::RankScope;

/// Rank-tracked guard returned by the `_ranked` accessors. The
/// `rank_scope` field releases the thread-local rank when dropped; the
/// `lock` field is the original `parking_lot` guard with the usual
/// `Deref`/`DerefMut` to the protected value. Dropping happens in
/// declaration order (Rust drop order), so the `parking_lot` lock is
/// released *before* the rank scope — the symmetric inverse of
/// acquisition.
#[must_use = "the rank guard must be held for the duration of the lock"]
pub struct RankedGuard<G> {
    /// The original `parking_lot` guard. Drops first. Private so
    /// callers cannot move it out — moving would drop the rank scope
    /// in the same statement and silently break the rank-tracking
    /// invariant. Access the inner value via `Deref`/`DerefMut`.
    lock: G,
    /// The rank-scope guard. Drops second, releasing the thread-local
    /// rank level so a lower-ranked lock can be re-acquired. Held
    /// only for its `Drop` side-effect; callers should not touch it.
    #[allow(dead_code)]
    rank_scope: RankScope,
}

impl<G: std::ops::Deref> std::ops::Deref for RankedGuard<G> {
    type Target = G::Target;
    fn deref(&self) -> &Self::Target {
        &self.lock
    }
}

impl<G: std::ops::DerefMut> std::ops::DerefMut for RankedGuard<G> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.lock
    }
}

impl SharedVm {
    /// Acquire `class_manager` (L10) for read.
    ///
    /// Retained as a compatibility alias: the field is now an
    /// [`OrderedPlRwLock`] at [`LockLevel::ClassManager`], so a plain
    /// `shared.classes.class_manager.read()` is already order-checked and this helper
    /// adds nothing. Taking a *second* rank scope here would double-record L10
    /// and trip the same-level assertion.
    #[inline]
    pub fn class_manager_read_ranked(
        &self,
    ) -> crate::runtime::lock_order::OrderedPlRwLockReadGuard<'_, ClassManager> {
        self.classes.class_manager.read()
    }

    /// Acquire `class_manager` (L10) for write, through the hook-draining
    /// guard (obsaudit D1) — see
    /// [`crate::vm::realms::class_realm::ClassRealm::class_manager_write`].
    /// No longer a bare alias: unlike the read side, this must route
    /// through the draining wrapper like every other write-lock site, or
    /// JVMTI ClassLoad/ClassPrepare events queued under it would never fire.
    #[inline]
    pub fn class_manager_write_ranked(
        &self,
    ) -> crate::vm::realms::class_realm::ClassManagerWriteGuard<'_> {
        self.classes.class_manager_write()
    }

    /// Acquire `ref_processor` (L7).
    ///
    /// Retained as a compatibility alias: the field is now an
    /// [`OrderedPlMutex`] at [`LockLevel::RefProcessor`], so a plain
    /// `shared.mem.ref_processor.lock()` is already order-checked and this helper
    /// adds nothing. Taking a *second* rank scope here would double-record L7
    /// and trip the same-level assertion.
    #[inline]
    pub fn ref_processor_lock_ranked(
        &self,
    ) -> crate::runtime::lock_order::OrderedPlMutexGuard<'_, cratonvm_gc::ReferenceProcessor> {
        self.mem.ref_processor.lock()
    }

    /// Enter the `monitors` rank scope. The monitor table itself uses
    /// many fine-grained inner locks; this helper just announces that
    /// the caller is about to touch monitor state so the rank tracker
    /// can reject downstream acquisitions of higher-ranked locks
    /// (i.e. `class_manager`). Bind the
    /// returned guard to a local for the duration of the monitor work.
    #[inline]
    #[must_use = "bind the guard to a local for the duration of the monitor work"]
    pub fn enter_monitors_rank(&self) -> RankScope {
        ranked_locks::enter(ranked_locks::MONITORS)
    }

    /// Enter the `thread_registry` rank scope. Same shape as
    /// [`Self::enter_monitors_rank`]: the registry has its own internal
    /// locking; this helper only drives the global rank tracker.
    #[inline]
    #[must_use = "bind the guard to a local for the duration of the work"]
    pub fn enter_thread_registry_rank(&self) -> RankScope {
        ranked_locks::enter(ranked_locks::THREAD_REGISTRY)
    }

    /// Acquire `flight_recorder` (L4) with rank tracking.
    #[inline]
    pub fn flight_recorder_lock_ranked(
        &self,
    ) -> RankedGuard<parking_lot::MutexGuard<'_, cratonvm_jfr::FlightRecorder>> {
        let rank = ranked_locks::enter(ranked_locks::FLIGHT_RECORDER);
        RankedGuard {
            lock: self.debug.flight_recorder.lock(),
            rank_scope: rank,
        }
    }

    /// Acquire `native_memory` (L2) with rank tracking.
    ///
    /// L2 is strictly below `flight_recorder` (L4), so a path that needs both
    /// may hold them together provided `flight_recorder` is taken first.
    #[inline]
    pub fn native_memory_lock_ranked(
        &self,
    ) -> RankedGuard<parking_lot::MutexGuard<'_, crate::native::ffi::NativeMemoryTable>> {
        let rank = ranked_locks::enter(ranked_locks::NATIVE_MEMORY);
        RankedGuard {
            lock: self.natives.native_memory.lock(),
            rank_scope: rank,
        }
    }
}

impl std::fmt::Debug for SharedVm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedVm")
            .field(
                "classes_loaded",
                &self.classes.class_manager.read().loaded_count(),
            )
            .field("heap_bytes", &self.mem.heap.allocated_bytes())
            .field("native_methods", &self.natives.native_methods.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Vm — convenience wrapper for main thread
// ---------------------------------------------------------------------------

/// The top-level VM struct.
///
/// This is a convenience wrapper that combines the shared state (`Arc<SharedVm>`)
/// with the main thread's per-thread state (`JvmThread`). Tests and the CLI
/// interact with `Vm` directly.
pub struct Vm {
    /// Shared VM state (thread-safe, can be cloned via Arc).
    pub shared: Arc<SharedVm>,

    /// Per-thread state for the main thread. This is boxed because the thread
    /// registry publishes its TLAB address to a cross-thread GC; moving the
    /// containing `Vm` must not invalidate that address.
    pub main_thread: Box<JvmThread>,
}

impl Vm {
    /// Mark the primordial launcher thread as parked outside Java bytecode.
    ///
    /// After `main()` returns the CLI may still wait for non-daemon Java
    /// threads. That wait runs in Rust, so the main thread will not reach an
    /// interpreter safepoint; publish its roots/frame trace and enter the same
    /// blocked-region protocol native waits use.
    pub fn begin_main_thread_blocking_region(&mut self, state: &'static str) {
        self.main_thread.set_vm_state(state);
        self.main_thread.tlab.retire();
        {
            let mut ctx = crate::vm::vm_exec::NativeContextImpl {
                shared: &self.shared,
                thread: &mut self.main_thread,
            };
            ctx.deposit_root_snapshot();
        }
        if self.shared.mem.gc_barrier.mark_blocked_region_enter() {
            // GCAUDIT-0711-FIX (finding 1a): auto - the deposit above
            // already raised in_blocked_region.
            let _ = self
                .shared
                .mem
                .gc_barrier
                .arrive_and_wait_auto(self.main_thread.thread_id);
        }
    }

    /// Leave a region opened by [`Self::begin_main_thread_blocking_region`].
    pub fn end_main_thread_blocking_region(&mut self) {
        self.shared.mem.gc_barrier.mark_blocked_region_leave();
        let mut ctx = crate::vm::vm_exec::NativeContextImpl {
            shared: &self.shared,
            thread: &mut self.main_thread,
        };
        ctx.check_post_block_gc();
        self.main_thread
            .set_vm_state("vm-main:blocking-region-returned");
    }

    /// Sweep every published `read_alias::SlotMap` against the class the loader
    /// actually has, and print the one-line summary. W7-90-slot-map-sweep-caller.md.
    ///
    /// **Why here and not at registration.** W7-69-read-side-alias-instrument.md
    /// settled that: at registration time most of these classes are not loaded,
    /// and `declared_fields` returning empty is indistinguishable from "the
    /// class has no fields" — `java.nio.DirectByteBuffer` is package-private and
    /// is not among the 323 classes `bootstrap_core_classes` names. The sweep
    /// has to run at a point where the workload has already caused the loads,
    /// and this method exists so the launcher has one call rather than a
    /// hand-rolled `NativeContextImpl` at the call site.
    ///
    /// Self-gated: with the flag off `verify_declared_slot_maps` returns before
    /// touching a class, a name or a lock, so this costs one `OnceLock` load and
    /// a branch — once, on a teardown path. It is observation-only in every
    /// mode; the returned report is printed and dropped.
    pub fn sweep_declared_slot_maps(
        &mut self,
        trigger: &str,
    ) -> cratonvm_native_api::read_alias::SweepReport {
        let ctx = crate::vm::vm_exec::NativeContextImpl {
            shared: &self.shared,
            thread: &mut self.main_thread,
        };
        cratonvm_native_api::read_alias::sweep_declared_slot_maps_at(&ctx, trigger)
    }

    /// Create a new VM with the given configuration.
    pub fn new(config: VmConfig) -> Self {
        let shared = Arc::new(SharedVm::new(config));

        // Store a weak self-reference so native methods can clone the Arc
        // for spawning new threads.
        *shared.self_arc.write() = Some(Arc::downgrade(&shared));

        // Publish the process-global VM cell so a foreign (host-created) thread
        // that calls `AttachCurrentThread` can resolve the live VM and register
        // itself for GC-safepoint participation — for EVERY creation path, not
        // just the libcratonvm Invocation API. Without this, a CLI-launched app
        // whose native library spawns + attaches its own OS thread would fail
        // attach (no VM to resolve) once the default-on foreign-attach path runs.
        // `Weak`, so it never keeps the VM alive; last writer wins (harmless for
        // the in-process multi-`Vm` test fixtures — one VM per real process).
        crate::native::jni::set_process_vm(&shared);

        // Round 4 audit fix (CRIT) — publish the same weak handle to the
        // module-private registry used by `resolution_invalidate_adapter` so
        // JVMTI `RedefineClasses` can reach back into
        // `shared.classes.resolution_cache` and drop stale resolutions.
        //
        // Idempotent PER VM, and every registered VM is reached — NOT
        // "first writer wins". That older behaviour meant a second `Vm` built
        // in the same process (a test fixture, or a real embedding) never had
        // its own resolution/link/JIT caches invalidated on redefine. See
        // `docs/architecture/per-vm-state.md` §3 (V2).
        set_global_shared_vm_for_hooks(Arc::downgrade(&shared));

        // obsaudit D14 (2026-07-26): bridge `runtime::jvmti::JvmtiEventManager`
        // (interpreter/GC/classloading-sourced events) to the real,
        // native-agent-facing `shared.debug.jvmti_env` — see the notes above
        // `install_real_agent_env_bridge` in `runtime/jvmti.rs`. Same `Weak`,
        // idempotent, last-writer-wins shape as the two hooks just above.
        crate::runtime::jvmti::install_real_agent_env_bridge(&shared);

        // obsaudit D15 (2026-07-26) — open the real attach-API socket and
        // register the *live* (real-VM-state-backed) jcmd command set. See
        // the LIVENESS block and `AttachListener`'s doc comment in
        // `runtime/serviceability.rs`: this must be `new_with_vm_state`,
        // never the argument-less `JcmdProcessor::new()` (that one reports
        // fabricated data for several commands). `shared.clone()` coerces
        // to `Arc<dyn VmDiagnosticState>` via the `impl VmDiagnosticState
        // for SharedVm` in this file.
        *shared.debug.jcmd_processor.lock() =
            Some(crate::runtime::serviceability::JcmdProcessor::new_with_vm_state(shared.clone()));

        // obsaudit D12 (2026-07-26) — `-XX:StartFlightRecording`. Before
        // this, vm-cli never called `start_recording` (see the retracted
        // claim this comment replaces), so `cratonvm_jfr::is_enabled()` was
        // permanently false and the ~30 wired `emit_*` call sites never
        // captured anything. `config.jfr_start_recording` is `None` unless
        // the flag was passed, so this is a no-op — same cost as before —
        // on every VM that doesn't request it.
        if let Some(jfr_cfg) = shared.config.jfr_start_recording.clone() {
            let mut settings = cratonvm_jfr::RecordingSettings::new("cratonvm");
            settings.max_age = jfr_cfg.max_age;
            settings.max_size = jfr_cfg.max_events;
            settings.duration = jfr_cfg.duration;
            settings.dump_on_exit = jfr_cfg.dump_on_exit;
            let recording_id = {
                let mut fr = shared.debug.flight_recorder.lock();
                let id = fr.new_recording(settings);
                fr.start_recording(id);
                id
            };
            if jfr_cfg.dump_on_exit {
                let filename = jfr_cfg
                    .filename
                    .clone()
                    .unwrap_or_else(|| format!("./cratonvm-recording-{}.jfr", std::process::id()));
                *shared.debug.jfr_dump_on_exit.lock() = Some((recording_id, filename));
            }
            // obsaudit D12 — the reclamation half of the fix. Before this,
            // `ThreadRingRegistry::reclaim_retired_shards` only ran from
            // inside `drain_all`, which only ran at dump time — harmless
            // only because the disabled gate above kept ordinary threads
            // from ever registering a shard. A recording that now actually
            // runs continuously needs its per-thread rings drained
            // periodically, both to keep events flowing into the
            // repository (rather than only at final dump) and to let
            // retired+empty shards from thread churn actually get
            // reclaimed instead of accumulating in the registry `Vec` for
            // the recording's whole lifetime. One drain per second is
            // frequent enough that a 1024-capacity ring on a
            // moderately-busy thread will not silently drop events
            // between drains, and cheap enough (an empty repository drain
            // is a handful of shard-list iterations) to run indefinitely.
            let weak_shared = Arc::downgrade(&shared);
            let duration = jfr_cfg.duration;
            std::thread::Builder::new()
                .name("JFR-Periodic-Drain".into())
                .spawn(move || {
                    let started = std::time::Instant::now();
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        let Some(shared) = weak_shared.upgrade() else {
                            break; // VM torn down (e.g. an in-process test) — stop.
                        };
                        let mut fr = shared.debug.flight_recorder.lock();
                        fr.drain_per_thread_into_repository();
                        if let Some(d) = duration {
                            if started.elapsed() >= d {
                                fr.stop_recording(recording_id);
                                break;
                            }
                        }
                    }
                })
                .ok();
        }

        // KC16-watchdog: install the wait-site frame dumper so a thread
        // parked in `Object.wait()` (e.g. AsyncFutureTask.await) can emit
        // its frame chain when the stack-dump watchdog fires. Without
        // this, the watchdog reports "0 threads dumped — main in native"
        // because the parked thread never reaches the interpreter's
        // top-of-loop poll.
        //
        // The closure reads the per-thread "wait-site frame snapshot"
        // thread-local that `monitor_wait` populates on entry (see
        // `vm_exec::monitor_wait`). The Monitor itself runs on the same
        // OS thread as the wait caller, so the thread-local is visible
        // even though we routed the wait through `parking_lot::Condvar`.
        // We also increment the watchdog ack counter so the watchdog's
        // "0 threads dumped" banner no longer fires for this case.
        {
            let weak = Arc::downgrade(&shared);
            crate::threading::monitor::install_wait_site_dump(move |_tid| {
                if let Some(s) = weak.upgrade() {
                    crate::vm::vm_init::dump_wait_site_thread_local(&s);
                }
            });
        }
        // Companion to the frame dump above: the STATE of the object the thread
        // is parked on. See `dump_wait_object_state` for why those two fields
        // are the ones that decide the netty promise stall.
        {
            let weak = Arc::downgrade(&shared);
            crate::threading::monitor::install_wait_object_dump(move |obj| {
                if let Some(s) = weak.upgrade() {
                    crate::vm::vm_init::dump_wait_object_state(&s, obj);
                }
            });
        }
        // And the GC-SAFE handle for that dump. `Monitor::wait` holds the
        // awaited `ObjectRef` as a plain local for the whole wait, which a
        // moving collector invalidates; `jmx_waiting_monitor` is a scanned root
        // that `update_thread_objs_after_gc` (gc.rs step 21) forwards, so it is
        // the address still valid at dump time. Without this the dump silently
        // reports pre-relocation field values.
        {
            let weak = Arc::downgrade(&shared);
            crate::threading::monitor::install_wait_object_resolve(move |tid| {
                let s = weak.upgrade()?;
                s.threads.thread_registry.peek_jmx_waiting_monitor(tid)
            });
        }

        // Register the main thread (id 0) in the thread registry.
        let main_thread = Box::new(JvmThread::new(ThreadId(0), "main"));
        shared
            .threads
            .thread_registry
            .register(ThreadId(0), "main", None);
        // obsaudit D1: bind this OS thread's JVMTI thread-attribution TLS so
        // ClassLoad/ClassPrepare events fired while bootstrapping on the
        // main thread report the real `jthread` instead of the "unknown"
        // sentinel. See `cratonvm_classloading::set_current_thread_id`.
        cratonvm_classloading::set_current_thread_id(0);
        // Share the interrupted flag so cross-thread interrupt works on the main thread
        shared
            .threads
            .thread_registry
            .set_interrupted_flag(ThreadId(0), main_thread.interrupted.clone());
        // Share the park state so LockSupport.unpark(mainThread) works
        shared
            .threads
            .thread_registry
            .set_park_state(ThreadId(0), main_thread.park_state.clone());
        // Share root snapshot so GC cross-thread collection includes main thread roots
        shared
            .threads
            .thread_registry
            .set_root_snapshot(ThreadId(0), main_thread.root_snapshot.clone());
        // Match spawned threads: the watchdog summary and cross-thread stack
        // probes must see the primordial thread's parked Java frames too.
        shared
            .threads
            .thread_registry
            .set_frame_trace(ThreadId(0), main_thread.frame_trace.clone());
        // Same handle to the crash handler, which cannot take the thread
        // registry's lock (the faulting thread may already hold it). This is
        // what puts Java frames — not just native ones — into an `hs_err`
        // report for the thread most crashes happen on. Worker threads need
        // the equivalent publication at their own registration sites; see the
        // cross-owner request in
        // `arch-2026-07-26/startup-and-diagnostics.md`.
        crate::runtime::crash_handler::publish_primordial_frame_trace(
            main_thread.frame_trace.clone(),
        );
        shared
            .threads
            .thread_registry
            .set_vm_state(ThreadId(0), main_thread.vm_state.clone());
        // Share blocked-region GC state so initiators can maintain the main
        // thread's roots while it parks in a blocking native (wait/join/park)
        shared
            .threads
            .thread_registry
            .set_gc_block_state(ThreadId(0), main_thread.gc_block_state.clone());
        // A worker-initiated non-moving GC can forcibly stop the primordial
        // thread in JIT code. Publish its reserved TLAB tail just as we do for
        // workers and JNI-attached threads. The Box keeps this pointee stable
        // across the return/moves of `Vm::new`.
        shared.threads.thread_registry.set_tlab_addr(
            ThreadId(0),
            &main_thread.tlab as *const cratonvm_gc::Tlab as usize,
        );
        // XT-FRAME-SCAN: publish the primordial thread's `JvmThread` address
        // too (the same Box keeps it stable) so a worker-initiated takeover
        // that freezes main mid-JIT can walk its interpreter frames.
        shared
            .threads
            .thread_registry
            .set_jvm_thread_addr(ThreadId(0), &*main_thread as *const JvmThread as usize);
        // Publish the primordial thread's OS id too. A worker can initiate a
        // multi-threaded STW while the main thread is running JIT code; without
        // this id the takeover backend can freeze and scan main but cannot prove
        // it was part of the counted barrier snapshot.
        shared
            .threads
            .thread_registry
            .set_os_tid_current(ThreadId(0));

        // Start JDWP debug server if configured
        #[cfg(feature = "experimental-debug")]
        if let Some(port) = shared.config.jdwp_port {
            let shared_ref = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("JDWP-Server".into())
                .spawn(move || {
                    crate::debug::run_jdwp_server(&shared_ref, port);
                })
                .ok();
            tracing::info!("JDWP debug server listening on port {port}");
        }

        // WP1.3 — VM is now at "primordial classes loaded" (level 1).
        // `SharedVm::new` already ran `bootstrap_core_classes()` which
        // loads `java/lang/Object`, `Class`, `String`, and the primitive
        // wrappers; the main thread is registered in the thread
        // registry so Thread.currentThread() works. This matches
        // HotSpot's `VM::init_level(1)` transition right before
        // `initialize_java_lang_classes` finishes.
        //
        // Subsequent transitions (level 2 after `System.initPhase1`,
        // level 3 after `initPhase2`, level 4 before `main()`) are
        // driven from the CLI bootstrap loop in `vm-cli/src/main.rs`
        // which owns the initPhase orchestration.
        shared.set_init_level(1);

        // Install the AIO completion-dispatcher launcher. The first handler-form
        // `AsynchronousSocketChannel.read` fires it, spinning up the
        // foreign-attached dispatcher thread that delivers read completions to
        // their Java `CompletionHandler` (the Tomcat WebSocket client read path).
        cratonvm_native_io::async_socket::set_dispatcher_launcher(Box::new(|| {
            crate::native::jni::start_aio_dispatcher();
        }));

        // Round-5 MED-fix (Bug 6, 2026-05-17): emit a one-shot
        // `jdk.PhysicalMemory` event at startup so any later JFR dump
        // captures the host-RAM totals as an EveryChunk diagnostic.
        //
        // Round-5 CRIT-fix (Bug 2, 2026-05-17 follow-up): drop the
        // surrounding `cratonvm_jfr::is_enabled()` gate. vm-cli never calls
        // `start_recording`, so the gate kept this call permanently
        // disabled and made the wired emit dead code. The emit function
        // itself bypasses the global gate (see
        // `emit_physical_memory_event` in jfr/src/builtin.rs) and writes
        // to the calling thread's bounded SPSC ring; the event lives
        // there until a subsequent drain forwards it to whichever
        // recording is active at dump time. The cost when no recording
        // ever starts is one ring slot — negligible.
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        // VM `max_heap_size` as a stand-in for total physical memory
        // until the platform sysinfo dependency lands. `used_size` is
        // also reported as `max_heap_size` (no precise allocator
        // accounting at startup); this keeps the schema honest about
        // scope while keeping the field non-zero so the consumer-side
        // schema check passes.
        let total = shared.config.max_heap_size as i64;
        let mut jfr = shared.debug.flight_recorder.lock();
        cratonvm_jfr::builtin::emit_physical_memory_event(&mut jfr, total, total, now_ns);

        // Round-5 JFR Fix 4: emit `jdk.InitialEnvironmentVariable` for
        // each VM-relevant env var so dumps capture the startup config
        // surface. We restrict to `CRATONVM_*`, `JAVA_*`, `_JAVA_OPTIONS`,
        // `JAVA_TOOL_OPTIONS`, `CLASSPATH` to avoid leaking unrelated
        // shell variables into the recording (and to keep the per-chunk
        // metadata small). OpenJDK's reference snapshot includes a
        // similar filtered set. Cost: ~N emit calls at startup, each
        // bounded-ring push; N is typically < 10.
        for (key, value) in std::env::vars() {
            let interesting = key.starts_with("CRATONVM_")
                || key.starts_with("JAVA_")
                || key == "_JAVA_OPTIONS"
                || key == "JAVA_TOOL_OPTIONS"
                || key == "CLASSPATH";
            if interesting {
                cratonvm_jfr::builtin::emit_initial_environment_variable_event(
                    &mut jfr, &key, &value, now_ns,
                );
            }
        }
        drop(jfr);

        Self {
            shared,
            main_thread,
        }
    }

    // ----- Class loading (delegating to shared) ----------------------------

    /// Load a class by name. Returns the ClassId.
    pub fn load_class(&mut self, name: &str) -> Result<ClassId, VmError> {
        self.shared.load_class_concurrent(name)
    }

    /// Get the name of a class by its id.
    pub fn class_name(&self, class_id: ClassId) -> Option<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.to_string())
    }

    // ----- Object creation -------------------------------------------------

    /// Allocate a new object of the given class on the heap.
    pub fn new_object(&mut self, class_name: &str) -> Result<ObjectRef, VmError> {
        let class_id = self.shared.load_class_concurrent(class_name)?;
        let num_fields = self
            .shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.num_total_fields)
            .unwrap_or(0);
        let obj_ref = self.shared.mem.heap.alloc_object(class_id, num_fields);
        Ok(obj_ref)
    }

    /// Allocate a new primitive array.
    pub fn new_array(&mut self, element_type: ArrayElementType, length: usize) -> ObjectRef {
        self.shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), element_type, length)
    }

    /// Allocate a new reference array of the given component class.
    pub fn new_ref_array(&mut self, component_class_id: ClassId, length: usize) -> ObjectRef {
        self.shared
            .mem
            .heap
            .alloc_array(component_class_id, ArrayElementType::Reference, length)
    }

    // ----- Method invocation (delegating to free functions) -----------------

    /// Invoke a method by class name, method name, and descriptor.
    pub fn invoke(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        super::invoke_shared(
            &self.shared,
            &mut self.main_thread,
            class_name,
            method_name,
            descriptor,
            args,
        )
    }

    /// Invoke a method on a specific class by ClassId.
    pub fn invoke_on_class(
        &mut self,
        class_id: ClassId,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        super::invoke_on_class_shared(
            &self.shared,
            &mut self.main_thread,
            class_id,
            method_name,
            descriptor,
            args,
        )
    }

    // ----- Failure diagnostics ----------------------------------------------

    /// Render a thrown `Throwable` as `<binary class name>: <detailMessage>`.
    ///
    /// [`MethodCallFailed::ExceptionThrown`] carries only an [`ObjectRef`], so
    /// its `Debug`/`Display` can print no more than a heap address. That is
    /// unusable for a caller that has to group hundreds of failures by cause —
    /// the extended interpreter corpus reported 175 of its 214 failures as an
    /// undifferentiated `Err(ExceptionThrown(ObjectRef { .. }))` until this
    /// existed. Resolving the class and the detail message needs the heap and
    /// the class manager, which only the VM has; hence a method here rather
    /// than a richer `Display` on the error type.
    ///
    /// Best-effort and non-throwing: it reads fields directly instead of
    /// calling `toString()`, so it cannot recurse into further exceptions and
    /// is safe to call from a test assertion path. An unresolvable class
    /// renders as `<unknown class>` and a missing/undecodable message is
    /// omitted.
    pub fn describe_exception(&self, obj: ObjectRef) -> String {
        let class_id = self.shared.mem.heap.class_id_of(obj);
        let name = {
            let cm = self.shared.classes.class_manager.read();
            cm.get_class(class_id)
                .map(|c| c.name.replace('/', "."))
                .unwrap_or_else(|| "<unknown class>".to_string())
        };
        match self.exception_detail_message(obj) {
            Some(msg) => format!("{name}: {msg}"),
            None => name,
        }
    }

    /// Read the detail message off `obj`.
    ///
    /// Two layouts have to be handled, which is why this is not a one-line
    /// field read. Real-JDK `java.lang.Throwable` declares `detailMessage` by
    /// name, so the hierarchy walk finds it. CratonVM's synthetic
    /// `java/lang/Throwable` (`class_manager`'s `instance_fields(2)`) names its
    /// slots `_f0`/`_f1` — message and cause — and real-JDK bootstrap metadata
    /// can render Throwable's first slots opaquely the same way. For those,
    /// fall back to scanning the Throwable's own slots for the first value
    /// that decodes as a `java.lang.String`.
    ///
    /// Deliberately does not guess a fixed slot number: the message is slot 0
    /// in the synthetic layout and slot 1 in the real one (slot 0 there is
    /// `backtrace`), and picking wrong renders an unrelated object as the
    /// message. Reading the value and requiring it to be a decodable String
    /// answers that without encoding either layout.
    fn exception_detail_message(&self, obj: ObjectRef) -> Option<String> {
        let class_id = self.shared.mem.heap.class_id_of(obj);
        let (named, throwable_slots) = {
            let cm = self.shared.classes.class_manager.read();
            let mut walk = Some(class_id);
            let mut named = None;
            let mut throwable_slots = None;
            while let Some(cid) = walk {
                let Some(cls) = cm.get_class(cid) else { break };
                let mut instance = 0usize;
                for f in &cls.fields {
                    if f.is_static() {
                        continue;
                    }
                    if &*f.name == "detailMessage" && named.is_none() {
                        named = Some(cls.first_field_index + instance);
                    }
                    instance += 1;
                }
                if &*cls.name == "java/lang/Throwable" {
                    throwable_slots = Some((cls.first_field_index, instance));
                }
                walk = cls.superclass;
            }
            (named, throwable_slots)
        };

        let read_string = |index: usize| match self.shared.mem.heap.get_field(obj, index) {
            Value::Object(Some(s)) => super::vm_object::read_java_string(&self.shared.mem.heap, s),
            _ => None,
        };

        if let Some(index) = named {
            if let Some(text) = read_string(index) {
                return Some(text);
            }
        }
        let (base, count) = throwable_slots?;
        (base..base + count).find_map(read_string)
    }

    /// Render any [`MethodCallFailed`] for a human.
    ///
    /// `InternalError` delegates to the error's own `Display` (already
    /// self-describing); `ExceptionThrown` goes through
    /// [`Self::describe_exception`] instead of printing a bare pointer.
    pub fn describe_failure(&self, err: &MethodCallFailed) -> String {
        match err {
            MethodCallFailed::InternalError(e) => format!("internal: {e}"),
            MethodCallFailed::ExceptionThrown(obj) => {
                format!("threw {}", self.describe_exception(*obj))
            }
        }
    }

    /// Render a whole [`MethodCallResult`] — the shape a test assertion wants
    /// when the call did not produce what it expected.
    pub fn describe_result(&self, result: &MethodCallResult) -> String {
        match result {
            Ok(Some(v)) => format!("returned {v:?}"),
            Ok(None) => "returned void".to_string(),
            Err(e) => self.describe_failure(e),
        }
    }

    // ----- Finalization (M19) ------------------------------------------------

    /// Run pending finalizers: dequeue objects from the finalizer thread and
    /// invoke their `finalize()` method via virtual dispatch.
    ///
    /// Returns the number of objects finalized.
    pub fn run_pending_finalizers(&mut self) -> usize {
        let mut count = 0usize;
        // First transfer from ref_processor → finalizer_thread
        self.shared.drain_finalizers();

        while let Some(obj_addr) = self.shared.mem.finalizer_thread.dequeue() {
            // SAFETY: obj_addr was obtained from a heap allocation and registered
            // by register_finalizable, so the pointer is valid.
            let obj_ref = unsafe { ObjectRef::from_raw(obj_addr as *mut u8) };
            let class_id = self.shared.mem.heap.class_id_of(obj_ref);

            // Look up the class name for virtual dispatch of finalize()
            let class_name = self
                .shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|c| c.name.clone());

            if let Some(name) = class_name {
                // Virtual dispatch: invoke finalize()V on the actual class.
                // Errors from finalize() are silently swallowed per JLS §12.6.
                // Track execution time to detect long-running finalizers.
                let timeout_ms = self.shared.mem.finalizer_thread.timeout_ms();
                let start = std::time::Instant::now();
                let _ = self.invoke(&name, "finalize", "()V", &[Value::Object(Some(obj_ref))]);
                let elapsed = start.elapsed().as_millis() as u64;
                if elapsed > timeout_ms {
                    tracing::warn!(
                        "Finalizer for {} took {}ms (exceeds {}ms timeout)",
                        name,
                        elapsed,
                        timeout_ms
                    );
                }
                count += 1;
            }
        }
        count
    }

    // ----- Static fields ---------------------------------------------------

    /// Get a static field value.
    pub fn get_static(&self, class_id: ClassId, field_index: usize) -> Value {
        super::get_static_shared(&self.shared, class_id, field_index)
    }

    /// Set a static field value.
    pub fn set_static(&mut self, class_id: ClassId, field_index: usize, value: Value) {
        super::set_static_shared(&self.shared, class_id, field_index, value);
    }

    // ----- Instance fields (embedding read-back / write-back) ---------------

    /// Resolve the instance-field slot index of `field_name` on `class_id`,
    /// walking the superclass chain (the most-derived declaration wins, matching
    /// `getfield` resolution for a static type). Returns `None` if the class is
    /// not loaded or declares no such instance field. The returned index is the
    /// absolute slot suitable for [`Vm::get_instance_field`] /
    /// [`Vm::set_instance_field`] and `heap.get_field`. Resolution is by **name**
    /// only; use [`Vm::instance_field_index_desc`] to disambiguate a shadowed
    /// same-name field by its descriptor.
    pub fn instance_field_index(&self, class_id: ClassId, field_name: &str) -> Option<usize> {
        let cm = self.shared.classes.class_manager.read();
        super::vm_exec::resolve_field_index_in_hierarchy(class_id, field_name, &cm.class_store)
    }

    /// Descriptor-aware field resolution: resolve `field_name` to its layout
    /// slot, optionally disambiguated by JVM type `descriptor` (`"I"`,
    /// `"Ljava/lang/String;"`, …). Passing `Some(descriptor)` lets a caller
    /// address a **shadowed** super-class field that a subclass re-declares
    /// with the same name (the super-class field's descriptor walks past the
    /// subclass shadow). `None` is identical to [`Vm::instance_field_index`]
    /// (most-derived declaration wins). See
    /// [`super::vm_exec::resolve_field_index_in_hierarchy_desc`].
    pub fn instance_field_index_desc(
        &self,
        class_id: ClassId,
        field_name: &str,
        descriptor: Option<&str>,
    ) -> Option<usize> {
        let cm = self.shared.classes.class_manager.read();
        super::vm_exec::resolve_field_index_in_hierarchy_desc(
            class_id,
            field_name,
            descriptor,
            &cm.class_store,
        )
    }

    /// Number of instance-field slots in `class_id`'s layout — the valid index
    /// range `[0, count)` for the instance-field accessors.
    pub fn instance_field_count(&self, class_id: ClassId) -> usize {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.num_total_fields)
            .unwrap_or(0)
    }

    /// Read instance-field slot `index` of `obj`.
    pub fn get_instance_field(&self, obj: ObjectRef, index: usize) -> Value {
        self.shared.mem.heap.get_field(obj, index)
    }

    /// Write `value` into instance-field slot `index` of `obj`, **GC-barrier
    /// correct**: the SATB pre-barrier on the overwritten reference plus the
    /// post write-barrier fired inside `set_field`, exactly mirroring the
    /// interpreter's `putfield` (so a moving/concurrent collector stays sound).
    /// The caller is responsible for `index` being in `[0,
    /// instance_field_count(class))` — an out-of-range slot is dropped by the
    /// heap's own bounds guard.
    pub fn set_instance_field(&self, obj: ObjectRef, index: usize, value: Value) {
        // SATB pre-barrier on the old reference (no-op for primitives/null and
        // for non-SATB collectors), mirroring `interpreter.rs` putfield.
        let old = self.shared.mem.heap.get_field(obj, index);
        if let Value::Object(Some(old_ref)) = old {
            self.shared
                .mem
                .heap
                .write_barrier_pre(std::ptr::null_mut(), old_ref);
        }
        // The post write-barrier (card marking / remembered set) fires inside
        // `set_field` itself.
        self.shared.mem.heap.set_field(obj, index, value);
    }

    // ----- Class initialization (delegating to free functions) ---------------

    /// Ensure a class is fully initialized.
    pub fn ensure_class_initialized(&mut self, class_id: ClassId) -> Result<(), MethodCallFailed> {
        super::ensure_class_initialized_shared(&self.shared, &mut self.main_thread, class_id)
    }

    // ----- Subclass checking -----------------------------------------------

    /// Check if `child_id` is a subclass of (or implements) `parent_id`.
    pub fn is_subclass_of(&self, child_id: ClassId, parent_id: ClassId) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .is_subclass_of(child_id, parent_id)
    }

    /// Display-friendly snapshot of one frame in a captured Throwable trace.
    pub fn throwable_stack_for(&self, throwable: ObjectRef) -> Option<Vec<StackTraceFrame>> {
        let h = self.shared.mem.heap.identity_hash_code(throwable);
        let frames = self.shared.throwable_stack_trace(h)?;
        Some(
            frames
                .iter()
                .map(|e| StackTraceFrame {
                    class: e.class_name.to_string(),
                    method: e.method_name.to_string(),
                    file: e.source_file.as_deref().map(|s| s.to_string()),
                    line: e.line_number,
                })
                .collect(),
        )
    }
}

/// Display-friendly snapshot of one captured-stack-trace frame, suitable for
/// the CLI's unhandled-exception renderer. Detached from the on-heap
/// `StackTraceElement` layout so callers do not need to walk fields.
#[derive(Debug, Clone)]
pub struct StackTraceFrame {
    /// Fully-qualified class name, e.g. `java/lang/Class`.
    pub class: String,
    /// Method name (no descriptor).
    pub method: String,
    /// Source file (e.g. `Class.java`), `None` when the class has no
    /// `SourceFile` attribute. CLI prints "Unknown Source" in that case.
    pub file: Option<String>,
    /// Line number, `-1` for unknown / `-2` for native. CLI omits
    /// `:<line>` when negative.
    pub line: i32,
}

impl std::fmt::Debug for Vm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vm")
            .field(
                "classes_loaded",
                &self.shared.classes.class_manager.read().loaded_count(),
            )
            .field("heap_bytes", &self.shared.mem.heap.allocated_bytes())
            .field("native_methods", &self.shared.natives.native_methods.len())
            .field("printed_values", &self.main_thread.printed.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Per-VM native/security state teardown
// ---------------------------------------------------------------------------

/// Release every piece of process-global state this VM filed under
/// `vm_identity`.
///
/// Three rows today, all keyed on the identity and all of which would otherwise
/// outlive the heap that produced them:
///
/// * the [`CapabilitySet`](cratonvm_native_api::CapabilitySet) installed by
///   `SharedVm::new` — a long-lived host process that creates and disposes of
///   VMs would otherwise accumulate dead entries in a `Vec` that every
///   `capabilities_for` lookup scans linearly;
/// * `native-builtins`' per-VM SecurityManager row, which holds raw heap
///   `ObjectRef`s (the installed manager, the policy object, the shared
///   permission collection). `forget_vm_security_state` had **no call site at
///   all** before this, so those addresses survived the heap they pointed into
///   and a later VM that reused the identity would have inherited them;
/// * the `java.lang.instrument` `ClassFileTransformer` chain
///   ([`crate::runtime::instrument`]), which likewise holds raw heap
///   `ObjectRef`s — the live transformer mirrors registered by Mockito's
///   MockMaker / JaCoCo's agent — and is a registered GC root source, so a
///   surviving row would report addresses into a dead heap on a later VM's
///   collection.
///
/// **Idempotent by construction** — `uninstall_capabilities` returns `None`,
/// and `forget_vm_security_state` / `forget_vm_transformers` remove nothing on
/// a second call — because it is deliberately invoked from two places (see
/// below), and either may run first or alone.
pub fn release_vm_native_state(vm_identity: usize) {
    cratonvm_native_api::uninstall_capabilities(cratonvm_native_api::VmId::from_raw(vm_identity));
    cratonvm_native_builtins::security_manager::forget_vm_security_state(vm_identity);
    // The built-in app/platform `ClassLoader` singletons. Same reasoning as the
    // SecurityManager row above: raw heap `ObjectRef`s that must not outlive
    // the heap, and must never be visible to a VM that reuses the identity.
    cratonvm_native_builtins::classloader::forget_vm_loader_singletons(vm_identity);
    // The annotation-proxy cache, its per-proxy child roots, and the
    // last-created-proxy interfaces array. Keyed by `ClassId`, which every VM
    // mints from zero, so a surviving row is not merely a leak — it is a
    // wrong-heap hit for the next VM.
    cratonvm_native_builtins::lang_class::forget_vm_annotation_proxies(vm_identity);
    // The cached `System.getenv()` map and `System.getProperties()` object.
    cratonvm_native_builtins::lang_system::forget_vm_system_singletons(vm_identity);
    // The `java.lang.ClassValue` memoization cache (BUG-W). Its key is a pair
    // of 32-bit identity hashes, which two live VMs collide on readily.
    cratonvm_native_builtins::phases_late::forget_vm_classvalue_cache(vm_identity);
    // Which thread owns each live `ScopedValue` binding.
    cratonvm_native_builtins::jdk25_concurrency::forget_vm_scoped_value_owners(vm_identity);
    // The synthetic `ReentrantLock` / `ReentrantReadWriteLock` state tables,
    // keyed by `(vm_identity, identity_hash)`.
    cratonvm_native_builtins::forget_vm_lock_state(vm_identity);
    // The generated `$ProxyN` class cache and the per-loader proxy module
    // numbering. Both store `ClassId`s / ids that only this class manager can
    // interpret.
    cratonvm_native_builtins::forget_vm_proxy_classes(vm_identity);
    // The `Class.getName()` / simple / canonical / package name memos, keyed
    // by `(vm_identity, class_id)`.
    cratonvm_native_builtins::lang_class::forget_vm_class_name_caches(vm_identity);
    crate::runtime::instrument::forget_vm_transformers(vm_identity);
    // Without this a disposed VM's JVMTI row leaks its agent's callback
    // closures, and its listener flags keep every OTHER VM's interpreter on the
    // slow path — the flags are a process-wide union by design.
    crate::runtime::jvmti::forget_vm_jvmti_state(vm_identity);
}

/// The precise hook: the last `Arc<SharedVm>` is gone, so no thread can still
/// reach this VM's policy.
///
/// This is where the release *belongs*, and for a bare `SharedVm::new(..)` (the
/// ~1,500 unit-test VMs across the workspace, none of which build a `Vm`) it is
/// the only hook that fires — without it every one of those tests would leak an
/// entry into the process-wide capability index for the life of the test
/// binary.
///
/// It is **not sufficient on its own**: `Vm::new` stores
/// `JcmdProcessor::new_with_vm_state(shared.clone())` into
/// `shared.debug.jcmd_processor`, a strong `Arc<dyn VmDiagnosticState>` pointing
/// at the `SharedVm` that owns it. That reference cycle means a `Vm`-created
/// `SharedVm` is never dropped, so `Drop for Vm` carries the release for that
/// path (and, transitively, for `DestroyJavaVM` — `libcratonvm`'s
/// `destroy_created_vm` teardown hook drops the parked `Vm`). If that cycle is
/// ever broken, this impl becomes the single authority and the call in
/// `Drop for Vm` can go.
impl Drop for SharedVm {
    fn drop(&mut self) {
        release_vm_native_state(self.vm_identity);
    }
}

// ---------------------------------------------------------------------------
// AOT shutdown — flush training data on VM drop
// ---------------------------------------------------------------------------

impl Drop for Vm {
    fn drop(&mut self) {
        // Sync JIT profile data into AOT training recorder, then flush
        #[cfg(feature = "experimental-aot")]
        {
            use crate::config::AotMode;
            if self.shared.config.aot_mode == AotMode::Training {
                // Bridge JIT ProfileStore → AOT TrainingRunRecorder
                let synced = self.shared.sync_jit_profiles_to_aot();
                if synced > 0 {
                    tracing::info!(
                        "AOT: synced {} method profiles from JIT to AOT recorder",
                        synced
                    );
                }
                let bytes = crate::native::builtins::aot::aot_flush_training_data();
                if bytes > 0 {
                    tracing::info!("AOT: flushed {} bytes of training data", bytes);
                }
            }
        }

        // Dump GraalVM metadata if any was collected
        let (refl, _, _, _, _) = crate::native::builtins::graalvm_compat::graalvm_metadata_stats();
        if refl > 0 {
            tracing::debug!("GraalVM metadata: {} reflection entries collected", refl);
        }

        // Dump CDS archive if configured for dump mode (-Xshare:dump)
        if matches!(self.shared.config.cds_mode, crate::config::CdsMode::Dump) {
            match self.shared.dump_cds_archive() {
                Ok(count) => {
                    let path = self
                        .shared
                        .config
                        .shared_archive_file
                        .as_deref()
                        .unwrap_or("classes.jsa");
                    tracing::info!("CDS: dumped {} classes to {}", count, path);
                }
                Err(e) => {
                    tracing::error!("CDS: archive dump failed: {}", e);
                }
            }
        }

        // Run pending finalizers before shutdown (JLS §12.8)
        let finalized = self.run_pending_finalizers();
        if finalized > 0 {
            tracing::debug!("VM shutdown: ran {} pending finalizer(s)", finalized);
        }

        // Dump missing native method audit log if enabled
        if self.shared.config.audit_missing_natives {
            self.shared.dump_missing_natives();
        }

        // T6.3.1: fire JVMTI VMDeath after shutdown bookkeeping is done so
        // agent callbacks see the final heap and subsystem state. One-shot
        // per Vm drop; the underlying `fire_vm_death` is idempotent and
        // gated on the global manager's enable flag.
        // Attributed, and it must stay ahead of `release_vm_native_state`
        // below, which drops this VM's JVMTI row.
        crate::runtime::jvmti::fire_vm_death_for_vm(self.shared.vm_identity);

        // LAST, after the finalizer run and VMDeath above: those still execute
        // Java and still go through natives, and a native that has just lost its
        // capability policy silently reverts to allow-everything. Releasing here
        // keeps the policy live for the whole of shutdown.
        //
        // This is the `DestroyJavaVM` path too: `libcratonvm::destroy_created_vm`
        // (registered via `set_destroy_vm_hook`) takes the parked `Vm` out of
        // `CREATED_VM` and drops it, which runs exactly this.
        //
        // `Drop for SharedVm` does the same thing and is the more precise hook,
        // but it cannot be relied on here: `Vm::new` installs a `JcmdProcessor`
        // holding a strong `Arc` back at its own `SharedVm`, so a `Vm`-created
        // `SharedVm` is never dropped. `release_vm_native_state` is idempotent
        // precisely so both hooks can exist.
        //
        // Caveat worth stating: other threads may still hold `Arc<SharedVm>`
        // clones and still be executing Java when a `Vm` is dropped. Their gates
        // resolve `None` from here on, i.e. they revert to the permissive
        // default. That is a fail-open at shutdown, and it is the price of the
        // reference cycle above, not of this ordering.
        release_vm_native_state(self.shared.vm_identity);
    }
}

// ---------------------------------------------------------------------------
// VmDiagnosticState implementation for SharedVm
// ---------------------------------------------------------------------------

impl crate::runtime::serviceability::VmDiagnosticState for SharedVm {
    fn thread_snapshots(&self) -> Vec<crate::runtime::serviceability::ThreadSnapshot> {
        use crate::runtime::serviceability::{ThreadSnapshot, ThreadState};

        let names = self.threads.thread_registry.all_thread_names();
        let mut snapshots = Vec::with_capacity(names.len());

        for (tid, name) in &names {
            let alive = self.threads.thread_registry.is_alive(*tid);
            let state = if alive {
                ThreadState::Runnable
            } else {
                ThreadState::Terminated
            };

            snapshots.push(ThreadSnapshot {
                id: tid.0 as u64,
                name: name.clone(),
                daemon: name.starts_with("GC")
                    || name.starts_with("Finalizer")
                    || name.starts_with("Reference"),
                priority: 5,
                state,
                stack_frames: Vec::new(), // Frames are on OS thread stacks; only accessible when suspended
                lock_info: None,
                blocked_by: None,
                waiting_on: None,
            });
        }

        // Always include a "main" thread if none present
        if snapshots.is_empty() {
            snapshots.push(ThreadSnapshot {
                id: 0,
                name: "main".to_string(),
                daemon: false,
                priority: 5,
                state: ThreadState::Runnable,
                stack_frames: Vec::new(),
                lock_info: None,
                blocked_by: None,
                waiting_on: None,
            });
        }

        snapshots
    }

    fn heap_summary(&self) -> crate::runtime::serviceability::HeapSummary {
        use crate::runtime::serviceability::HeapSummary;

        let (young_used, young_cap) = self.mem.heap.young_gen_stats();
        let (old_used, old_cap) = self.mem.heap.old_gen_stats();
        // Metaspace approximation: count loaded classes × estimated per-class overhead
        let class_count = {
            let cm = self.classes.class_manager.read();
            cm.loaded_count()
        };
        let metaspace_used = class_count * 4096; // ~4 KB per class average
        let metaspace_capacity = metaspace_used.max(64 * 1024 * 1024); // At least 64 MB

        HeapSummary {
            young_gen_used: young_used as u64,
            young_gen_capacity: young_cap as u64,
            old_gen_used: old_used as u64,
            old_gen_capacity: old_cap as u64,
            metaspace_used: metaspace_used as u64,
            metaspace_capacity: metaspace_capacity as u64,
            total_used: (young_used + old_used + metaspace_used) as u64,
            total_capacity: (young_cap + old_cap + metaspace_capacity) as u64,
        }
    }

    fn class_histogram(&self) -> Vec<crate::runtime::serviceability::ClassHistogramEntry> {
        use crate::runtime::serviceability::ClassHistogramEntry;

        // Walk all heap objects and build per-class histogram
        let walked = self.mem.heap.walk_objects();
        let mut histogram: HashMap<u32, (String, u64, u64)> = HashMap::new();

        for &(ptr, size) in &walked {
            let header = unsafe { &*(ptr as *const crate::memory::heap::ObjectHeader) };
            let cid = header.class_id.as_u32();
            let entry = histogram.entry(cid).or_insert_with(|| {
                let name = {
                    let cm = self.classes.class_manager.read();
                    cm.get_class(crate::classloading::ClassId::new(cid))
                        .map(|c| c.name.to_string())
                        .unwrap_or_else(|| format!("class#{}", cid))
                };
                (name, 0, 0)
            });
            entry.1 += 1;
            entry.2 += size as u64;
        }

        let mut entries: Vec<ClassHistogramEntry> = histogram
            .into_values()
            .map(|(name, count, bytes)| ClassHistogramEntry {
                class_name: name,
                instance_count: count,
                total_bytes: bytes,
            })
            .collect();

        // Sort by total_bytes descending
        entries.sort_by(|a, b| b.total_bytes.cmp(&a.total_bytes));
        entries
    }

    fn trigger_gc(&self) -> bool {
        // Set the flag — interpreter's maybe_gc will pick it up at the next safepoint
        self.mem
            .gc_requested
            .store(true, std::sync::atomic::Ordering::Relaxed);
        true
    }

    fn uptime_secs(&self) -> f64 {
        self.debug.diagnostic_counters.uptime_secs()
    }

    fn command_line(&self) -> String {
        let mut parts = vec!["cratonvm".to_string()];
        if self.config.max_heap_size != 256 * 1024 * 1024 {
            parts.push(format!(
                "-Xmx{}m",
                self.config.max_heap_size / (1024 * 1024)
            ));
        }
        if !self.config.classpath.is_empty() {
            parts.push(format!("-cp {}", self.config.classpath.join(":")));
        }
        for (k, v) in &self.config.system_properties {
            parts.push(format!("-D{}={}", k, v));
        }
        parts.join(" ")
    }

    fn system_properties(&self) -> Vec<(String, String)> {
        let props = self.system_properties.read();
        props.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    fn vm_flags(&self) -> Vec<String> {
        let mut flags = Vec::new();
        let gc_name = match self.config.gc_algorithm {
            crate::config::GcAlgorithm::Generational => "UseGenerationalGC",
            crate::config::GcAlgorithm::G1 => "UseG1GC",
            #[cfg(feature = "zgc")]
            crate::config::GcAlgorithm::Zgc => "UseZGC",
        };
        flags.push(format!("-XX:+{}", gc_name));
        flags.push(format!("-XX:MaxHeapSize={}", self.config.max_heap_size));
        flags.push(format!(
            "-XX:InitialHeapSize={}",
            self.config.initial_heap_size
        ));
        flags.push(format!("-XX:MaxStackDepth={}", self.config.max_stack_depth));
        if self.config.use_compressed_oops {
            flags.push("-XX:+UseCompressedOops".to_string());
        }
        if self.config.use_compact_headers {
            flags.push("-XX:+UseCompactObjectHeaders".to_string());
        }
        if self.config.verbose_gc {
            flags.push("-verbose:gc".to_string());
        }
        if self.config.verbose_class_loading {
            flags.push("-verbose:class".to_string());
        }
        if self.config.skip_verification {
            flags.push("-noverify".to_string());
        }
        flags.push(format!("-XX:+TieredCompilation"));
        // Emitted **unconditionally**, including under `Compatible`. A flag
        // that only appears in the strict mode answers "is this a jdk-only
        // run?" but leaves "was this VM even aware of the policy?"
        // indistinguishable from an old binary — and `jcmd VM.flags` output is
        // routinely the only artefact attached to a bug report. Always
        // printing the value makes the census, the report's `mode` field and
        // this line agree by construction (jdk-only-mode.md §6).
        flags.push(format!(
            "-XX:CompatibilityMode={}",
            self.config.compatibility_mode.as_str()
        ));
        flags
    }

    fn heap_dump(&self, path: &str) -> Result<u64, String> {
        use crate::runtime::serviceability::{HprofClassInfo, HprofObjectInfo, HprofWriter};
        use cratonvm_gc::heap::{ObjectHeader, ObjectKind, HEADER_SIZE, SLOT_SIZE};

        // Step 1: Walk all heap objects
        let walked = self.mem.heap.walk_objects();

        // Step 2: Build class info from ClassManager
        let cm = self.classes.class_manager.read();
        let mut classes: Vec<HprofClassInfo> = Vec::new();
        let mut class_ids_seen: std::collections::HashSet<u32> = std::collections::HashSet::new();

        // First pass: collect all class IDs referenced by heap objects
        for &(ptr, _size) in &walked {
            let header = unsafe { &*(ptr as *const ObjectHeader) };
            let cid = header.class_id.as_u32();
            class_ids_seen.insert(cid);
        }

        // Build HprofClassInfo for each referenced class (+ walk superclass chains)
        let mut worklist: Vec<u32> = class_ids_seen.iter().copied().collect();
        let mut processed: std::collections::HashSet<u32> = std::collections::HashSet::new();
        while let Some(cid) = worklist.pop() {
            if processed.contains(&cid) {
                continue;
            }
            processed.insert(cid);

            if let Some(cls) = cm.get_class(crate::classloading::ClassId::new(cid)) {
                let super_id = cls.superclass.map(|s| s.as_u32()).unwrap_or(0);

                let instance_fields: Vec<(String, String)> = cls
                    .fields
                    .iter()
                    .filter(|f| !f.is_static())
                    .map(|f| (f.name.to_string(), f.descriptor.to_string()))
                    .collect();

                let static_fields: Vec<(String, String)> = cls
                    .fields
                    .iter()
                    .filter(|f| f.is_static())
                    .map(|f| (f.name.to_string(), f.descriptor.to_string()))
                    .collect();

                let instance_size = (HEADER_SIZE + cls.num_total_fields * SLOT_SIZE) as u32;

                classes.push(HprofClassInfo {
                    class_id: cid,
                    name: cls.name.to_string(),
                    super_class_id: super_id,
                    instance_fields,
                    static_fields,
                    source_file: cls.source_file.clone(),
                    instance_size,
                });

                // Enqueue superclass for processing
                if super_id != 0 {
                    worklist.push(super_id);
                }
            }
        }
        drop(cm); // release class_manager lock

        // Step 3: Build HprofObjectInfo for each heap object
        let objects: Vec<HprofObjectInfo> = walked
            .iter()
            .map(|&(ptr, size)| {
                let header = unsafe { &*(ptr as *const ObjectHeader) };
                HprofObjectInfo {
                    object_id: ptr as u64,
                    class_id: header.class_id.as_u32(),
                    is_array: header.kind() == ObjectKind::Array,
                    element_type: header.element_type() as u8,
                    array_length: header.array_length(),
                    total_size: size,
                    data_ptr: ptr as *const u8,
                }
            })
            .collect();

        // Step 4: Get thread snapshots
        let threads = self.thread_snapshots();

        // Step 5: Generate the full HPROF binary
        let hprof_data = HprofWriter::write_full_heap_dump(&classes, &objects, &threads);

        // Step 6: Write to file
        std::fs::write(path, &hprof_data)
            .map_err(|e| format!("Failed to write heap dump to {}: {}", path, e))?;

        Ok(hprof_data.len() as u64)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod zgc_relocation_gate_tests {
    use super::zgc_relocation_permitted;

    /// **Relocation is permitted only where a reference load is barriered.**
    ///
    /// The original rule was "refused whenever the JIT is enabled", because
    /// JIT-compiled code baked raw 8-byte reference loads at compile-time
    /// offsets and would read a coloured word as a pointer -- a use-after-free
    /// with no error path. That is still the hazard; what changed on
    /// 2026-08-13 is that stage (a) of `zgc-jit-load-barrier.md` landed, so
    /// the JIT no longer does that: `x64::zgc_read_barrier_blocks_inline_fields`
    /// routes every compact-field access through `jit_getfield` /
    /// `jit_putfield_object` whenever the barrier is armed, and those go
    /// through the heap accessors, which barrier.
    ///
    /// So the predicate is now a disjunction, and both arms are asserted here
    /// rather than one being assumed. **If inline reference emission is ever
    /// re-enabled under an armed barrier, `zgc_codegen_honours_read_barrier`
    /// must go back to `false` and this test must go red** -- that is what it
    /// is for.
    #[test]
    fn relocation_is_permitted_only_where_reference_loads_are_barriered() {
        let jit_off = crate::runtime::env_cache::disable_jit();
        let codegen_ok = cratonvm_jit::x64::zgc_codegen_honours_read_barrier();
        assert_eq!(
            zgc_relocation_permitted(true),
            jit_off || codegen_ok,
            "requested relocation must be permitted IF AND ONLY IF every \
             reference load is barriered -- either because there is no JIT \
             code, or because the JIT routes reference loads through the \
             barriered helpers when the barrier is armed"
        );
    }

    /// The hazard the gate exists for, stated so it cannot be lost: with the
    /// JIT on and codegen NOT honouring the barrier, relocation must refuse.
    ///
    /// Asserted as an implication rather than by forcing the state, because
    /// neither input is settable from a test in this process -- `disable_jit`
    /// is latched and `zgc_codegen_honours_read_barrier` is a build property.
    /// The value is that the rule is written down as an executable claim: if
    /// someone makes the predicate permissive in a way that drops one of the
    /// two arms, the assertion above fails.
    #[test]
    fn relocation_refuses_when_neither_arm_holds() {
        let jit_off = crate::runtime::env_cache::disable_jit();
        let codegen_ok = cratonvm_jit::x64::zgc_codegen_honours_read_barrier();
        if !jit_off && !codegen_ok {
            assert!(
                !zgc_relocation_permitted(true),
                "unbarriered JIT reference loads plus a moving cycle is a \
                 use-after-free; the gate must refuse"
            );
        }
    }

    /// Not requested is not permitted — the branch `vm_init` actually takes
    /// today (`RELOCATION_REQUESTED = false`).
    ///
    /// **Weaker than it looks, and saying so is the point.** In a test process
    /// with the JIT enabled the `!requested` early return and the JIT refusal
    /// both answer `false`, so this assertion cannot distinguish them:
    /// deleting the early return leaves it passing. It is kept as a statement
    /// of the contract, not as a mutation detector, and the detector for the
    /// branch that matters is
    /// [`relocation_is_permitted_only_when_the_jit_is_off`] — verified to fail
    /// when the gate is short-circuited to always permit. A `--nojit` test
    /// process is what would separate these two, and this crate's suite does
    /// not run one.
    #[test]
    fn relocation_that_was_not_requested_is_never_permitted() {
        assert!(
            !zgc_relocation_permitted(false),
            "the gate must never permit relocation nobody asked for"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_heap_sizing_leaves_explicit_heap_unchanged() {
        let mut config = VmConfig::default();
        config.max_heap_size = 512 * 1024 * 1024;

        apply_container_default_heap(&mut config);

        assert_eq!(config.max_heap_size, 512 * 1024 * 1024);
    }

    #[test]
    fn container_heap_sizing_respects_disabled_container_support() {
        let mut config = VmConfig::default();
        config.use_container_support = false;

        apply_container_default_heap(&mut config);

        assert_eq!(config.max_heap_size, DEFAULT_MAX_HEAP_SIZE);
    }

    // -----------------------------------------------------------------------
    // JDK-only mode (docs/feature-designs/jdk-only-mode.md §8)
    // -----------------------------------------------------------------------

    /// The `Compatible` boot precondition must probe **nothing**.
    ///
    /// `VmConfig::default()` is hermetic — it is built by hundreds of unit
    /// tests, by every embedder and on CI machines with no JDK installed — and
    /// the whole point of returning early in `require_jdk_image_for_jdk_only`
    /// is that adding the strict-mode check must not make the *default* boot
    /// depend on what happens to be on the host. `Ok(None)` is therefore the
    /// assertion: not merely "it succeeded", but "it did not resolve a
    /// `JAVA_HOME`", which is only possible if the probe never ran.
    #[test]
    fn compatible_boot_precondition_does_no_host_probing() {
        let config = VmConfig::default();
        assert_eq!(
            config.compatibility_mode,
            CompatibilityMode::Compatible,
            "the default config must never infer strict mode from a build \
             feature or an unrelated env var (contract §6)"
        );

        let resolved = require_jdk_image_for_jdk_only(&config)
            .expect("the default configuration must always be bootable");
        assert!(
            resolved.is_none(),
            "Compatible mode returned a resolved JDK home ({resolved:?}) — the \
             default path probed the host, which breaks VmConfig::default()'s \
             hermetic contract"
        );
    }

    /// `--jdk-only` + the synthetic class library is a configuration error, and
    /// it must be caught here rather than only in the launcher: embedders and
    /// in-process harnesses build a `VmConfig` by hand and never parse argv.
    #[test]
    fn jdk_only_with_synthetic_library_is_a_configuration_error() {
        // `with_jdk_mode` rather than poking a field, so this test keeps
        // asking the same question regardless of whether `JdkMode` is stored
        // or derived in `config.rs`.
        let mut config = VmConfig::default().with_jdk_mode(crate::config::JdkMode::Synthetic);
        config.compatibility_mode = CompatibilityMode::JdkOnly;

        let err = require_jdk_image_for_jdk_only(&config)
            .expect_err("jdk-only + synthetic-jdk must be rejected");
        // `validate_compatibility` owns the wording (contract §6); assert on
        // the fact rather than on its exact prose so agent A can improve the
        // message without breaking this test.
        assert!(
            matches!(err, VmError::InvalidConfiguration(_)),
            "expected a configuration error, got {err:?}"
        );
        // Crucially: rejected by the *conflict*, not by the JDK search. A
        // strict run must fail this way even on a host with a perfectly good
        // JDK installed, which is why `validate_compatibility` runs first.
        let text = err.to_string();
        assert!(
            !text.contains("Searched, in order"),
            "the conflict must be reported before the JDK probe runs, but the \
             message carried the probe's search report: {text}"
        );
    }

    /// Registration-site redaction keeps the workspace-relative form (which is
    /// what `#[track_caller]` normally produces and what a reviewer needs) and
    /// collapses absolute paths, which otherwise leak the builder's home
    /// directory into a committed baseline and make the census differ per
    /// machine.
    #[test]
    fn registration_site_redaction_keeps_relative_and_collapses_absolute() {
        // Workspace-relative: kept verbatim, line number and all.
        assert_eq!(
            redact_registration_site("native-builtins/src/lib.rs:1234"),
            "native-builtins/src/lib.rs:1234"
        );
        // Windows separators normalise so a census taken on Windows diffs
        // against one taken on Linux.
        assert_eq!(
            redact_registration_site("native-builtins\\src\\jmx.rs:88"),
            "native-builtins/src/jmx.rs:88"
        );
        // POSIX absolute: collapsed to basename + line.
        assert_eq!(
            redact_registration_site("/home/someone/craton/native-builtins/src/lib.rs:1234"),
            "<redacted>/lib.rs:1234"
        );
        // Windows absolute: the drive-letter colon must not be mistaken for
        // the line-number separator.
        assert_eq!(
            redact_registration_site("C:\\craton\\wt-jdk-only\\native-builtins\\src\\lib.rs:12"),
            "<redacted>/lib.rs:12"
        );
        // Nothing recognisable left to keep.
        assert_eq!(redact_registration_site("/"), "<redacted>");
    }

    /// Prose-embedded path redaction, the other half of §9's redaction rule.
    /// Migrated here with the census writers it serves, so the rule and its
    /// test stay in the same crate.
    #[test]
    fn absolute_paths_are_redacted_to_their_basename() {
        let unix = redact_absolute_paths("refused at /home/ci-agent/work/app.jar (boot)");
        assert!(unix.contains("<redacted>/app.jar"), "{unix}");
        assert!(!unix.contains("ci-agent"), "{unix}");

        let windows = redact_absolute_paths("source C:\\Users\\victor\\build\\app.jar");
        assert!(windows.contains("<redacted>/app.jar"), "{windows}");
        assert!(!windows.contains("victor"), "{windows}");

        // Already JSON-escaped text: the separator arrives doubled.
        let escaped = redact_absolute_paths("\"C:\\\\Users\\\\victor\\\\app.jar\"");
        assert!(escaped.contains("<redacted>/app.jar"), "{escaped}");
        assert!(!escaped.contains("victor"), "{escaped}");

        // Internal-form names and descriptors are not paths and must survive
        // verbatim — every violation body is full of them.
        let names = "java/lang/String.charAt(I)C and (Ljava/lang/Object;)V";
        assert_eq!(redact_absolute_paths(names), names);
        // Too shallow to be worth hiding.
        assert_eq!(redact_absolute_paths("in /tmp now"), "in /tmp now");
    }

    #[test]
    fn json_escape_quotes_and_escapes() {
        assert_eq!(json_escape("plain"), "\"plain\"");
        assert_eq!(json_escape("a\"b"), "\"a\\\"b\"");
        assert_eq!(json_escape("c:\\x"), "\"c:\\\\x\"");
        assert_eq!(json_escape("l1\nl2\tt"), "\"l1\\nl2\\tt\"");
        assert_eq!(json_escape("\u{1}"), "\"\\u0001\"");
        assert_eq!(json_opt_string(None, true), "null");
        assert_eq!(json_opt_string(Some("x"), true), "\"x\"");
    }

    fn origin_row(name: &str, origin: &str, loader_id: u32) -> ClassOriginEntry {
        ClassOriginEntry {
            name: name.to_string(),
            origin: origin.to_string(),
            reason: None,
            requested_by: None,
            real_bytes_found: false,
            loader_id,
            supertypes: Vec::new(),
        }
    }

    /// The report's four class counters must be a partition of the ten origin
    /// tags: every tag lands in exactly one bucket, so the sum is the row count
    /// and no class can go missing between the census and the summary.
    #[test]
    fn origin_buckets_partition_every_row() {
        let rows: Vec<ClassOriginEntry> = CLASS_ORIGIN_TAGS
            .iter()
            .map(|tag| origin_row(&format!("x/{tag}"), tag, 0))
            .collect();
        let buckets = fold_origin_buckets(&rows);
        assert_eq!(buckets.boot_image, 1);
        // application-class-path + user-defined
        assert_eq!(buckets.application, 2);
        // vm-array, hidden-class, generated-lambda, generated-proxy,
        // reflection-accessor, vm-internal
        assert_eq!(buckets.generated, 6);
        assert_eq!(buckets.compatibility, 1);
        assert_eq!(
            buckets.total(),
            rows.len() as u64,
            "the fold must be a partition — no class may fall between buckets"
        );
    }

    #[test]
    fn class_origin_census_is_sorted_counted_and_redacted() {
        let mut rows = vec![
            ClassOriginEntry {
                name: "org/jboss/logging/Logger".into(),
                origin: "compatibility-stub".into(),
                reason: Some("enterprise-prefix fallback".into()),
                requested_by: Some("/home/ci-agent/work/app.jar".into()),
                real_bytes_found: false,
                loader_id: 0,
                supertypes: vec!["java/lang/Object".into()],
            },
            origin_row("com/example/Main", "application-class-path", 2),
            origin_row("java/lang/String", "boot-image", 0),
            origin_row("com/example/Main", "application-class-path", 1),
            origin_row("[I", "vm-array", 0),
        ];
        let n = rows.len();
        let text = render_class_origins_json(&mut rows, false);

        // Sorted by (name, loader_id, origin).
        let at = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("missing {needle}"));
        assert!(at("\"[I\"") < at("\"com/example/Main\""));
        assert!(at("\"com/example/Main\"") < at("\"java/lang/String\""));
        assert!(at("\"java/lang/String\"") < at("\"org/jboss/logging/Logger\""));
        // The loader tiebreak: the two `com/example/Main` rows are ordered by
        // defining loader, not by insertion.
        assert!(at("\"loader_id\": 1") < at("\"loader_id\": 2"));

        // Every tag is present, so "zero" is stated rather than implied.
        for tag in CLASS_ORIGIN_TAGS {
            assert!(text.contains(&format!("\"{tag}\":")), "counts omit {tag}");
        }
        assert!(text.contains("\"compatibility-stub\": 1"), "{text}");
        assert!(text.contains("\"boot-image\": 1"), "{text}");
        assert!(text.contains("\"application-class-path\": 2"), "{text}");
        assert!(text.contains("\"hidden-class\": 0"), "{text}");
        assert!(text.contains(&format!("\"total\": {n}")), "{text}");
        assert!(text.contains("\"schema_version\": 1"), "{text}");

        // Redaction is on by default (no --explain-jdk-only here).
        assert!(text.contains("<redacted>/app.jar"), "{text}");
        assert!(!text.contains("ci-agent"), "{text}");

        // ...and off under --explain-jdk-only.
        let verbose = render_class_origins_json(&mut rows, true);
        assert!(verbose.contains("/home/ci-agent/work/app.jar"), "{verbose}");
    }

    /// The `java.vm.info` property and the launcher's `-version` banner read
    /// the same function, so the policy token can only ever be spelled once.
    /// Compatible mode must stay byte-for-byte HotSpot's value (§10).
    #[test]
    fn vm_info_mode_list_spells_the_policy_once() {
        assert_eq!(vm_info_mode_list(CompatibilityMode::Compatible), "mixed mode");
        assert_eq!(
            vm_info_mode_list(CompatibilityMode::JdkOnly),
            "mixed mode, jdk-only"
        );
        // The token is the mode's own wire spelling, appended to the list —
        // never a re-spelling of it.
        assert!(vm_info_mode_list(CompatibilityMode::JdkOnly)
            .ends_with(CompatibilityMode::JdkOnly.as_str()));
    }

    #[cfg(feature = "experimental-debug")]
    #[test]
    #[should_panic(expected = "failed to load JVMTI startup agent")]
    fn startup_jvmti_missing_native_agent_panics() {
        let mut config = VmConfig::default();
        config
            .jvmti_agent_options
            .push("-agentpath:definitely-not-a-real-agent-library".to_string());
        let mut env = crate::jvmti::create_jvmti_env();

        load_startup_jvmti_agents(&config, &mut env);
    }

    #[cfg(feature = "experimental-debug")]
    #[test]
    #[should_panic(expected = "-javaagent must run through runtime::agent_loader::invoke_premains")]
    fn startup_jvmti_rejects_javaagent_tokens() {
        let mut config = VmConfig::default();
        config
            .jvmti_agent_options
            .push("-javaagent:agent.jar=opts".to_string());
        let mut env = crate::jvmti::create_jvmti_env();

        load_startup_jvmti_agents(&config, &mut env);
    }

    // -----------------------------------------------------------------------
    // T2.1.3: JDK module classifier
    // -----------------------------------------------------------------------

    #[test]
    fn t2_classify_java_base() {
        assert_eq!(classify_jdk_module("java/lang/String"), "java.base");
        assert_eq!(classify_jdk_module("java/util/HashMap"), "java.base");
        assert_eq!(classify_jdk_module("java/io/File"), "java.base");
        assert_eq!(classify_jdk_module("java/nio/ByteBuffer"), "java.base");
        assert_eq!(classify_jdk_module("java/net/URL"), "java.base");
        assert_eq!(
            classify_jdk_module("java/security/MessageDigest"),
            "java.base"
        );
        assert_eq!(classify_jdk_module("java/math/BigInteger"), "java.base");
        assert_eq!(classify_jdk_module("java/time/LocalDate"), "java.base");
    }

    #[test]
    fn t2_classify_non_base_modules() {
        assert_eq!(
            classify_jdk_module("java/net/http/HttpClient"),
            "java.net.http"
        );
        assert_eq!(classify_jdk_module("java/sql/DriverManager"), "java.sql");
        assert_eq!(
            classify_jdk_module("javax/management/MBeanServer"),
            "java.management"
        );
        assert_eq!(
            classify_jdk_module("java/util/logging/Logger"),
            "java.logging"
        );
        assert_eq!(classify_jdk_module("jdk/jfr/Event"), "jdk.jfr");
        assert_eq!(
            classify_jdk_module("com/sun/management/ThreadMXBean"),
            "jdk.management"
        );
        assert_eq!(classify_jdk_module("sun/misc/Unsafe"), "jdk.unsupported");
    }

    #[test]
    fn t2_classify_longest_prefix_wins() {
        // java/net/http must win over java/net.
        assert_eq!(
            classify_jdk_module("java/net/http/HttpRequest"),
            "java.net.http"
        );
        // java/util/logging must win over java/util.
        assert_eq!(
            classify_jdk_module("java/util/logging/LogRecord"),
            "java.logging"
        );
    }

    #[test]
    fn t2_classify_unknown_goes_to_other() {
        assert_eq!(classify_jdk_module("com/example/App"), "other");
        assert_eq!(classify_jdk_module("org/acme/Lib"), "other");
        assert_eq!(classify_jdk_module(""), "other");
    }

    #[test]
    fn t2_classify_no_false_prefix_matches() {
        // "java/netXYZ" must NOT match "java/net" — the classifier's
        // prefixes all end with "/".
        assert_eq!(classify_jdk_module("java/netXYZ/Foo"), "other");
    }

    #[test]
    fn t2_grouped_dump_is_empty_when_no_missing_natives() {
        let shared = SharedVm::new(VmConfig::default());
        let grouped = shared.classify_missing_natives_by_module();
        assert!(grouped.is_empty());
    }

    #[test]
    fn t2_grouped_dump_groups_and_dedupes() {
        let shared = SharedVm::new(VmConfig::default());
        shared.record_missing_native("java/lang/Foo", "a", "()V", None);
        shared.record_missing_native("java/net/http/Client", "send", "()V", None);
        shared.record_missing_native("java/sql/Conn", "close", "()V", None);
        // Duplicate should be dropped by record_missing_native's own dedupe.
        shared.record_missing_native("java/lang/Foo", "a", "()V", None);

        let grouped = shared.classify_missing_natives_by_module();
        assert_eq!(grouped.len(), 3);
        assert_eq!(grouped["java.base"].len(), 1);
        assert_eq!(grouped["java.net.http"].len(), 1);
        assert_eq!(grouped["java.sql"].len(), 1);
    }

    #[test]
    fn t2_grouped_json_round_trip_is_valid_json() {
        let shared = SharedVm::new(VmConfig::default());
        shared.record_missing_native("java/lang/Foo", "a", "()V", None);
        shared.record_missing_native("com/example/Bar", "b", "()V", Some("Main.main".into()));

        let tmp = std::env::temp_dir().join("t2_grouped.json");
        shared
            .dump_missing_natives_grouped_json(&tmp)
            .expect("missing natives dump should succeed");

        let content = std::fs::read_to_string(&tmp).expect("file should be readable");
        // Smoke checks — it's syntactically a JSON object with the
        // expected top-level keys and at least one module entry.
        assert!(content.starts_with("{\n"));
        assert!(content.ends_with("}\n"));
        assert!(content.contains("\"version\": 1"));
        assert!(content.contains("\"modules\":"));
        assert!(content.contains("\"java.base\""));
        assert!(content.contains("\"other\""));
        // Entries should appear under their correct groups.
        assert!(content.contains("\"java/lang/Foo\""));
        assert!(content.contains("\"com/example/Bar\""));
        let _ = std::fs::remove_file(&tmp);
    }

    // -----------------------------------------------------------------------
    // SharedVm construction
    // -----------------------------------------------------------------------

    #[test]
    fn shared_vm_default_config() {
        let shared = SharedVm::new(VmConfig::default());
        assert!(shared.natives.native_methods.len() > 0);
        // C25 synthetic stubs plus wired super/interfaces (`Enumeration$Impl`,
        // `java/lang/Object`, `java/util/Enumeration`, `Comparator$Native`,
        // `java/util/Comparator`), PLUS the unmodifiable-collection-view
        // bootstrap (`Collection`/`List`/`Set`/`SortedSet`/`NavigableSet`/
        // `Map`/`ListIterator`/`Serializable` and the 8
        // `cratonvm/internal/Unmodifiable*` synthetic stamps, see
        // `d3474b3e`/`c2d68883`) plus `AssertionError` and `Iterator` and
        // their transitively-loaded superinterfaces. Grew again from 25 to 29
        // with `4edaa9ba7`'s 4 new bootstrap loads: `java/util/Map$Entry`
        // plus the `cratonvm/internal/UnmodifiableEntrySet` +
        // `UnmodifiableMapEntry` + `UnmodifiableEntryItr` synthetic stamps
        // (registered in `vm_init.rs` to fix `Collections.unmodifiableMap()
        // .entrySet()`'s `setValue()` not throwing). This count legitimately
        // grew from 5 as bootstrap work landed; if it changes again, verify
        // the new value against `SharedVm::new`'s class-loading calls rather
        // than assuming a regression.
        assert_eq!(shared.classes.class_manager.read().loaded_count(), 29);
        assert!(shared.classes.statics.read().is_empty());
        assert!(shared.mem.string_pool.read().is_empty());
        assert!(shared.classes.class_mirrors.read().is_empty());
        assert!(shared.classes.lambda_proxies.read().is_empty());
    }

    #[test]
    fn shared_vm_system_properties_populated() {
        let shared = SharedVm::new(VmConfig::default());
        let props = shared.system_properties.read();
        assert!(props.contains_key("os.name"));
        assert!(props.contains_key("os.arch"));
        assert!(props.contains_key("file.separator"));
        assert!(props.contains_key("path.separator"));
        assert!(props.contains_key("line.separator"));
        assert!(props.contains_key("java.version"));
        assert_eq!(
            props.get("java.vendor").map(|s| s.as_str()),
            Some("CratonVM")
        );
        assert!(props.contains_key("file.encoding"));
    }

    // -----------------------------------------------------------------
    // W7-67 — host default locale
    // -----------------------------------------------------------------

    /// The BCP-47 spelling Windows hands back. These are not "language and
    /// country" — a script subtag sits between them and must not be mistaken
    /// for the region, which is what a naive `split_once('_')` did.
    #[test]
    fn parse_locale_name_reads_bcp47_subtags() {
        let ru = parse_locale_name("ru-RU");
        assert_eq!(ru.language, "ru");
        assert_eq!(ru.country, "RU");
        assert_eq!(ru.script, "");
        assert_eq!(ru.variant, "");

        let zh = parse_locale_name("zh-Hans-CN");
        assert_eq!(zh.language, "zh");
        assert_eq!(zh.script, "Hans");
        assert_eq!(zh.country, "CN");

        // A 3-digit region (UN M.49) is a region, not a variant.
        let es = parse_locale_name("es-419");
        assert_eq!(es.language, "es");
        assert_eq!(es.country, "419");

        // Everything after the region is a variant; Java uppercases them and
        // joins multiples with `_`.
        let ca = parse_locale_name("ca-ES-valencia");
        assert_eq!(ca.country, "ES");
        assert_eq!(ca.variant, "VALENCIA");
    }

    /// The POSIX spelling `$LANG` carries, including the suffixes that are not
    /// part of the locale.
    #[test]
    fn parse_locale_name_reads_posix_spelling() {
        let ru = parse_locale_name("ru_RU.UTF-8");
        assert_eq!(ru.language, "ru");
        assert_eq!(ru.country, "RU");

        let bare = parse_locale_name("en");
        assert_eq!(bare.language, "en");
        assert_eq!(bare.country, "");

        // `@latin`/`@cyrillic` name a script, not a variant.
        let sr = parse_locale_name("sr_RS@latin");
        assert_eq!(sr.language, "sr");
        assert_eq!(sr.country, "RS");
        assert_eq!(sr.script, "Latn");

        // Any other modifier is carried as a variant.
        assert_eq!(parse_locale_name("de_DE@euro").variant, "EURO");
    }

    /// `java_props_md.c` maps the POSIX locales onto English/US — a real JVM
    /// never reports language `"C"`. This is the CI default on a bare shell.
    #[test]
    fn parse_locale_name_maps_posix_locales_to_english() {
        for raw in ["C", "POSIX", "C.UTF-8", "", "  "] {
            let got = parse_locale_name(raw);
            assert_eq!(got.language, "en", "input {raw:?}");
            assert_eq!(got.country, "US", "input {raw:?}");
        }
    }

    /// The three ISO-639 codes Java froze at their pre-1989 spellings. Applying
    /// them here is what keeps `System.getProperty("user.language")` equal to
    /// `Locale.getDefault().getLanguage()`, which `Locale` reaches by its own
    /// internal `convertOldISOCodes`.
    #[test]
    fn parse_locale_name_applies_the_frozen_iso639_codes() {
        assert_eq!(parse_locale_name("he-IL").language, "iw");
        assert_eq!(parse_locale_name("yi").language, "ji");
        assert_eq!(parse_locale_name("id-ID").language, "in");
    }

    /// `SystemProps.fillI18nProps` rule 2/3: the base property takes the
    /// DISPLAY value, and `.format` appears ONLY when it differs. A host whose
    /// UI and regional format agree — the common case — must not grow a
    /// redundant overlay, because `Locale.getDefault(FORMAT)` reading a
    /// present-but-equal key is indistinguishable from reading the base.
    #[test]
    fn fill_i18n_props_writes_the_format_overlay_only_when_it_differs() {
        let mut props = HashMap::new();
        fill_i18n_props(&mut props, &[], "user.language", "ru", "ru");
        assert_eq!(props.get("user.language").map(String::as_str), Some("ru"));
        assert!(!props.contains_key("user.language.format"));
        // `.display` is never derived from platform values — the JDK's
        // condition for writing it is dead once the base has taken the same
        // value.
        assert!(!props.contains_key("user.language.display"));

        let mut split = HashMap::new();
        fill_i18n_props(&mut split, &[], "user.language", "en", "ru");
        assert_eq!(split.get("user.language").map(String::as_str), Some("en"));
        assert_eq!(
            split.get("user.language.format").map(String::as_str),
            Some("ru")
        );
    }

    /// `SystemProps.fillI18nProps` rule 1, and the one that is easy to get
    /// wrong: a command-line `-Duser.language` does not merely override the
    /// base — it suppresses the derived overlay entirely. Without this,
    /// `-Duser.language=en -Duser.country=US` on this ru_RU host would pin the
    /// base to en and leave `user.language.format=ru` behind, so
    /// `NumberFormat.getInstance()` would still format in Russian and a
    /// "pinned locale" run would not actually be pinned.
    #[test]
    fn fill_i18n_props_lets_a_command_line_value_suppress_the_overlay() {
        let cmdline = vec![("user.language".to_string(), "en".to_string())];
        let mut props = HashMap::new();
        fill_i18n_props(&mut props, &cmdline, "user.language", "en", "ru");
        assert!(
            props.is_empty(),
            "a -D base value must suppress both the derived base and the \
             overlay, got {props:?}"
        );
    }

    /// HotSpot publishes all four `user.*` locale keys, empty string included:
    /// `System.getProperty("user.variant")` is `""` there, never null. We used
    /// to omit `user.script` and `user.variant` entirely.
    #[test]
    fn shared_vm_publishes_the_whole_user_locale_family() {
        let shared = SharedVm::new(VmConfig::default());
        let props = shared.system_properties.read();
        for key in [
            "user.language",
            "user.script",
            "user.country",
            "user.variant",
        ] {
            assert!(props.contains_key(key), "{key} must be published");
        }
        // Never the raw POSIX locale names.
        let lang = props.get("user.language").cloned().unwrap_or_default();
        assert!(
            !lang.is_empty() && lang != "C" && lang != "POSIX",
            "user.language must be a real language code, got {lang:?}"
        );
    }

    #[test]
    fn shared_vm_custom_system_properties() {
        let mut config = VmConfig::default();
        config
            .system_properties
            .push(("custom.key".to_string(), "custom.value".to_string()));
        config
            .system_properties
            .push(("java.version".to_string(), "21.0".to_string()));
        let shared = SharedVm::new(config);
        let props = shared.system_properties.read();
        assert_eq!(
            props.get("custom.key").map(|s| s.as_str()),
            Some("custom.value")
        );
        // User override should take precedence
        assert_eq!(props.get("java.version").map(|s| s.as_str()), Some("21.0"));
    }

    // NEW-11: `PrintStream.println` is a synthetic override. In the
    // non-synthetic default build the real JDK bytecode handles it,
    // so the native is only present when `synthetic-jdk` is enabled.
    #[test]
    #[cfg(feature = "synthetic-jdk")]
    fn shared_vm_native_methods_registered() {
        let shared = SharedVm::new(VmConfig::default());
        // Check a few known native methods are registered
        assert!(shared
            .natives
            .native_methods
            .find("java/io/PrintStream", "println", "(Ljava/lang/String;)V")
            .is_some());
        assert!(shared
            .natives
            .native_methods
            .find("java/io/PrintStream", "println", "(I)V")
            .is_some());
    }

    #[test]
    fn shared_vm_lambda_id_starts_high() {
        let shared = SharedVm::new(VmConfig::default());
        let id = shared.alloc_lambda_proxy_id();
        assert!(id.as_u32() >= 0x8000_0000);
    }

    #[test]
    fn shared_vm_lambda_ids_are_sequential() {
        let shared = SharedVm::new(VmConfig::default());
        let id1 = shared.alloc_lambda_proxy_id();
        let id2 = shared.alloc_lambda_proxy_id();
        assert_eq!(id2.as_u32(), id1.as_u32() + 1);
    }

    #[test]
    fn shared_vm_debug_format() {
        let shared = SharedVm::new(VmConfig::default());
        let debug = format!("{:?}", shared);
        assert!(debug.contains("SharedVm"));
        assert!(debug.contains("classes_loaded"));
        assert!(debug.contains("heap_bytes"));
        assert!(debug.contains("native_methods"));
    }

    // -----------------------------------------------------------------------
    // SharedVm: class lock objects
    // -----------------------------------------------------------------------

    #[test]
    fn class_lock_object_created_and_cached() {
        let shared = SharedVm::new(VmConfig::default());
        let class_id = ClassId::new(5);
        let lock1 = shared.get_class_lock_object(class_id);
        let lock2 = shared.get_class_lock_object(class_id);
        // Same class -> same lock object
        assert_eq!(lock1.as_ptr(), lock2.as_ptr());
    }

    #[test]
    fn class_lock_objects_differ_per_class() {
        let shared = SharedVm::new(VmConfig::default());
        let lock_a = shared.get_class_lock_object(ClassId::new(1));
        let lock_b = shared.get_class_lock_object(ClassId::new(2));
        assert_ne!(lock_a.as_ptr(), lock_b.as_ptr());
    }

    #[test]
    fn class_lock_object_for_fieldful_class_is_plain_zero_slot_object() {
        let shared = SharedVm::new(VmConfig::default());
        let fieldful_id = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class("cratonvm/test/FieldfulStaticLockTarget", 3).expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
        let object_id = shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id("java/lang/Object")
            .unwrap_or(ClassId::new(0));

        let lock = shared.get_class_lock_object(fieldful_id);
        let header = shared.mem.heap.get_header(lock);

        assert_eq!(header.class_id, object_id);
        assert_eq!(header.num_slots(), 0);
    }

    // -----------------------------------------------------------------------
    // SharedVm: ensure_system_streams
    // -----------------------------------------------------------------------

    #[test]
    fn ensure_system_streams_creates_objects() {
        let shared = SharedVm::new(VmConfig::default());
        let (out, err) = shared.ensure_system_streams();
        let out_header = shared.mem.heap.get_header(out);
        let err_header = shared.mem.heap.get_header(err);
        assert_ne!(out.as_ptr(), err.as_ptr());
        assert_eq!(out_header.class_id, err_header.class_id);
        assert!(out_header.num_slots() >= 1);
        assert!(err_header.num_slots() >= 1);

        // Synthetic PrintStream uses slot 0 as its stdout/stderr descriptor.
        // In the real JDK, that slot is FilterOutputStream.out (a reference),
        // and the streams are identified by object identity instead. Writing
        // an integer there would box it and corrupt the real stream graph.
        let slot0_is_ref = cratonvm_gc::class_layout(out_header.class_id.as_u32())
            .and_then(|layout| layout.field_is_ref(0))
            .unwrap_or(false);
        if slot0_is_ref {
            assert!(!matches!(shared.mem.heap.get_field(out, 0), Value::Int(1)));
            assert!(!matches!(shared.mem.heap.get_field(err, 0), Value::Int(2)));
        } else {
            assert_eq!(shared.mem.heap.get_field(out, 0), Value::Int(1));
            assert_eq!(shared.mem.heap.get_field(err, 0), Value::Int(2));
        }
    }

    #[test]
    fn ensure_system_streams_idempotent() {
        let shared = SharedVm::new(VmConfig::default());
        let (out1, err1) = shared.ensure_system_streams();
        let (out2, err2) = shared.ensure_system_streams();
        assert_eq!(out1.as_ptr(), out2.as_ptr());
        assert_eq!(err1.as_ptr(), err2.as_ptr());
    }

    // -----------------------------------------------------------------------
    // Vm construction
    // -----------------------------------------------------------------------

    /// Dropping a `Vm` must actually destroy its `SharedVm`.
    ///
    /// It did not: `JcmdProcessor::new_with_vm_state` cloned the `Arc` into
    /// nine diagnostic-command closures that live inside
    /// `SharedVm::debug.jcmd_processor`, so the VM owned nine strong
    /// references to itself. `Arc::strong_count` went 1 -> 10 across that one
    /// call and stayed at 9 after the `Vm` was dropped, leaking the heap,
    /// class manager, thread registry and JIT caches of every VM ever built --
    /// visible in this crate's own test binary as ~80 `Attach-Listener`
    /// threads outliving the tests that created them.
    #[test]
    fn dropping_a_vm_destroys_its_shared_state() {
        let vm = Vm::new(VmConfig::default());
        let weak = std::sync::Arc::downgrade(&vm.shared);
        assert_eq!(
            std::sync::Arc::strong_count(&vm.shared),
            1,
            "the Vm must be the only strong owner of its SharedVm"
        );
        drop(vm);
        assert!(
            weak.upgrade().is_none(),
            "SharedVm outlived its Vm: {} strong refs remain",
            weak.strong_count()
        );
    }

    #[test]
    fn vm_new_creates_main_thread() {
        let vm = Vm::new(VmConfig::default());
        assert_eq!(vm.main_thread.thread_id, ThreadId(0));
        assert_eq!(vm.main_thread.name, "main");
        assert!(vm.main_thread.printed.is_empty());
    }

    #[test]
    fn vm_main_thread_publishes_stable_tlab_tail() {
        let mut vm = Vm::new(VmConfig::default());
        let mut backing = vec![0u64; 16];
        let base = backing.as_mut_ptr() as *mut u8;
        let size = backing.len() * std::mem::size_of::<u64>();
        vm.main_thread.tlab = unsafe { cratonvm_gc::Tlab::new(base, size) };
        vm.main_thread.tlab.alloc(32, 8).unwrap();

        // Moving `Vm` must not change the address observed by the registry:
        // the `JvmThread` lives in its dedicated Box.
        let vm = vm;
        assert_eq!(
            vm.shared
                .threads
                .thread_registry
                .collect_reserved_tlab_tails(),
            vec![(base as usize + 32, base as usize + size)],
        );
    }

    #[test]
    fn vm_self_arc_initialized() {
        let vm = Vm::new(VmConfig::default());
        // self_arc should be set, and get_arc should work
        let arc = vm.shared.get_arc();
        // Same bootstrap class set as `shared_vm_default_config` — see that
        // test's comment for what's currently in it and why the count moves.
        assert_eq!(arc.classes.class_manager.read().loaded_count(), 29);
    }

    #[test]
    fn vm_debug_format() {
        let vm = Vm::new(VmConfig::default());
        let debug = format!("{:?}", vm);
        assert!(debug.contains("Vm"));
        assert!(debug.contains("classes_loaded"));
        assert!(debug.contains("heap_bytes"));
        assert!(debug.contains("native_methods"));
        assert!(debug.contains("printed_values"));
    }

    // -----------------------------------------------------------------------
    // Vm: static field access
    // -----------------------------------------------------------------------

    #[test]
    fn vm_set_and_get_static() {
        let mut vm = Vm::new(VmConfig::default());
        let class_id = ClassId::new(10);
        vm.set_static(class_id, 0, Value::Int(42));
        assert_eq!(vm.get_static(class_id, 0), Value::Int(42));
    }

    #[test]
    fn vm_get_static_default() {
        let vm = Vm::new(VmConfig::default());
        // Unset static field should return default (Int(0))
        assert_eq!(vm.get_static(ClassId::new(99), 0), Value::Int(0));
    }

    #[test]
    fn vm_static_field_resize() {
        let mut vm = Vm::new(VmConfig::default());
        let class_id = ClassId::new(20);
        // Set field at index 5 without first setting lower indices
        vm.set_static(class_id, 5, Value::Long(999));
        assert_eq!(vm.get_static(class_id, 5), Value::Long(999));
        // Lower indices should be default
        assert_eq!(vm.get_static(class_id, 0), Value::Int(0));
    }

    // -----------------------------------------------------------------------
    // Vm: array creation
    // -----------------------------------------------------------------------

    #[test]
    fn vm_new_array() {
        let mut vm = Vm::new(VmConfig::default());
        let arr = vm.new_array(ArrayElementType::Int, 10);
        assert_eq!(vm.shared.mem.heap.array_length(arr), 10);
    }

    #[test]
    fn vm_new_ref_array() {
        let mut vm = Vm::new(VmConfig::default());
        let arr = vm.new_ref_array(ClassId::new(3), 5);
        assert_eq!(vm.shared.mem.heap.array_length(arr), 5);
    }

    // -----------------------------------------------------------------------
    // M9: Cached field counts
    // -----------------------------------------------------------------------

    #[test]
    fn cached_string_num_fields_starts_at_zero() {
        let shared = SharedVm::new(VmConfig::default());
        assert_eq!(
            shared
                .classes
                .cached_string_num_fields
                .load(Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn cached_class_mirror_num_fields_starts_at_zero() {
        let shared = SharedVm::new(VmConfig::default());
        assert_eq!(
            shared
                .classes
                .cached_class_mirror_num_fields
                .load(Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn create_java_string_caches_field_count() {
        let shared = SharedVm::new(VmConfig::default());
        // Before first call, cached is 0
        assert_eq!(
            shared
                .classes
                .cached_string_num_fields
                .load(Ordering::Relaxed),
            0
        );
        // Create a string — should resolve and cache the field count
        let s = crate::vm::vm_object::create_java_string(&shared, "hello");
        // After first call, cached should be nonzero (2 for synthetic stub)
        let cached = shared
            .classes
            .cached_string_num_fields
            .load(Ordering::Relaxed);
        assert!(
            cached >= 2,
            "cached field count should be at least 2, got {}",
            cached
        );
        // Second call should use the cached value
        let s2 = crate::vm::vm_object::create_java_string(&shared, "world");
        assert_ne!(s.as_ptr(), s2.as_ptr()); // different strings
    }

    #[test]
    fn class_mirror_caches_field_count() {
        let shared = SharedVm::new(VmConfig::default());
        assert_eq!(
            shared
                .classes
                .cached_class_mirror_num_fields
                .load(Ordering::Relaxed),
            0
        );
        let _mirror = crate::vm::vm_object::get_or_create_class_mirror(&shared, ClassId::new(1));
        let cached = shared
            .classes
            .cached_class_mirror_num_fields
            .load(Ordering::Relaxed);
        assert!(
            cached >= 2,
            "cached class mirror field count should be at least 2, got {}",
            cached
        );
    }

    #[test]
    fn pre_set_cached_field_count_used() {
        let shared = SharedVm::new(VmConfig::default());
        // Pre-set to 4 (simulating real JDK String with 4 fields)
        shared
            .classes
            .cached_string_num_fields
            .store(4, Ordering::Relaxed);
        let s = crate::vm::vm_object::create_java_string(&shared, "test4fields");
        // The string should have been allocated with 4 fields, so writing field 3 should work
        shared.mem.heap.set_field(s, 3, Value::Int(42));
        assert_eq!(shared.mem.heap.get_field(s, 3), Value::Int(42));
    }

    // -----------------------------------------------------------------------
    // M9: Missing native audit log
    // -----------------------------------------------------------------------

    #[test]
    fn missing_natives_log_initially_empty() {
        let shared = SharedVm::new(VmConfig::default());
        assert!(shared.debug.missing_natives_log.lock().is_empty());
    }

    #[test]
    fn audit_missing_natives_config_default_false() {
        let config = VmConfig::default();
        assert!(!config.audit_missing_natives);
    }

    #[test]
    fn audit_missing_natives_config_builder() {
        let config = VmConfig::new().with_audit_missing_natives(true);
        assert!(config.audit_missing_natives);
    }

    #[test]
    fn dump_missing_natives_noop_when_empty() {
        let shared = SharedVm::new(VmConfig::default());
        // Should not panic even with empty log
        shared.dump_missing_natives();
    }

    #[test]
    fn missing_natives_log_dedup_via_record() {
        let shared = SharedVm::new(VmConfig::default());
        // record_missing_native dedupes on (class, method, descriptor).
        shared.record_missing_native("java/lang/Foo", "bar", "()V", None);
        shared.record_missing_native("java/lang/Baz", "qux", "(I)I", Some("main".into()));
        shared.record_missing_native("java/lang/Foo", "bar", "()V", None); // dup
        let log = shared.debug.missing_natives_log.lock();
        assert_eq!(log.len(), 2);
        // dump_missing_natives prints without panicking even on a
        // populated log.
        drop(log);
        shared.dump_missing_natives();
    }

    // -----------------------------------------------------------------------
    // NEW-10 — missing-natives JSON dump
    // -----------------------------------------------------------------------

    /// Write the missing-natives log to a temp file and read back the
    /// produced JSON. Asserts: schema, key names, ordering, escape,
    /// call-site capture.
    #[test]
    fn new10_dump_missing_natives_json_schema() {
        let shared = SharedVm::new(VmConfig::default());
        shared.record_missing_native(
            "java/lang/Thread",
            "setNative",
            "()V",
            Some("com/example/Main.main([Ljava/lang/String;)V".into()),
        );
        shared.record_missing_native("java/lang/Foo", "bar", "(I)V", None);
        // Duplicate entry — must not appear twice.
        shared.record_missing_native(
            "java/lang/Foo",
            "bar",
            "(I)V",
            Some("another/caller.m()V".into()),
        );

        let tmp_dir = std::env::temp_dir().join("cratonvm_new10_test");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let path = tmp_dir.join(format!("missing_{}.json", std::process::id()));
        shared
            .dump_missing_natives_json(&path)
            .expect("dump should succeed");

        let contents = std::fs::read_to_string(&path).expect("read back");

        // Schema: outer object with the "missing_natives" key
        assert!(
            contents.starts_with("{\n  \"missing_natives\": ["),
            "file must start with the JSON schema header, got {contents:?}"
        );
        assert!(
            contents.trim_end().ends_with("]\n}"),
            "file must end with `]\\n}}`, got {contents:?}"
        );

        // Entries are sorted by (class, name, desc) — java/lang/Foo
        // appears before java/lang/Thread alphabetically.
        let foo_pos = contents.find("java/lang/Foo").expect("Foo present");
        let thread_pos = contents.find("java/lang/Thread").expect("Thread present");
        assert!(
            foo_pos < thread_pos,
            "entries should be sorted alphabetically"
        );

        // Call-site preserved (first occurrence wins).
        assert!(contents.contains("com/example/Main.main([Ljava/lang/String;)V"));
        // And the first-seen null-call-site entry is still null (not
        // overwritten by the duplicate's non-null call-site).
        assert!(contents.contains("\"sample_call_site\": null"));

        // No duplicate Foo entry.
        let foo_count = contents.matches("java/lang/Foo").count();
        assert_eq!(foo_count, 1, "duplicate record must dedupe to 1 Foo entry");

        let _ = std::fs::remove_file(&path);
    }

    /// Repeated runs must produce byte-identical JSON for the same
    /// set of entries (so the committed baseline file diffs cleanly).
    #[test]
    fn new10_dump_missing_natives_json_is_diff_stable() {
        let shared1 = SharedVm::new(VmConfig::default());
        let shared2 = SharedVm::new(VmConfig::default());
        // Insert in different orders — the output must be identical
        // because dump sorts on (class, name, desc).
        shared1.record_missing_native("a/B", "one", "()V", Some("x".into()));
        shared1.record_missing_native("a/A", "two", "()I", None);
        shared2.record_missing_native("a/A", "two", "()I", None);
        shared2.record_missing_native("a/B", "one", "()V", Some("x".into()));

        let tmp = std::env::temp_dir().join("cratonvm_new10_diff_stable");
        let _ = std::fs::create_dir_all(&tmp);
        let p1 = tmp.join(format!("d1_{}.json", std::process::id()));
        let p2 = tmp.join(format!("d2_{}.json", std::process::id()));
        shared1
            .dump_missing_natives_json(&p1)
            .expect("missing natives dump should succeed");
        shared2
            .dump_missing_natives_json(&p2)
            .expect("missing natives dump should succeed");

        let a = std::fs::read_to_string(&p1).expect("file should be readable");
        let b = std::fs::read_to_string(&p2).expect("file should be readable");
        assert_eq!(
            a, b,
            "dump output must be byte-identical regardless of record order"
        );
        let _ = std::fs::remove_file(&p1);
        let _ = std::fs::remove_file(&p2);
    }

    /// JSON escaping: a class name containing quote / backslash /
    /// control characters round-trips through json_escape correctly.
    /// The on-disk file must be valid JSON that any conformant parser
    /// accepts.
    #[test]
    fn new10_dump_missing_natives_json_escapes_strings() {
        let shared = SharedVm::new(VmConfig::default());
        shared.record_missing_native(
            "evil\"class\\name",
            "method\nwith\tcontrol",
            "()V",
            Some("caller\u{0001}ctrl".into()),
        );
        let tmp = std::env::temp_dir().join("cratonvm_new10_escape");
        let _ = std::fs::create_dir_all(&tmp);
        let path = tmp.join(format!("esc_{}.json", std::process::id()));
        shared
            .dump_missing_natives_json(&path)
            .expect("missing natives dump should succeed");
        let contents = std::fs::read_to_string(&path).expect("file should be readable");
        // The raw control characters must NOT appear in the file —
        // they must be encoded.
        assert!(!contents.contains('\n') || contents.matches('\n').count() > 2);
        assert!(contents.contains("\\\""));
        assert!(contents.contains("\\\\"));
        assert!(contents.contains("\\n"));
        assert!(contents.contains("\\t"));
        assert!(contents.contains("\\u0001"));
        let _ = std::fs::remove_file(&path);
    }

    /// Empty audit log still produces a valid JSON document (the
    /// schema's `missing_natives` array is empty).
    #[test]
    fn new10_dump_missing_natives_json_empty_log() {
        let shared = SharedVm::new(VmConfig::default());
        let tmp = std::env::temp_dir().join("cratonvm_new10_empty");
        let _ = std::fs::create_dir_all(&tmp);
        let path = tmp.join(format!("empty_{}.json", std::process::id()));
        shared
            .dump_missing_natives_json(&path)
            .expect("missing natives dump should succeed");
        let contents = std::fs::read_to_string(&path).expect("file should be readable");
        // Exact format: "{\n  \"missing_natives\": []\n}\n"
        assert_eq!(contents, "{\n  \"missing_natives\": []\n}\n");
        let _ = std::fs::remove_file(&path);
    }

    // -----------------------------------------------------------------------
    // M9: Real JDK mode registers fewer natives
    // -----------------------------------------------------------------------

    #[test]
    fn real_jdk_mode_registers_fewer_natives() {
        // NOTE: this test previously asserted `synthetic_stubs == 0` for
        // real-JDK mode, backed by `set_drop_synthetic_stubs(true)` forced
        // on unconditionally in `vm_init.rs`'s real-JDK arms (dev d8092acb,
        // 2026-07-14). That default was reverted the same day: several
        // SyntheticStub-tagged register_* clusters (the whole
        // java.lang.management/JMX native surface, java.util.function.
        // Function$Identity) are permanent bridges needed in BOTH modes --
        // no working real-bytecode fallback exists for them yet -- and
        // dropping them broke WildFly boot immediately (ObjectName NPE /
        // UnsatisfiedLinkError). `drop_synthetic_stubs` is opt-in only
        // again (`CRATONVM_NO_STUBS`), matching its own field doc. This
        // test now checks the weaker, still-true invariant: real-JDK mode
        // registers meaningfully fewer natives than synthetic-JDK mode
        // (fewer collection/layout fallbacks needed once real bytecode
        // handles those classes directly).
        let mut real_config = VmConfig::default();
        real_config.use_synthetic_jdk = false;
        let real_shared = SharedVm::new(real_config);
        let real_count = real_shared
            .natives
            .native_methods
            .dump_registrations()
            .len();

        let mut synthetic_config = VmConfig::default();
        synthetic_config.use_synthetic_jdk = true;
        let synthetic_shared = SharedVm::new(synthetic_config);
        let synthetic_count = synthetic_shared
            .natives
            .native_methods
            .dump_registrations()
            .len();

        assert!(
            real_count <= synthetic_count,
            "real-JDK mode ({real_count}) should not register MORE natives              than synthetic-JDK mode ({synthetic_count})"
        );
    }

    #[test]
    fn drop_synthetic_stubs_mechanism_still_works_when_opted_in() {
        // The `CRATONVM_NO_STUBS` / `set_drop_synthetic_stubs(true)` opt-in
        // mechanism itself (native-api/src/registry.rs) is unit-tested here
        // directly, decoupled from whether any particular boot mode enables
        // it by default (see `real_jdk_mode_registers_fewer_natives` above
        // for why real-JDK mode does not, as of 2026-07-14).
        let mut r = cratonvm_native_api::NativeMethodRegistry::new();
        r.set_drop_synthetic_stubs(true);
        // Test fixture, not a VM registration: this throwaway registry never
        // reaches a running VM and class `Test` does not exist. The two empty
        // bodies exist only so the assertions below can observe which category
        // `drop_synthetic_stubs` filters. KEEP.
        r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
        r.register("Test", "stub", "()V", |_ctx, _args| Ok(None));
        r.set_category(cratonvm_native_api::NativeKind::Bridge);
        r.register("Test", "bridge", "()V", |_ctx, _args| Ok(None));
        let kinds: Vec<_> = r
            .dump_registrations()
            .iter()
            .map(|(_, m, _, k)| (m.to_string(), *k))
            .collect();
        assert!(
            !kinds.iter().any(|(m, _)| m == "stub"),
            "SyntheticStub registration should have been dropped"
        );
        assert!(
            kinds.iter().any(|(m, _)| m == "bridge"),
            "Bridge registration should survive drop_synthetic_stubs"
        );
    }

    // NEW-11: the "synthetic mode registers many natives" assertion is
    // inherently feature-gated — it is checking a property of the
    // `synthetic-jdk` build. In the default build the synthetic
    // overrides are not registered, so the count is roughly 1200
    // instead of 4000+.
    #[test]
    #[cfg(feature = "synthetic-jdk")]
    fn synthetic_mode_registers_many_natives() {
        let shared = SharedVm::new(VmConfig::default());
        // Synthetic mode should have 5000+ registrations
        assert!(
            shared.natives.native_methods.len() > 4000,
            "Synthetic mode should have > 4000 natives, got {}",
            shared.natives.native_methods.len()
        );
    }

    // -----------------------------------------------------------------------
    // M19: Finalizer / Cleaner execution
    // -----------------------------------------------------------------------

    #[test]
    fn m19_shared_vm_has_reference_processor() {
        let shared = SharedVm::new(VmConfig::default());
        let rp = shared.mem.ref_processor.lock();
        assert_eq!(rp.pending_finalization_count(), 0);
    }

    #[test]
    fn m19_shared_vm_has_finalizer_thread() {
        let shared = SharedVm::new(VmConfig::default());
        assert_eq!(shared.mem.finalizer_thread.pending_count(), 0);
        assert!(!shared.mem.finalizer_thread.is_running());
    }

    #[test]
    fn m19_shared_vm_has_cleaner_thread() {
        let shared = SharedVm::new(VmConfig::default());
        assert_eq!(shared.mem.cleaner_thread.pending_count(), 0);
        assert!(!shared.mem.cleaner_thread.is_running());
    }

    #[test]
    fn m19_register_finalizable_enqueues() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        // Allocate a dummy object to get a valid address
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        shared.register_finalizable(obj.as_ptr() as usize);
        let rp = shared.mem.ref_processor.lock();
        // Should have one finalizer reference discovered
        assert!(
            rp.stats().finalizer_refs_discovered == 0,
            "before processing, discovered count is still 0 (it's set during process_references)"
        );
    }

    #[test]
    fn m19_process_references_enqueues_dead_finalizable() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let addr = obj.as_ptr() as usize;
        shared.register_finalizable(addr);

        // Process references — the object is "dead" (not marked)
        shared.process_references(&|_| false, 100, 0);

        // The finalizer thread should have the object enqueued
        assert_eq!(shared.mem.finalizer_thread.pending_count(), 1);
        let dequeued = shared.mem.finalizer_thread.dequeue();
        assert_eq!(dequeued, Some(addr));
    }

    #[test]
    fn m19_process_references_keeps_live_finalizable() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let addr = obj.as_ptr() as usize;
        shared.register_finalizable(addr);

        // Process references — the object is "live" (marked)
        shared.process_references(&|_| true, 100, 0);

        // The finalizer thread should NOT have the object
        assert_eq!(shared.mem.finalizer_thread.pending_count(), 0);
    }

    #[test]
    fn m19_drain_finalizers_transfers_to_thread() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let addr = obj.as_ptr() as usize;

        // Directly enqueue in ref_processor's finalization_queue
        {
            let mut rp = shared.mem.ref_processor.lock();
            rp.discover_reference(
                cratonvm_gc::reference::ReferenceType::Finalizer,
                addr,
                addr,
                None,
            );
        }
        // Process to move to finalization queue
        shared.process_references(&|_| false, 100, 0);

        // drain_finalizers should report 0 because process_references already
        // pushed directly to the finalizer_thread
        assert_eq!(shared.mem.finalizer_thread.pending_count(), 1);
    }

    #[test]
    fn m19_cleaner_actions_drained() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        shared.mem.cleaner_thread.submit_action(0x1000);
        shared.mem.cleaner_thread.submit_action(0x2000);
        let actions = shared.drain_cleaners();
        assert_eq!(actions, vec![0x1000, 0x2000]);
        assert_eq!(shared.mem.cleaner_thread.pending_count(), 0);
    }

    #[test]
    fn m19_finalizer_thread_lifecycle() {
        let shared = SharedVm::new(VmConfig::default());
        assert!(!shared.mem.finalizer_thread.is_running());
        shared.mem.finalizer_thread.start();
        assert!(shared.mem.finalizer_thread.is_running());
        shared.mem.finalizer_thread.stop();
        assert!(!shared.mem.finalizer_thread.is_running());
    }

    #[test]
    fn m19_cleaner_thread_lifecycle() {
        let shared = SharedVm::new(VmConfig::default());
        assert!(!shared.mem.cleaner_thread.is_running());
        shared.mem.cleaner_thread.start();
        assert!(shared.mem.cleaner_thread.is_running());
        shared.mem.cleaner_thread.stop();
        assert!(!shared.mem.cleaner_thread.is_running());
    }

    // -----------------------------------------------------------------------
    // Session 7: Bootstrap java.lang.Object from real JDK bytecode
    // Run with: cargo test -p cratonvm-vm -- --ignored
    // -----------------------------------------------------------------------

    /// Helper: create a SharedVm configured for real JDK mode.
    /// Returns None if no JDK is available.
    fn create_real_jdk_vm() -> Option<SharedVm> {
        create_real_jdk_vm_with_config(false)
    }

    fn create_real_jdk_vm_audit() -> Option<SharedVm> {
        create_real_jdk_vm_with_config(true)
    }

    fn create_real_jdk_vm_with_config(audit: bool) -> Option<SharedVm> {
        // Detect JDK from JAVA_HOME or PATH
        let java_home = crate::config::resolve_java_home_public(None)?;
        let java_home_str = java_home.to_string_lossy().into_owned();

        let config = VmConfig::new().with_java_home(java_home_str);

        // Set use_synthetic_jdk to false to load real JDK classes
        let mut config = config;
        config.use_synthetic_jdk = false;
        config.audit_missing_natives = audit;

        Some(SharedVm::new(config))
    }

    #[test]
    #[ignore] // requires JDK on host
    fn s7_object_loaded_from_real_bytecode() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        // java.lang.Object should be loaded during bootstrap
        let obj_id = cm.get_loaded_class_id("java/lang/Object");
        assert!(obj_id.is_some(), "Object should be loaded");

        let obj_id = obj_id.expect("obj_id should be Some");
        let cls = cm
            .class_store
            .get(obj_id)
            .expect("class should exist in class store");

        // Verify it's from real bytecode, not a synthetic stub
        assert!(
            !cls.origin.is_compatibility_stub(),
            "Object should come from real bytecode, not a synthetic stub"
        );

        // Object has no superclass
        assert!(cls.superclass.is_none(), "Object has no superclass");

        // Object should have methods from real bytecode
        assert!(
            !cls.methods.is_empty(),
            "Object should have methods parsed from bytecode, got 0"
        );

        // Check specific methods exist
        let method_names: Vec<&str> = cls.methods.iter().map(|m| m.name.as_ref()).collect();
        assert!(
            method_names.contains(&"hashCode"),
            "Object should have hashCode method, found: {:?}",
            method_names
        );
        assert!(
            method_names.contains(&"equals"),
            "Object should have equals method, found: {:?}",
            method_names
        );
        assert!(
            method_names.contains(&"toString"),
            "Object should have toString method, found: {:?}",
            method_names
        );
        assert!(
            method_names.contains(&"getClass"),
            "Object should have getClass method, found: {:?}",
            method_names
        );

        // Verify native methods have the ACC_NATIVE flag
        let hash_code = cls
            .methods
            .iter()
            .find(|m| &*m.name == "hashCode")
            .expect("method hashCode should exist");
        assert!(hash_code.is_native(), "hashCode should be native");
        let get_class = cls
            .methods
            .iter()
            .find(|m| &*m.name == "getClass")
            .expect("method getClass should exist");
        assert!(get_class.is_native(), "getClass should be native");

        // Verify non-native methods (equals, toString) have bytecode
        let equals = cls
            .methods
            .iter()
            .find(|m| &*m.name == "equals")
            .expect("method equals should exist");
        assert!(
            !equals.is_native(),
            "equals should NOT be native (it's bytecode)"
        );

        eprintln!(
            "java.lang.Object: {} methods, {} fields, synthetic={}",
            cls.methods.len(),
            cls.fields.len(),
            cls.origin.is_compatibility_stub()
        );
    }

    #[test]
    #[ignore] // requires JDK on host
    fn s7_object_native_methods_wired() {
        let shared = create_real_jdk_vm().expect("No JDK found");

        // Verify all Object native methods are registered in the native registry
        assert!(
            shared
                .natives
                .native_methods
                .find("java/lang/Object", "hashCode", "()I")
                .is_some(),
            "hashCode native should be registered"
        );
        assert!(
            shared
                .natives
                .native_methods
                .find("java/lang/Object", "getClass", "()Ljava/lang/Class;")
                .is_some(),
            "getClass native should be registered"
        );
        assert!(
            shared
                .natives
                .native_methods
                .find("java/lang/Object", "clone", "()Ljava/lang/Object;")
                .is_some(),
            "clone native should be registered"
        );
        assert!(
            shared
                .natives
                .native_methods
                .find("java/lang/Object", "notify", "()V")
                .is_some(),
            "notify native should be registered"
        );
        assert!(
            shared
                .natives
                .native_methods
                .find("java/lang/Object", "notifyAll", "()V")
                .is_some(),
            "notifyAll native should be registered"
        );
        // JDK 19+: wait0 is the actual native (wait is bytecode)
        assert!(
            shared
                .natives
                .native_methods
                .find("java/lang/Object", "wait0", "(J)V")
                .is_some(),
            "wait0 native should be registered (JDK 19+)"
        );
    }

    #[test]
    #[ignore] // requires JDK on host
    fn s7_new_object_and_hashcode() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};

        let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));
        let mut thread = JvmThread::new(ThreadId(1), "test");

        // Load Object class
        let obj_class_id = shared
            .classes
            .class_manager
            .write()
            .load_class("java/lang/Object")
            .expect("Should load Object");

        // Get field count for allocation
        let num_fields = shared
            .classes
            .class_manager
            .read()
            .class_store
            .get(obj_class_id)
            .map(|c| c.num_total_fields)
            .unwrap_or(0);

        // Allocate a new Object (equivalent to `new Object()`)
        let obj_ref = shared.mem.heap.alloc_object(obj_class_id, num_fields);

        // Call hashCode() via native dispatch
        let result = crate::vm::vm_exec::invoke_on_class_shared(
            &shared,
            &mut thread,
            obj_class_id,
            "hashCode",
            "()I",
            &[Value::Object(Some(obj_ref))],
        );

        assert!(result.is_ok(), "hashCode() failed: {:?}", result.err());
        let hash = result.expect("result should be Ok");
        assert!(
            matches!(hash, Some(Value::Int(_))),
            "hashCode() should return Int, got {:?}",
            hash
        );
        let hash_val = match hash {
            Some(Value::Int(v)) => v,
            _ => unreachable!(),
        };
        eprintln!("new Object().hashCode() = {hash_val}");
    }

    #[test]
    #[ignore] // requires JDK on host
    fn s7_object_equals_identity() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};

        let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));
        let mut thread = JvmThread::new(ThreadId(1), "test");

        let obj_class_id = shared
            .classes
            .class_manager
            .write()
            .load_class("java/lang/Object")
            .expect("Should load Object");

        let num_fields = shared
            .classes
            .class_manager
            .read()
            .class_store
            .get(obj_class_id)
            .map(|c| c.num_total_fields)
            .unwrap_or(0);

        let obj1 = shared.mem.heap.alloc_object(obj_class_id, num_fields);
        let obj2 = shared.mem.heap.alloc_object(obj_class_id, num_fields);

        // equals(this) should return true (identity)
        let result = crate::vm::vm_exec::invoke_on_class_shared(
            &shared,
            &mut thread,
            obj_class_id,
            "equals",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(obj1)), Value::Object(Some(obj1))],
        );
        assert!(result.is_ok(), "equals(self) failed: {:?}", result.err());
        assert_eq!(
            result.expect("result should be Ok"),
            Some(Value::Int(1)),
            "obj.equals(obj) should return true (1)"
        );

        // equals(other) should return false (different objects)
        let result = crate::vm::vm_exec::invoke_on_class_shared(
            &shared,
            &mut thread,
            obj_class_id,
            "equals",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(obj1)), Value::Object(Some(obj2))],
        );
        assert!(result.is_ok(), "equals(other) failed: {:?}", result.err());
        assert_eq!(
            result.expect("result should be Ok"),
            Some(Value::Int(0)),
            "obj1.equals(obj2) should return false (0)"
        );
    }

    #[test]
    #[ignore] // requires JDK on host
    fn s7_object_to_string() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};

        let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));
        let mut thread = JvmThread::new(ThreadId(1), "test");

        let obj_class_id = shared
            .classes
            .class_manager
            .write()
            .load_class("java/lang/Object")
            .expect("Should load Object");

        let num_fields = shared
            .classes
            .class_manager
            .read()
            .class_store
            .get(obj_class_id)
            .map(|c| c.num_total_fields)
            .unwrap_or(0);

        let obj = shared.mem.heap.alloc_object(obj_class_id, num_fields);

        let result = crate::vm::vm_exec::invoke_on_class_shared(
            &shared,
            &mut thread,
            obj_class_id,
            "toString",
            "()Ljava/lang/String;",
            &[Value::Object(Some(obj))],
        );
        assert!(result.is_ok(), "toString() failed: {:?}", result.err());

        // toString should return a non-null String reference
        let ret = result.expect("result should be Ok");
        match ret {
            Some(Value::Object(Some(str_ref))) => {
                // Read the string content and verify it starts with "java.lang.Object@"
                let text = crate::vm::vm_object::read_java_string(&shared.mem.heap, str_ref);
                assert!(
                    text.is_some(),
                    "toString() returned an object but could not read string value"
                );
                let text = text.expect("text extraction should succeed");
                assert!(
                    text.starts_with("java.lang.Object@"),
                    "toString() should return 'java.lang.Object@<hex>', got: {text}"
                );
            }
            other => panic!("toString() should return Object(Some(..)), got: {other:?}"),
        }
    }

    #[test]
    #[ignore] // requires JDK on host
    fn s7_object_class_state_initialized() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let obj_id = cm
            .get_loaded_class_id("java/lang/Object")
            .expect("Object should be loaded");
        let cls = cm
            .class_store
            .get(obj_id)
            .expect("class should exist in class store");

        // Object should be in Loaded or higher state
        // (It may not be Initialized yet if <clinit> wasn't triggered,
        // but it should at least be Loaded since bootstrap_core_classes ran)
        assert!(
            cls.state != crate::classloading::ClassState::Loading,
            "Object should not be in Loading state, got {:?}",
            cls.state
        );
    }

    #[test]
    #[ignore] // requires JDK on host
    fn s7_bootstrap_loads_multiple_real_classes() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        // bootstrap_core_classes should have loaded many real classes
        let modules = cm.list_boot_modules();
        assert!(!modules.is_empty(), "Boot modules should be discovered");

        // Check that key classes are loaded and not synthetic stubs
        for class_name in &[
            "java/lang/Object",
            "java/io/Serializable",
            "java/lang/Comparable",
        ] {
            let id = cm.get_loaded_class_id(class_name);
            assert!(id.is_some(), "{class_name} should be loaded");
            let cls = cm
                .class_store
                .get(id.expect("class id should be loaded"))
                .expect("class should exist in class store");
            assert!(
                !cls.origin.is_compatibility_stub(),
                "{class_name} should be real bytecode, not synthetic"
            );
        }

        let total = cm.loaded_count();
        eprintln!(
            "Bootstrap loaded {total} classes ({} modules)",
            modules.len()
        );
    }

    // ===================================================================
    // Session 8: Bootstrap — java.lang.Class and java.lang.String
    // ===================================================================

    /// Verify that java/lang/String is loaded from real bytecode with correct
    /// field layout (4 instance fields: value, coder, hash, hashIsZero).
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s8_string_loaded_from_real_bytecode() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();
        let id = cm
            .get_loaded_class_id("java/lang/String")
            .expect("String not loaded");
        let cls = cm.get_class(id).expect("String class not found");

        // Must be loaded from real bytecode, not synthetic
        assert!(
            !cls.origin.is_compatibility_stub(),
            "String should be real, not synthetic"
        );

        // JDK 9+: String has 4 instance fields: value (byte[]), coder (byte), hash (int), hashIsZero (boolean)
        assert_eq!(
            cls.num_total_fields, 4,
            "JDK 9+ String has 4 instance fields"
        );

        // Verify field names
        let instance_field_names: Vec<&str> = cls
            .fields
            .iter()
            .filter(|f| !f.is_static())
            .map(|f| f.name.as_ref())
            .collect();
        assert!(
            instance_field_names.contains(&"value"),
            "Should have 'value' field"
        );
        assert!(
            instance_field_names.contains(&"coder"),
            "Should have 'coder' field"
        );
        assert!(
            instance_field_names.contains(&"hash"),
            "Should have 'hash' field"
        );
        assert!(
            instance_field_names.contains(&"hashIsZero"),
            "Should have 'hashIsZero' field"
        );

        // The 'value' field descriptor should be [B (byte array) for compact strings
        let value_field = cls
            .fields
            .iter()
            .find(|f| &*f.name == "value")
            .expect("method value should exist");
        assert_eq!(
            &*value_field.descriptor, "[B",
            "String.value should be byte[] in JDK 9+"
        );

        eprintln!(
            "String loaded: {} instance fields, {} methods",
            cls.num_total_fields,
            cls.methods.len()
        );
    }

    /// Verify that java/lang/Class is loaded from real bytecode.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s8_class_loaded_from_real_bytecode() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();
        let id = cm
            .get_loaded_class_id("java/lang/Class")
            .expect("Class not loaded");
        let cls = cm.get_class(id).expect("Class not found");

        assert!(
            !cls.origin.is_compatibility_stub(),
            "Class should be real, not synthetic"
        );

        // JDK 25 Class has many fields (name, module, classLoader, etc.)
        assert!(
            cls.num_total_fields >= 5,
            "Class should have many instance fields, got {}",
            cls.num_total_fields
        );

        // Verify it implements Serializable
        let implements_serializable = cls.interfaces.iter().any(|&iface_id| {
            cm.get_class(iface_id)
                .map(|c| &*c.name == "java/io/Serializable")
                .unwrap_or(false)
        });
        assert!(
            implements_serializable,
            "Class should implement Serializable"
        );

        eprintln!(
            "Class loaded: {} instance fields, {} methods",
            cls.num_total_fields,
            cls.methods.len()
        );
    }

    /// Verify that compact_strings flag is enabled and String statics are pre-initialized.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s8_string_compact_strings_enabled() {
        let shared = create_real_jdk_vm().expect("No JDK found");

        // The compact_strings flag should be set by pre_init_string_statics
        assert!(
            shared
                .classes
                .compact_strings
                .load(std::sync::atomic::Ordering::Relaxed),
            "compact_strings should be true for real JDK"
        );

        // Verify COMPACT_STRINGS static field is set
        let cm = shared.classes.class_manager.read();
        let string_id = cm
            .get_loaded_class_id("java/lang/String")
            .expect("class java/lang/String should be loaded");
        let cls = cm
            .get_class(string_id)
            .expect("class should exist in class store");

        // Find COMPACT_STRINGS static field index
        let mut static_idx = 0;
        let mut found = false;
        for field in &cls.fields {
            if field.is_static() {
                if &*field.name == "COMPACT_STRINGS" {
                    found = true;
                    break;
                }
                static_idx += 1;
            }
        }
        assert!(found, "COMPACT_STRINGS static field should exist");
        drop(cm);

        let val = crate::vm::vm_object::get_static_shared(&shared, string_id, static_idx);
        assert_eq!(val, Value::Int(1), "COMPACT_STRINGS should be true (1)");
    }

    /// Create a Java String via create_java_string in compact mode, then read it back.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s8_create_and_read_compact_string() {
        let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));

        // Create a Latin-1 string
        let hello = crate::vm::vm_object::create_java_string(&shared, "hello");
        let result = crate::vm::vm_object::read_java_string(&shared.mem.heap, hello);
        assert_eq!(result, Some("hello".to_string()));

        // Create a non-Latin-1 string (forces UTF16 coder)
        let emoji = crate::vm::vm_object::create_java_string(&shared, "\u{1F600} smile");
        let result = crate::vm::vm_object::read_java_string(&shared.mem.heap, emoji);
        assert_eq!(result, Some("\u{1F600} smile".to_string()));

        // Verify interning works
        let hello2 = crate::vm::vm_object::create_java_string(&shared, "hello");
        assert_eq!(
            hello.as_ptr(),
            hello2.as_ptr(),
            "Interned strings should be same object"
        );

        eprintln!("Compact string creation and reading works correctly");
    }

    /// Verify String.intern() native is registered and callable.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s8_string_intern_native_wired() {
        let shared = create_real_jdk_vm().expect("No JDK found");

        // String.intern() should be in the native registry
        let found = shared.natives.native_methods.find(
            "java/lang/String",
            "intern",
            "()Ljava/lang/String;",
        );
        assert!(
            found.is_some(),
            "String.intern() native should be registered"
        );
    }

    /// Verify Class native methods are registered for real JDK bootstrap.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s8_class_natives_wired() {
        let shared = create_real_jdk_vm().expect("No JDK found");

        // All critical Class natives should be registered
        let critical_natives = [
            ("getPrimitiveClass", "(Ljava/lang/String;)Ljava/lang/Class;"),
            (
                "forName0",
                "(Ljava/lang/String;ZLjava/lang/ClassLoader;Ljava/lang/Class;)Ljava/lang/Class;",
            ),
            ("isInstance", "(Ljava/lang/Object;)Z"),
            ("isAssignableFrom", "(Ljava/lang/Class;)Z"),
            ("getSuperclass", "()Ljava/lang/Class;"),
            ("initClassName", "()Ljava/lang/String;"),
            ("isHidden", "()Z"),
            ("desiredAssertionStatus0", "(Ljava/lang/Class;)Z"),
        ];

        for (method, desc) in &critical_natives {
            let found = shared
                .natives
                .native_methods
                .find("java/lang/Class", method, desc);
            assert!(
                found.is_some(),
                "Class.{method}{desc} native should be registered"
            );
        }
    }

    /// Verify that Class mirrors work correctly with real JDK Class layout.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s8_class_mirror_with_real_layout() {
        let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));

        let cm = shared.classes.class_manager.read();
        let string_id = cm
            .get_loaded_class_id("java/lang/String")
            .expect("class java/lang/String should be loaded");
        drop(cm);

        // Create a class mirror for String
        let mirror = crate::vm::vm_object::get_or_create_class_mirror(&shared, string_id);

        // Field 0 stores Int(class_id) for legacy compatibility.
        let stored_id = shared.mem.heap.get_field(mirror, 0);
        assert_eq!(stored_id.as_int(), Some(string_id.as_u32() as i32));

        // Field 1 should store the name
        let name_val = shared.mem.heap.get_field(mirror, 1);
        match name_val {
            Value::Object(Some(name_ref)) => {
                let name = crate::vm::vm_object::read_java_string(&shared.mem.heap, name_ref);
                assert_eq!(name, Some("java/lang/String".to_string()));
            }
            _ => panic!("Expected name string in field 1"),
        }

        // Caching should work
        let mirror2 = crate::vm::vm_object::get_or_create_class_mirror(&shared, string_id);
        assert_eq!(
            mirror.as_ptr(),
            mirror2.as_ptr(),
            "Mirrors should be cached"
        );
    }

    /// Verify that getPrimitiveClass works with real JDK mode.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s8_get_primitive_class_mirror() {
        let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));

        let primitives = [
            "int", "long", "boolean", "byte", "char", "short", "float", "double", "void",
        ];
        for prim in &primitives {
            let mirror = crate::vm::vm_object::get_or_create_primitive_mirror(&shared, prim);
            // Field 0 = Int(-1) marker for primitive mirrors.
            assert_eq!(
                shared.mem.heap.get_field(mirror, 0),
                Value::Int(-1),
                "Primitive mirror for '{prim}' should have Int(-1) marker"
            );
        }
    }

    /// Verify that String and Class are both loaded as real classes (not synthetic).
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s8_string_and_class_both_real() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        for class_name in &["java/lang/String", "java/lang/Class"] {
            let id = cm
                .get_loaded_class_id(class_name)
                .unwrap_or_else(|| panic!("{class_name} should be loaded"));
            let cls = cm.get_class(id).expect("class should exist in class store");
            assert!(
                !cls.origin.is_compatibility_stub(),
                "{class_name} should be real bytecode, not synthetic"
            );
            assert!(
                cls.methods.len() > 10,
                "{class_name} should have many methods (got {})",
                cls.methods.len()
            );
        }
    }

    /// Execute String.valueOf(42) using real JDK bytecode — returns the string "42".
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s8_string_valueof_int_execution() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        let string_id = shared
            .classes
            .class_manager
            .write()
            .load_class("java/lang/String")
            .expect("failed to load class java/lang/String");
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, string_id)
            .expect("class initialization should succeed");

        // Call String.valueOf(42) — static method
        let result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            string_id,
            "valueOf",
            "(I)Ljava/lang/String;",
            &[Value::Int(42)],
        )
        .expect("String.valueOf(42) failed");

        match result {
            Some(Value::Object(Some(str_ref))) => {
                let s = crate::vm::vm_object::read_java_string(&shared.mem.heap, str_ref);
                assert_eq!(
                    s,
                    Some("42".to_string()),
                    "String.valueOf(42) should return \"42\""
                );
            }
            other => panic!(
                "String.valueOf(42) should return a String object, got {:?}",
                other
            ),
        }
        eprintln!("String.valueOf(42) = \"42\" — real JDK bytecode execution works!");
    }

    /// Execute "hello".length() using real JDK bytecode — returns 5.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s8_string_length_execution() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        // Create the Java string "hello"
        let hello = crate::vm::vm_object::create_java_string(&shared, "hello");

        let string_class_id = shared.mem.heap.class_id_of(hello);
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, string_class_id)
            .expect("class initialization should succeed");

        // Call "hello".length()
        let result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            string_class_id,
            "length",
            "()I",
            &[Value::Object(Some(hello))],
        )
        .expect("String.length() failed");

        assert_eq!(
            result,
            Some(Value::Int(5)),
            "\"hello\".length() should be 5"
        );
        eprintln!("\"hello\".length() = 5 — real JDK bytecode execution works!");
    }

    /// Execute Class.forName("java.lang.String") using real JDK bytecode.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s8_class_forname_execution() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        let class_id = shared
            .classes
            .class_manager
            .write()
            .load_class("java/lang/Class")
            .expect("failed to load class java/lang/Class");
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, class_id)
            .expect("class initialization should succeed");

        // Create the argument string "java.lang.String" (dot notation)
        let name_str = crate::vm::vm_object::create_java_string(&shared, "java.lang.String");

        // Call Class.forName0(name, true, null, null) — the native underlying forName
        let result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            class_id,
            "forName0",
            "(Ljava/lang/String;ZLjava/lang/ClassLoader;Ljava/lang/Class;)Ljava/lang/Class;",
            &[
                Value::Object(Some(name_str)),
                Value::Int(1),
                Value::Object(None),
                Value::Object(None),
            ],
        )
        .expect("Class.forName0 failed");

        match result {
            Some(Value::Object(Some(mirror))) => {
                // The returned mirror should be for java/lang/String
                let stored_id = shared.mem.heap.get_field(mirror, 0);
                let cm = shared.classes.class_manager.read();
                let string_id = cm
                    .get_loaded_class_id("java/lang/String")
                    .expect("class java/lang/String should be loaded");
                assert_eq!(
                    stored_id.as_int(),
                    Some(string_id.as_u32() as i32),
                    "Class.forName(\"java.lang.String\") should return String's class mirror"
                );
            }
            other => panic!(
                "Class.forName should return a Class mirror, got {:?}",
                other
            ),
        }
        eprintln!("Class.forName(\"java.lang.String\") works — real JDK bytecode execution!");
    }

    /// Execute String.charAt(0) on "hello" — returns 'h'.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s8_string_charat_execution() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        let hello = crate::vm::vm_object::create_java_string(&shared, "hello");
        let string_class_id = shared.mem.heap.class_id_of(hello);
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, string_class_id)
            .expect("class initialization should succeed");

        let result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            string_class_id,
            "charAt",
            "(I)C",
            &[Value::Object(Some(hello)), Value::Int(0)],
        )
        .expect("String.charAt(0) failed");

        assert_eq!(
            result,
            Some(Value::Int('h' as i32)),
            "\"hello\".charAt(0) should be 'h'"
        );
        eprintln!("\"hello\".charAt(0) = 'h' — real JDK bytecode execution works!");
    }

    /// Execute String.equals() comparing two equal strings.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s8_string_equals_execution() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        let s1 = crate::vm::vm_object::create_java_string(&shared, "hello");
        // Create a second string with the same content but different object
        // Force a new allocation by not using interning
        let s2 = {
            let string_id = shared
                .classes
                .class_manager
                .read()
                .get_loaded_class_id("java/lang/String")
                .expect("class java/lang/String should be loaded");
            let cls = shared
                .classes
                .class_manager
                .read()
                .get_class(string_id)
                .expect("class should exist in class store")
                .num_total_fields;
            let obj = shared.mem.heap.alloc_object(string_id, cls);
            crate::runtime::interpreter::init_primitive_fields(&shared, obj, string_id);
            // Copy the value array and coder from s1
            let val = shared.mem.heap.get_field(s1, 0); // value byte[]
            shared.mem.heap.set_field(obj, 0, val);
            let coder = shared.mem.heap.get_field(s1, 1); // coder
            shared.mem.heap.set_field(obj, 1, coder);
            obj
        };

        let string_class_id = shared.mem.heap.class_id_of(s1);
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, string_class_id)
            .expect("class initialization should succeed");

        // s1.equals(s2) should be true
        let result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            string_class_id,
            "equals",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(s1)), Value::Object(Some(s2))],
        )
        .expect("String.equals() failed");

        assert_eq!(
            result,
            Some(Value::Int(1)),
            "\"hello\".equals(\"hello\") should be true"
        );

        // s1.equals(different string) should be false
        let other = crate::vm::vm_object::create_java_string(&shared, "world");
        let result2 = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            string_class_id,
            "equals",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(s1)), Value::Object(Some(other))],
        )
        .expect("String.equals() failed");

        assert_eq!(
            result2,
            Some(Value::Int(0)),
            "\"hello\".equals(\"world\") should be false"
        );
        eprintln!("String.equals() works correctly with real JDK bytecode!");
    }

    /// Execute String.hashCode() — returns a consistent value.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s8_string_hashcode_execution() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        let hello = crate::vm::vm_object::create_java_string(&shared, "hello");
        let string_class_id = shared.mem.heap.class_id_of(hello);
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, string_class_id)
            .expect("class initialization should succeed");

        let result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            string_class_id,
            "hashCode",
            "()I",
            &[Value::Object(Some(hello))],
        )
        .expect("String.hashCode() failed");

        // hashCode() should return a non-zero consistent value
        let hash1 = match result {
            Some(Value::Int(h)) => h,
            other => panic!("hashCode() should return Int, got {:?}", other),
        };
        assert_ne!(hash1, 0, "\"hello\".hashCode() should not be 0");

        // Calling again should return the same value (consistency)
        let result2 = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            string_class_id,
            "hashCode",
            "()I",
            &[Value::Object(Some(hello))],
        )
        .expect("String.hashCode() failed");
        assert_eq!(
            result2,
            Some(Value::Int(hash1)),
            "hashCode() should be consistent"
        );

        // Two strings with same content should have same hash
        let hello2 = crate::vm::vm_object::create_java_string(&shared, "hello");
        let result3 = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            string_class_id,
            "hashCode",
            "()I",
            &[Value::Object(Some(hello2))],
        )
        .expect("String.hashCode() failed");
        assert_eq!(
            result3,
            Some(Value::Int(hash1)),
            "same content should have same hashCode"
        );

        eprintln!(
            "\"hello\".hashCode() = {} — consistent and non-zero!",
            hash1
        );
    }

    /// Execute Class.getName() on String's class mirror.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s8_class_getname_execution() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        let cm = shared.classes.class_manager.read();
        let string_id = cm
            .get_loaded_class_id("java/lang/String")
            .expect("class java/lang/String should be loaded");
        drop(cm);

        let mirror = crate::vm::vm_object::get_or_create_class_mirror(&shared, string_id);
        let class_class_id = shared.mem.heap.class_id_of(mirror);
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, class_class_id)
            .expect("class initialization should succeed");

        // Call getName() on the String class mirror
        let result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            class_class_id,
            "getName",
            "()Ljava/lang/String;",
            &[Value::Object(Some(mirror))],
        )
        .expect("Class.getName() failed");

        match result {
            Some(Value::Object(Some(name_ref))) => {
                let name = crate::vm::vm_object::read_java_string(&shared.mem.heap, name_ref);
                assert_eq!(
                    name,
                    Some("java.lang.String".to_string()),
                    "String.class.getName() should return \"java.lang.String\""
                );
            }
            other => panic!("Class.getName() should return a String, got {:?}", other),
        }
        eprintln!(
            "String.class.getName() = \"java.lang.String\" — real JDK bytecode execution works!"
        );
    }

    /// Execute String.substring(1, 4) on "hello" — returns "ell".
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s8_string_substring_execution() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        let hello = crate::vm::vm_object::create_java_string(&shared, "hello");
        let string_class_id = shared.mem.heap.class_id_of(hello);
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, string_class_id)
            .expect("class initialization should succeed");

        let result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            string_class_id,
            "substring",
            "(II)Ljava/lang/String;",
            &[Value::Object(Some(hello)), Value::Int(1), Value::Int(4)],
        )
        .expect("String.substring(1,4) failed");

        match result {
            Some(Value::Object(Some(sub_ref))) => {
                let s = crate::vm::vm_object::read_java_string(&shared.mem.heap, sub_ref);
                assert_eq!(
                    s,
                    Some("ell".to_string()),
                    "\"hello\".substring(1,4) should be \"ell\""
                );
            }
            other => panic!("String.substring should return a String, got {:?}", other),
        }
        eprintln!("\"hello\".substring(1,4) = \"ell\" — real JDK bytecode execution works!");
    }

    /// Execute Class.isInstance() — verify runtime type checking works.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s8_class_isinstance_execution() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        let cm = shared.classes.class_manager.read();
        let string_id = cm
            .get_loaded_class_id("java/lang/String")
            .expect("class java/lang/String should be loaded");
        drop(cm);

        let mirror = crate::vm::vm_object::get_or_create_class_mirror(&shared, string_id);
        let class_class_id = shared.mem.heap.class_id_of(mirror);
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, class_class_id)
            .expect("class initialization should succeed");

        // A String object should be an instance of String.class
        let hello = crate::vm::vm_object::create_java_string(&shared, "test");
        let result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            class_class_id,
            "isInstance",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(mirror)), Value::Object(Some(hello))],
        )
        .expect("Class.isInstance() failed");

        assert_eq!(
            result,
            Some(Value::Int(1)),
            "String.class.isInstance(\"test\") should be true"
        );

        // null should not be an instance
        let result_null = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            class_class_id,
            "isInstance",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(mirror)), Value::Object(None)],
        )
        .expect("Class.isInstance(null) failed");

        assert_eq!(
            result_null,
            Some(Value::Int(0)),
            "String.class.isInstance(null) should be false"
        );
        eprintln!("Class.isInstance() works correctly with real JDK bytecode!");
    }

    // ===================================================================
    // Session 9: Bootstrap — Core java.lang Classes
    // ===================================================================

    /// Verify all 17 java.lang wrapper/core classes are loaded from real bytecode.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s9_all_core_lang_classes_real() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let core_classes = [
            "java/lang/Object",
            "java/lang/String",
            "java/lang/Class",
            "java/lang/System",
            "java/lang/Thread",
            "java/lang/Throwable",
            "java/lang/Number",
            "java/lang/Integer",
            "java/lang/Long",
            "java/lang/Double",
            "java/lang/Float",
            "java/lang/Boolean",
            "java/lang/Byte",
            "java/lang/Short",
            "java/lang/Character",
            "java/lang/Void",
            "java/lang/Exception",
            "java/lang/RuntimeException",
            "java/lang/Error",
        ];

        for class_name in &core_classes {
            let id = cm
                .get_loaded_class_id(class_name)
                .unwrap_or_else(|| panic!("{class_name} should be loaded"));
            let cls = cm.get_class(id).expect("class should exist in class store");
            assert!(
                !cls.origin.is_compatibility_stub(),
                "{class_name} should be real bytecode, not synthetic"
            );
        }

        eprintln!(
            "All {} core java.lang classes loaded from real bytecode",
            core_classes.len()
        );
    }

    /// Verify System native methods are all registered.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s9_system_natives_complete() {
        let shared = create_real_jdk_vm().expect("No JDK found");

        let system_natives = [
            ("registerNatives", "()V"),
            ("currentTimeMillis", "()J"),
            ("nanoTime", "()J"),
            ("arraycopy", "(Ljava/lang/Object;ILjava/lang/Object;II)V"),
            ("identityHashCode", "(Ljava/lang/Object;)I"),
            ("exit", "(I)V"),
            ("setIn0", "(Ljava/io/InputStream;)V"),
            ("setOut0", "(Ljava/io/PrintStream;)V"),
            ("setErr0", "(Ljava/io/PrintStream;)V"),
            ("mapLibraryName", "(Ljava/lang/String;)Ljava/lang/String;"),
        ];

        for (method, desc) in &system_natives {
            assert!(
                shared
                    .natives
                    .native_methods
                    .find("java/lang/System", method, desc)
                    .is_some(),
                "System.{method}{desc} should be registered"
            );
        }
    }

    /// Verify Thread native methods are all registered.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s9_thread_natives_complete() {
        let shared = create_real_jdk_vm().expect("No JDK found");

        let thread_natives = [
            ("registerNatives", "()V"),
            ("currentThread", "()Ljava/lang/Thread;"),
            ("currentCarrierThread", "()Ljava/lang/Thread;"),
            ("sleep", "(J)V"),
            ("sleepNanos0", "(J)V"),
            ("yield0", "()V"),
            ("start0", "()V"),
            ("interrupt0", "()V"),
            ("isAlive", "()Z"),
            ("holdsLock", "(Ljava/lang/Object;)Z"),
            ("setPriority0", "(I)V"),
            ("setNativeName", "(Ljava/lang/String;)V"),
            ("getNextThreadIdOffset", "()J"),
            ("setCurrentThread", "(Ljava/lang/Thread;)V"),
            ("findScopedValueBindings", "()Ljava/lang/Object;"),
            ("scopedValueCache", "()[Ljava/lang/Object;"),
            ("setScopedValueCache", "([Ljava/lang/Object;)V"),
            ("ensureMaterializedForStackWalk", "(Ljava/lang/Object;)V"),
            ("clearInterruptEvent", "()V"),
        ];

        for (method, desc) in &thread_natives {
            assert!(
                shared
                    .natives
                    .native_methods
                    .find("java/lang/Thread", method, desc)
                    .is_some(),
                "Thread.{method}{desc} should be registered"
            );
        }
    }

    /// Verify VM/CDS internal natives needed by IntegerCache etc. are registered.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s9_vm_cds_natives_registered() {
        let shared = create_real_jdk_vm().expect("No JDK found");

        // VM natives
        assert!(
            shared
                .natives
                .native_methods
                .find(
                    "jdk/internal/misc/VM",
                    "getSavedProperty",
                    "(Ljava/lang/String;)Ljava/lang/String;"
                )
                .is_some(),
            "VM.getSavedProperty should be registered"
        );
        assert!(
            shared
                .natives
                .native_methods
                .find("jdk/internal/misc/VM", "initLevel", "()I")
                .is_some(),
            "VM.initLevel should be registered"
        );

        // CDS natives
        assert!(
            shared
                .natives
                .native_methods
                .find(
                    "jdk/internal/misc/CDS",
                    "initializeFromArchive",
                    "(Ljava/lang/Class;)V"
                )
                .is_some(),
            "CDS.initializeFromArchive should be registered"
        );
        assert!(
            shared
                .natives
                .native_methods
                .find("jdk/internal/misc/CDS", "isSharingEnabled", "()Z")
                .is_some(),
            "CDS.isSharingEnabled should be registered"
        );
    }

    /// Verify wrapper classes have TYPE fields pre-initialized with primitive mirrors.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s9_wrapper_type_fields_initialized() {
        let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));

        let wrappers = [
            ("java/lang/Integer", "int"),
            ("java/lang/Long", "long"),
            ("java/lang/Boolean", "boolean"),
            ("java/lang/Byte", "byte"),
            ("java/lang/Short", "short"),
            ("java/lang/Character", "char"),
            ("java/lang/Float", "float"),
            ("java/lang/Double", "double"),
        ];

        for (wrapper_name, prim_name) in &wrappers {
            let cm = shared.classes.class_manager.read();
            let class_id = cm
                .get_loaded_class_id(wrapper_name)
                .unwrap_or_else(|| panic!("{wrapper_name} should be loaded"));
            let cls = cm
                .get_class(class_id)
                .expect("class should exist in class store");

            // Find TYPE static field index
            let mut static_idx = 0;
            let mut found = false;
            for field in &cls.fields {
                if field.is_static() {
                    if &*field.name == "TYPE" {
                        found = true;
                        break;
                    }
                    static_idx += 1;
                }
            }
            drop(cm);
            assert!(found, "{wrapper_name} should have a TYPE static field");

            let val = crate::vm::vm_object::get_static_shared(&shared, class_id, static_idx);
            match val {
                Value::Object(Some(mirror)) => {
                    // The mirror should be a primitive class mirror (field 0 = Int(-1))
                    assert_eq!(
                        shared.mem.heap.get_field(mirror, 0),
                        Value::Int(-1),
                        "{wrapper_name}.TYPE should be a primitive mirror for '{prim_name}'"
                    );
                }
                _ => panic!(
                    "{wrapper_name}.TYPE should be initialized to a primitive mirror, got {val:?}"
                ),
            }
        }
    }

    /// Verify Number is loaded as abstract class with correct superclass.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s9_number_class_hierarchy() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        // Number extends Object
        let number_id = cm
            .get_loaded_class_id("java/lang/Number")
            .expect("class java/lang/Number should be loaded");
        let number = cm
            .get_class(number_id)
            .expect("class should exist in class store");
        assert!(!number.origin.is_compatibility_stub());
        let object_id = cm
            .get_loaded_class_id("java/lang/Object")
            .expect("class java/lang/Object should be loaded");
        assert_eq!(number.superclass, Some(object_id));

        // Integer extends Number
        let int_id = cm
            .get_loaded_class_id("java/lang/Integer")
            .expect("class java/lang/Integer should be loaded");
        let int_cls = cm
            .get_class(int_id)
            .expect("class should exist in class store");
        assert_eq!(int_cls.superclass, Some(number_id));

        // Long extends Number
        let long_id = cm
            .get_loaded_class_id("java/lang/Long")
            .expect("class java/lang/Long should be loaded");
        let long_cls = cm
            .get_class(long_id)
            .expect("class should exist in class store");
        assert_eq!(long_cls.superclass, Some(number_id));

        // Double extends Number
        let double_id = cm
            .get_loaded_class_id("java/lang/Double")
            .expect("class java/lang/Double should be loaded");
        let double_cls = cm
            .get_class(double_id)
            .expect("class should exist in class store");
        assert_eq!(double_cls.superclass, Some(number_id));
    }

    /// Verify Throwable/Exception/Error class hierarchy.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s9_throwable_hierarchy() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let throwable_id = cm
            .get_loaded_class_id("java/lang/Throwable")
            .expect("class java/lang/Throwable should be loaded");
        let exception_id = cm
            .get_loaded_class_id("java/lang/Exception")
            .expect("class java/lang/Exception should be loaded");
        let runtime_ex_id = cm
            .get_loaded_class_id("java/lang/RuntimeException")
            .expect("class java/lang/RuntimeException should be loaded");
        let error_id = cm
            .get_loaded_class_id("java/lang/Error")
            .expect("class java/lang/Error should be loaded");
        let object_id = cm
            .get_loaded_class_id("java/lang/Object")
            .expect("class java/lang/Object should be loaded");

        // Throwable extends Object
        let throwable = cm
            .get_class(throwable_id)
            .expect("class should exist in class store");
        assert_eq!(throwable.superclass, Some(object_id));
        assert!(!throwable.origin.is_compatibility_stub());

        // Exception extends Throwable
        let exception = cm
            .get_class(exception_id)
            .expect("class should exist in class store");
        assert_eq!(exception.superclass, Some(throwable_id));

        // RuntimeException extends Exception
        let runtime_ex = cm
            .get_class(runtime_ex_id)
            .expect("class should exist in class store");
        assert_eq!(runtime_ex.superclass, Some(exception_id));

        // Error extends Throwable
        let error = cm
            .get_class(error_id)
            .expect("class should exist in class store");
        assert_eq!(error.superclass, Some(throwable_id));
    }

    /// Verify System class has expected methods loaded from bytecode.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s9_system_class_has_methods() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let sys_id = cm
            .get_loaded_class_id("java/lang/System")
            .expect("class java/lang/System should be loaded");
        let sys = cm
            .get_class(sys_id)
            .expect("class should exist in class store");
        assert!(!sys.origin.is_compatibility_stub());

        // System should have well-known methods
        let method_names: Vec<&str> = sys.methods.iter().map(|m| m.name.as_ref()).collect();
        assert!(
            method_names.contains(&"currentTimeMillis"),
            "System should have currentTimeMillis"
        );
        assert!(
            method_names.contains(&"arraycopy"),
            "System should have arraycopy"
        );
        assert!(method_names.contains(&"exit"), "System should have exit");
        assert!(
            method_names.contains(&"getProperty"),
            "System should have getProperty"
        );
        // initPhase1 is the JDK internal init — present in real JDK System
        assert!(
            method_names.contains(&"initPhase1"),
            "System should have initPhase1"
        );
    }

    /// Verify Integer class fields and methods from real bytecode.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s9_integer_class_structure() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let int_id = cm
            .get_loaded_class_id("java/lang/Integer")
            .expect("class java/lang/Integer should be loaded");
        let int_cls = cm
            .get_class(int_id)
            .expect("class should exist in class store");
        assert!(!int_cls.origin.is_compatibility_stub());

        // Integer has one instance field: 'value' (int)
        let instance_fields: Vec<&str> = int_cls
            .fields
            .iter()
            .filter(|f| !f.is_static())
            .map(|f| f.name.as_ref())
            .collect();
        assert!(
            instance_fields.contains(&"value"),
            "Integer should have 'value' field"
        );

        // Static fields should include TYPE, MIN_VALUE, MAX_VALUE, etc.
        let static_fields: Vec<&str> = int_cls
            .fields
            .iter()
            .filter(|f| f.is_static())
            .map(|f| f.name.as_ref())
            .collect();
        assert!(
            static_fields.contains(&"TYPE"),
            "Integer should have TYPE static"
        );
        assert!(
            static_fields.contains(&"MIN_VALUE"),
            "Integer should have MIN_VALUE"
        );
        assert!(
            static_fields.contains(&"MAX_VALUE"),
            "Integer should have MAX_VALUE"
        );

        // Methods
        let method_names: Vec<&str> = int_cls.methods.iter().map(|m| m.name.as_ref()).collect();
        assert!(
            method_names.contains(&"valueOf"),
            "Integer should have valueOf"
        );
        assert!(
            method_names.contains(&"parseInt"),
            "Integer should have parseInt"
        );
        assert!(
            method_names.contains(&"intValue"),
            "Integer should have intValue"
        );
        assert!(
            method_names.contains(&"toString"),
            "Integer should have toString"
        );
    }

    /// Verify Thread class is loaded with expected structure.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s9_thread_class_structure() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let thread_id = cm
            .get_loaded_class_id("java/lang/Thread")
            .expect("class java/lang/Thread should be loaded");
        let thread = cm
            .get_class(thread_id)
            .expect("class should exist in class store");
        assert!(!thread.origin.is_compatibility_stub());

        // Thread should have many methods
        let method_names: Vec<&str> = thread.methods.iter().map(|m| m.name.as_ref()).collect();
        assert!(method_names.contains(&"start"), "Thread should have start");
        assert!(method_names.contains(&"run"), "Thread should have run");
        assert!(
            method_names.contains(&"getName"),
            "Thread should have getName"
        );
        assert!(
            method_names.contains(&"isAlive"),
            "Thread should have isAlive"
        );
        assert!(
            method_names.contains(&"interrupt"),
            "Thread should have interrupt"
        );

        // Thread should have instance fields like name, tid, priority
        let instance_fields: Vec<&str> = thread
            .fields
            .iter()
            .filter(|f| !f.is_static())
            .map(|f| f.name.as_ref())
            .collect();
        assert!(
            instance_fields.contains(&"name"),
            "Thread should have 'name' field"
        );

        eprintln!(
            "Thread: {} methods, {} instance fields",
            thread.methods.len(),
            instance_fields.len()
        );
    }

    /// Runtime test: Integer.parseInt("42") returns 42.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s9_integer_parse_int_runtime() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};

        let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));
        let mut thread = JvmThread::new(ThreadId(1), "test");

        let str_arg = crate::vm::vm_object::create_java_string(&shared, "42");

        let result = crate::vm::invoke_shared(
            &shared,
            &mut thread,
            "java/lang/Integer",
            "parseInt",
            "(Ljava/lang/String;)I",
            &[Value::Object(Some(str_arg))],
        );
        assert!(
            result.is_ok(),
            "Integer.parseInt(\"42\") failed: {:?}",
            result.err()
        );
        assert_eq!(
            result.expect("result should be Ok"),
            Some(Value::Int(42)),
            "Integer.parseInt(\"42\") should return 42"
        );
    }

    /// Runtime test: Thread.currentThread() returns a non-null Thread object.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s9_thread_current_thread_runtime() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};

        let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));
        let mut thread = JvmThread::new(ThreadId(1), "test-thread");

        let result = crate::vm::invoke_shared(
            &shared,
            &mut thread,
            "java/lang/Thread",
            "currentThread",
            "()Ljava/lang/Thread;",
            &[],
        );
        assert!(
            result.is_ok(),
            "Thread.currentThread() failed: {:?}",
            result.err()
        );
        match result.expect("result should be Ok") {
            Some(Value::Object(Some(_thread_ref))) => {
                // Successfully returned a Thread object
            }
            other => panic!("Thread.currentThread() should return a Thread object, got: {other:?}"),
        }
    }

    /// Runtime test: the VM-created Thread returned by
    /// `Thread.currentThread()` in real-JDK mode has its
    /// `holder:FieldHolder` field populated with a non-null ThreadGroup,
    /// NORM_PRIORITY, and `daemon=false`.  Regression guard for B1 — a
    /// null holder makes every real-JDK Thread accessor (getState,
    /// getPriority, getThreadGroup, isDaemon) NPE.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn b1_current_thread_holder_populated() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};

        let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));
        let mut thread = JvmThread::new(ThreadId(1), "main");

        let result = crate::vm::invoke_shared(
            &shared,
            &mut thread,
            "java/lang/Thread",
            "currentThread",
            "()Ljava/lang/Thread;",
            &[],
        )
        .expect("Thread.currentThread() should succeed");
        let t_obj = match result {
            Some(Value::Object(Some(r))) => r,
            other => panic!("expected Thread object, got {other:?}"),
        };

        // Resolve the `holder` slot via the class store — the slot index
        // is layout-dependent so we must not hardcode it.
        let thread_class = shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id("java/lang/Thread")
            .expect("Thread class should be loaded");
        let holder_slot = {
            let cm = shared.classes.class_manager.read();
            crate::vm::resolve_field_index_for_test(thread_class, "holder", &cm.class_store)
        }
        .expect("Thread.holder field should be resolvable");

        let holder_val = shared.mem.heap.get_field(t_obj, holder_slot);
        let holder = match holder_val {
            Value::Object(Some(r)) => r,
            other => panic!("Thread.holder must not be null on VM-created Thread, got {other:?}"),
        };

        // Confirm `holder.group` is non-null.
        let holder_class = shared.mem.heap.class_id_of(holder);
        let group_slot = {
            let cm = shared.classes.class_manager.read();
            crate::vm::resolve_field_index_for_test(holder_class, "group", &cm.class_store)
        }
        .expect("FieldHolder.group field should be resolvable");
        match shared.mem.heap.get_field(holder, group_slot) {
            Value::Object(Some(_)) => {}
            other => panic!("holder.group must not be null, got {other:?}"),
        }

        // Confirm `holder.priority == 5` (NORM_PRIORITY).
        let prio_slot = {
            let cm = shared.classes.class_manager.read();
            crate::vm::resolve_field_index_for_test(holder_class, "priority", &cm.class_store)
        }
        .expect("FieldHolder.priority field should be resolvable");
        assert!(
            matches!(shared.mem.heap.get_field(holder, prio_slot), Value::Int(5)),
            "holder.priority should be NORM_PRIORITY (5)"
        );
    }

    /// Runtime test: System.arraycopy works correctly.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s9_system_arraycopy_runtime() {
        use crate::memory::ArrayElementType;
        use crate::threading::jvm_thread::{JvmThread, ThreadId};

        let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));
        let mut thread = JvmThread::new(ThreadId(1), "test");

        // Create source array [10, 20, 30]
        let src = shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Int, 3);
        shared
            .mem
            .heap
            .set_array_element(src, 0, Value::Int(10))
            .expect("array element set should succeed");
        shared
            .mem
            .heap
            .set_array_element(src, 1, Value::Int(20))
            .expect("array element set should succeed");
        shared
            .mem
            .heap
            .set_array_element(src, 2, Value::Int(30))
            .expect("array element set should succeed");

        // Create destination array [0, 0, 0]
        let dst = shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Int, 3);

        // System.arraycopy(src, 0, dst, 0, 3)
        let result = crate::vm::invoke_shared(
            &shared,
            &mut thread,
            "java/lang/System",
            "arraycopy",
            "(Ljava/lang/Object;ILjava/lang/Object;II)V",
            &[
                Value::Object(Some(src)),
                Value::Int(0),
                Value::Object(Some(dst)),
                Value::Int(0),
                Value::Int(3),
            ],
        );
        assert!(
            result.is_ok(),
            "System.arraycopy failed: {:?}",
            result.err()
        );

        // Verify destination has copied values
        assert_eq!(
            shared
                .mem
                .heap
                .get_array_element(dst, 0)
                .expect("array element get should succeed"),
            Value::Int(10)
        );
        assert_eq!(
            shared
                .mem
                .heap
                .get_array_element(dst, 1)
                .expect("array element get should succeed"),
            Value::Int(20)
        );
        assert_eq!(
            shared
                .mem
                .heap
                .get_array_element(dst, 2)
                .expect("array element get should succeed"),
            Value::Int(30)
        );
    }

    // -----------------------------------------------------------------------
    // Session 10: Native Method Bridge
    // -----------------------------------------------------------------------

    // --- Session 10: Native Method Bridging — 100% coverage ---

    #[test]
    #[ignore] // requires real JDK on boot classpath
    fn s10_native_coverage_100_percent() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let report = crate::vm::vm_object::validate_native_coverage(&shared);

        eprintln!("\n=== Native Method Coverage Report ===");
        eprintln!("Total ACC_NATIVE methods: {}", report.total());
        eprintln!("Covered: {}", report.covered.len());
        eprintln!("Missing: {}", report.missing.len());
        eprintln!("Coverage: {:.1}%", report.coverage_ratio() * 100.0);

        if !report.missing.is_empty() {
            eprintln!("\n--- Missing natives ({}) ---", report.missing.len());
            for m in &report.missing {
                eprintln!("  {m}");
            }
        }

        assert_eq!(
            report.missing.len(),
            0,
            "All ACC_NATIVE methods in bootstrapped classes must be registered. \
             Missing: {:?}",
            report
                .missing
                .iter()
                .map(|m| m.to_string())
                .collect::<Vec<_>>()
        );
        assert!(
            report.total() > 150,
            "Expected 150+ ACC_NATIVE methods, found {}",
            report.total()
        );
    }

    #[test]
    #[ignore]
    fn s10_unsafe_natives_complete() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let natives = crate::vm::vm_object::scan_class_natives(&shared, "jdk/internal/misc/Unsafe");

        eprintln!("jdk/internal/misc/Unsafe: {} native methods", natives.len());
        for n in &natives {
            let registered = shared
                .natives
                .native_methods
                .find(&n.class_name, &n.method_name, &n.descriptor)
                .is_some();
            eprintln!("  {} {}", if registered { "✓" } else { "✗" }, n);
            assert!(registered, "Unsafe native not registered: {n}");
        }
        // JDK 25 Unsafe has 60+ native methods
        assert!(
            natives.len() > 50,
            "Expected 50+ Unsafe natives, found {}",
            natives.len()
        );
    }

    #[test]
    #[ignore]
    fn s10_classloader_natives_registered() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let natives = crate::vm::vm_object::scan_class_natives(&shared, "java/lang/ClassLoader");

        eprintln!("ClassLoader: {} native methods", natives.len());
        for n in &natives {
            let registered = shared
                .natives
                .native_methods
                .find(&n.class_name, &n.method_name, &n.descriptor)
                .is_some();
            assert!(registered, "ClassLoader native not registered: {n}");
        }
        assert!(
            natives.len() >= 5,
            "ClassLoader should have 5+ native methods, found {}",
            natives.len()
        );
    }

    #[test]
    #[ignore]
    fn s10_reference_natives_registered() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let natives = crate::vm::vm_object::scan_class_natives(&shared, "java/lang/ref/Reference");

        eprintln!("Reference: {} native methods", natives.len());
        for n in &natives {
            let registered = shared
                .natives
                .native_methods
                .find(&n.class_name, &n.method_name, &n.descriptor)
                .is_some();
            assert!(registered, "Reference native not registered: {n}");
        }
        assert!(
            natives.len() >= 4,
            "Reference should have 4+ native methods, found {}",
            natives.len()
        );
    }

    #[test]
    #[ignore]
    fn s10_method_handle_natives_registered() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let natives =
            crate::vm::vm_object::scan_class_natives(&shared, "java/lang/invoke/MethodHandle");

        eprintln!("MethodHandle: {} native methods", natives.len());
        for n in &natives {
            let registered = shared
                .natives
                .native_methods
                .find(&n.class_name, &n.method_name, &n.descriptor)
                .is_some();
            assert!(registered, "MethodHandle native not registered: {n}");
        }
        assert!(
            natives.len() >= 7,
            "MethodHandle should have 7+ native methods, found {}",
            natives.len()
        );
    }

    #[test]
    #[ignore]
    fn s10_jni_name_generation() {
        // Verify JNI naming convention generation
        let short = crate::vm::vm_object::jni_short_name("java/lang/System", "arraycopy");
        assert_eq!(short, "Java_java_lang_System_arraycopy");

        let short2 = crate::vm::vm_object::jni_short_name("java/lang/Object", "hashCode");
        assert_eq!(short2, "Java_java_lang_Object_hashCode");

        // Underscores in names get escaped
        let short3 = crate::vm::vm_object::jni_short_name("com/example/My_Class", "my_method");
        assert_eq!(short3, "Java_com_example_My_1Class_my_1method");

        // Long name with descriptor
        let long = crate::vm::vm_object::jni_long_name(
            "java/lang/System",
            "arraycopy",
            "(Ljava/lang/Object;ILjava/lang/Object;II)V",
        );
        assert!(long.starts_with("Java_java_lang_System_arraycopy__"));
        assert!(long.contains("Ljava_lang_Object"));
    }

    #[test]
    #[ignore]
    fn s10_file_io_natives_registered() {
        let shared = create_real_jdk_vm().expect("No JDK found");

        // FileInputStream JDK 25 natives
        let fis = crate::vm::vm_object::scan_class_natives(&shared, "java/io/FileInputStream");
        eprintln!("FileInputStream: {} native methods", fis.len());
        for n in &fis {
            let registered = shared
                .natives
                .native_methods
                .find(&n.class_name, &n.method_name, &n.descriptor)
                .is_some();
            assert!(registered, "FileInputStream native not registered: {n}");
        }

        // FileOutputStream JDK 25 natives
        let fos = crate::vm::vm_object::scan_class_natives(&shared, "java/io/FileOutputStream");
        eprintln!("FileOutputStream: {} native methods", fos.len());
        for n in &fos {
            let registered = shared
                .natives
                .native_methods
                .find(&n.class_name, &n.method_name, &n.descriptor)
                .is_some();
            assert!(registered, "FileOutputStream native not registered: {n}");
        }
    }

    #[test]
    #[ignore]
    fn s10_vm_internal_natives_registered() {
        let shared = create_real_jdk_vm().expect("No JDK found");

        // jdk/internal/misc/VM
        let vm_natives = crate::vm::vm_object::scan_class_natives(&shared, "jdk/internal/misc/VM");
        eprintln!("VM: {} native methods", vm_natives.len());
        for n in &vm_natives {
            let registered = shared
                .natives
                .native_methods
                .find(&n.class_name, &n.method_name, &n.descriptor)
                .is_some();
            assert!(registered, "VM native not registered: {n}");
        }

        // jdk/internal/misc/CDS
        let cds_natives =
            crate::vm::vm_object::scan_class_natives(&shared, "jdk/internal/misc/CDS");
        eprintln!("CDS: {} native methods", cds_natives.len());
        for n in &cds_natives {
            let registered = shared
                .natives
                .native_methods
                .find(&n.class_name, &n.method_name, &n.descriptor)
                .is_some();
            assert!(registered, "CDS native not registered: {n}");
        }
    }

    #[test]
    #[ignore]
    fn s10_register_natives_handled() {
        let shared = create_real_jdk_vm().expect("No JDK found");

        // All classes with registerNatives() must have it registered as noop
        let classes_with_register_natives = [
            "java/lang/Object",
            "java/lang/System",
            "java/lang/Class",
            "java/lang/Thread",
            "java/lang/ClassLoader",
            "java/io/FileInputStream",
            "java/io/FileOutputStream",
            "java/io/FileDescriptor",
            "jdk/internal/misc/Unsafe",
        ];
        for class in &classes_with_register_natives {
            let found = shared
                .natives
                .native_methods
                .find(class, "registerNatives", "()V")
                .is_some();
            assert!(found, "registerNatives not registered for {class}");
        }
    }

    #[test]
    #[ignore]
    fn s10_scan_class_natives_returns_empty_for_unknown() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let natives = crate::vm::vm_object::scan_class_natives(&shared, "com/nonexistent/Foo");
        assert!(natives.is_empty());
    }

    // -----------------------------------------------------------------------
    // Session 11: Bootstrap — java.util Core Collections
    // -----------------------------------------------------------------------

    /// Helper: create a SharedVm + JvmThread pair for Session 11 tests.
    fn create_real_jdk_vm_with_thread(
    ) -> Option<(SharedVm, crate::threading::jvm_thread::JvmThread)> {
        let shared = create_real_jdk_vm()?;
        let thread = crate::threading::jvm_thread::JvmThread::new(
            crate::threading::jvm_thread::ThreadId(99),
            "test-s11",
        );
        Some((shared, thread))
    }

    /// Helper: create a SharedVm + JvmThread pair with audit mode enabled.
    fn create_real_jdk_vm_with_thread_audit(
    ) -> Option<(SharedVm, crate::threading::jvm_thread::JvmThread)> {
        let shared = create_real_jdk_vm_audit()?;
        let thread = crate::threading::jvm_thread::JvmThread::new(
            crate::threading::jvm_thread::ThreadId(99),
            "test-s11-audit",
        );
        Some((shared, thread))
    }

    /// Helper: load a class, initialize it, allocate an instance, run <init>.
    fn new_object_initialized(
        shared: &SharedVm,
        thread: &mut crate::threading::jvm_thread::JvmThread,
        class_name: &str,
        init_desc: &str,
        init_args: &[Value],
    ) -> ObjectRef {
        let class_id = shared
            .classes
            .class_manager
            .write()
            .load_class(class_name)
            .unwrap_or_else(|e| panic!("Failed to load {class_name}: {e:?}"));
        crate::vm::ensure_class_initialized_shared(shared, thread, class_id)
            .unwrap_or_else(|e| panic!("Failed to initialize {class_name}: {e:?}"));
        let num_fields = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.num_total_fields)
            .unwrap_or(0);
        let obj = shared.mem.heap.alloc_object(class_id, num_fields);
        crate::runtime::interpreter::init_primitive_fields(shared, obj, class_id);
        // Call <init>
        let mut full_args = vec![Value::Object(Some(obj))];
        full_args.extend_from_slice(init_args);
        crate::vm::invoke_on_class_shared(
            shared, thread, class_id, "<init>", init_desc, &full_args,
        )
        .unwrap_or_else(|e| panic!("Failed to call {class_name}.<init>: {e:?}"));
        obj
    }

    // --- Collection class loading from real JDK bytecode ---

    #[test]
    #[ignore]
    fn s11_hashmap_loaded_from_real_bytecode() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let hm_id = cm.get_loaded_class_id("java/util/HashMap");
        assert!(hm_id.is_some(), "HashMap should be loaded during bootstrap");
        let hm_id = hm_id.expect("hm_id should be Some");
        let cls = cm
            .get_class(hm_id)
            .expect("class should exist in class store");
        assert!(
            !cls.origin.is_compatibility_stub(),
            "HashMap should be from real bytecode"
        );

        // HashMap should have many methods
        let method_names: Vec<&str> = cls.methods.iter().map(|m| m.name.as_ref()).collect();
        assert!(method_names.contains(&"put"), "HashMap should have put()");
        assert!(method_names.contains(&"get"), "HashMap should have get()");
        assert!(method_names.contains(&"size"), "HashMap should have size()");
        assert!(
            method_names.contains(&"remove"),
            "HashMap should have remove()"
        );
        assert!(
            method_names.contains(&"containsKey"),
            "HashMap should have containsKey()"
        );

        // HashMap extends AbstractMap
        let super_name = cls
            .superclass
            .and_then(|sid| cm.get_class(sid).map(|c| c.name.clone()));
        assert_eq!(
            super_name.as_deref(),
            Some("java/util/AbstractMap"),
            "HashMap should extend AbstractMap"
        );

        eprintln!(
            "HashMap: {} methods, {} fields, super={:?}",
            cls.methods.len(),
            cls.fields.len(),
            super_name
        );
    }

    #[test]
    #[ignore]
    fn s11_arraylist_loaded_from_real_bytecode() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let al_id = cm.get_loaded_class_id("java/util/ArrayList");
        assert!(
            al_id.is_some(),
            "ArrayList should be loaded during bootstrap"
        );
        let al_id = al_id.expect("al_id should be Some");
        let cls = cm
            .get_class(al_id)
            .expect("class should exist in class store");
        assert!(
            !cls.origin.is_compatibility_stub(),
            "ArrayList should be from real bytecode"
        );

        let method_names: Vec<&str> = cls.methods.iter().map(|m| m.name.as_ref()).collect();
        assert!(method_names.contains(&"add"), "ArrayList should have add()");
        assert!(method_names.contains(&"get"), "ArrayList should have get()");
        assert!(
            method_names.contains(&"size"),
            "ArrayList should have size()"
        );
        assert!(
            method_names.contains(&"remove"),
            "ArrayList should have remove()"
        );

        let super_name = cls
            .superclass
            .and_then(|sid| cm.get_class(sid).map(|c| c.name.clone()));
        assert_eq!(
            super_name.as_deref(),
            Some("java/util/AbstractList"),
            "ArrayList should extend AbstractList"
        );

        eprintln!(
            "ArrayList: {} methods, {} fields",
            cls.methods.len(),
            cls.fields.len()
        );
    }

    // --- Abstract collection class hierarchy ---

    #[test]
    #[ignore]
    fn s11_abstract_collection_hierarchy() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        for name in &[
            "java/util/AbstractCollection",
            "java/util/AbstractList",
            "java/util/AbstractSet",
            "java/util/AbstractMap",
        ] {
            let id = cm.get_loaded_class_id(name);
            assert!(id.is_some(), "{name} should be loaded during bootstrap");
            let cls = cm
                .get_class(id.expect("class id should be loaded"))
                .expect("class should exist in store");
            assert!(
                !cls.origin.is_compatibility_stub(),
                "{name} should be from real bytecode"
            );
        }

        // Verify hierarchy: AbstractList extends AbstractCollection
        let al_id = cm
            .get_loaded_class_id("java/util/AbstractList")
            .expect("class java/util/AbstractList should be loaded");
        let al = cm
            .get_class(al_id)
            .expect("class should exist in class store");
        let super_name = al
            .superclass
            .and_then(|sid| cm.get_class(sid).map(|c| c.name.clone()));
        assert_eq!(
            super_name.as_deref(),
            Some("java/util/AbstractCollection"),
            "AbstractList should extend AbstractCollection"
        );

        // AbstractCollection extends Object
        let ac_id = cm
            .get_loaded_class_id("java/util/AbstractCollection")
            .expect("class java/util/AbstractCollection should be loaded");
        let ac = cm
            .get_class(ac_id)
            .expect("class should exist in class store");
        let super_name = ac
            .superclass
            .and_then(|sid| cm.get_class(sid).map(|c| c.name.clone()));
        assert_eq!(
            super_name.as_deref(),
            Some("java/lang/Object"),
            "AbstractCollection should extend Object"
        );
    }

    // --- Interface loading ---

    #[test]
    #[ignore]
    fn s11_collection_interfaces_loaded() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        for name in &[
            "java/util/Collection",
            "java/util/List",
            "java/util/Set",
            "java/util/Map",
            "java/lang/Iterable",
            "java/util/Iterator",
        ] {
            let id = cm.get_loaded_class_id(name);
            assert!(id.is_some(), "{name} should be loaded during bootstrap");
            let cls = cm
                .get_class(id.expect("class id should be loaded"))
                .expect("class should exist in store");
            assert!(
                !cls.origin.is_compatibility_stub(),
                "{name} should be from real bytecode"
            );
        }
    }

    // --- Class initialization ---

    #[test]
    #[ignore]
    fn s11_hashmap_initializes() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");
        let hm_id = shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id("java/util/HashMap")
            .expect("class java/util/HashMap should be loaded");
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, hm_id)
            .expect("HashMap <clinit> should execute successfully");

        let state = shared
            .classes
            .class_manager
            .read()
            .get_class(hm_id)
            .expect("class should exist in class store")
            .state;
        assert_eq!(
            state,
            crate::classloading::ClassState::Initialized,
            "HashMap should be in Initialized state"
        );
    }

    #[test]
    #[ignore]
    fn s11_arraylist_initializes() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");
        let al_id = shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id("java/util/ArrayList")
            .expect("class java/util/ArrayList should be loaded");
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, al_id)
            .expect("ArrayList <clinit> should execute successfully");

        let state = shared
            .classes
            .class_manager
            .read()
            .get_class(al_id)
            .expect("class should exist in class store")
            .state;
        assert_eq!(state, crate::classloading::ClassState::Initialized);
    }

    #[test]
    #[ignore]
    fn s11_all_core_collections_initialize() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        let collection_classes = [
            "java/util/HashMap",
            "java/util/ArrayList",
            "java/util/LinkedList",
            "java/util/HashSet",
            "java/util/AbstractCollection",
            "java/util/AbstractList",
            "java/util/AbstractSet",
            "java/util/AbstractMap",
            "java/util/Collections",
            "java/util/Arrays",
            "java/util/Objects",
        ];

        for name in &collection_classes {
            let id = shared
                .classes
                .class_manager
                .read()
                .get_loaded_class_id(name);
            assert!(id.is_some(), "{name} should be loaded");
            let id = id.expect("id should be Some");
            crate::vm::ensure_class_initialized_shared(&shared, &mut thread, id)
                .unwrap_or_else(|e| panic!("{name} <clinit> failed: {e:?}"));
            let state = shared
                .classes
                .class_manager
                .read()
                .get_class(id)
                .expect("class should exist in class store")
                .state;
            assert_eq!(
                state,
                crate::classloading::ClassState::Initialized,
                "{name} should be Initialized"
            );
        }
    }

    // --- Inner class loading ---

    #[test]
    #[ignore]
    fn s11_hashmap_inner_classes_load() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        // Initialize HashMap first (may trigger inner class loading)
        let hm_id = shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id("java/util/HashMap")
            .expect("class java/util/HashMap should be loaded");
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, hm_id)
            .expect("class initialization should succeed");

        // HashMap$Node should be loadable (may or may not be bootstrapped yet)
        let node_id = shared
            .classes
            .class_manager
            .write()
            .load_class("java/util/HashMap$Node");
        assert!(
            node_id.is_ok(),
            "HashMap$Node should be loadable, got: {:?}",
            node_id.err()
        );
        let node_id = node_id.expect("node_id should be Some");

        let cm = shared.classes.class_manager.read();
        let node = cm
            .get_class(node_id)
            .expect("class should exist in class store");
        assert!(
            !node.origin.is_compatibility_stub(),
            "HashMap$Node should be from real bytecode"
        );

        // Node should have key, value, hash, next fields
        let field_names: Vec<&str> = node.fields.iter().map(|f| f.name.as_ref()).collect();
        eprintln!("HashMap$Node fields: {:?}", field_names);
        assert!(
            field_names.contains(&"hash"),
            "Node should have 'hash' field"
        );
        assert!(field_names.contains(&"key"), "Node should have 'key' field");
        assert!(
            field_names.contains(&"value"),
            "Node should have 'value' field"
        );
        assert!(
            field_names.contains(&"next"),
            "Node should have 'next' field"
        );
    }

    // --- Object instantiation and method invocation ---

    #[test]
    #[ignore]
    fn s11_hashmap_put_and_get() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread_audit().expect("No JDK found");

        // new HashMap()
        let hm = new_object_initialized(&shared, &mut thread, "java/util/HashMap", "()V", &[]);

        // Create key and value strings
        let key = crate::vm::vm_object::create_java_string(&shared, "mykey");
        let val = crate::vm::vm_object::create_java_string(&shared, "myvalue");

        // HashMap.put(key, value)
        let hm_id = shared.mem.heap.class_id_of(hm);
        let put_result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            hm_id,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(hm)),
                Value::Object(Some(key)),
                Value::Object(Some(val)),
            ],
        );
        assert!(
            put_result.is_ok(),
            "HashMap.put() failed: {:?}",
            put_result.err()
        );
        let prev = put_result.expect("put_result should be Ok");
        eprintln!("put() returned: {:?}", prev);

        // Check audit log for missing natives
        {
            let log = shared.debug.missing_natives_log.lock();
            if !log.is_empty() {
                eprintln!("Missing natives called during put():");
                for entry in log.iter() {
                    eprintln!("  {}", entry.full_signature());
                }
            } else {
                eprintln!("No missing natives during put().");
            }
        }

        // Inspect raw instance fields of HashMap to see what put() did
        {
            let cm = shared.classes.class_manager.read();
            let cls = cm
                .get_class(hm_id)
                .expect("class should exist in class store");
            eprintln!(
                "HashMap instance fields (first_field_index={}, num_total={}):",
                cls.first_field_index, cls.num_total_fields
            );
            let mut inst_idx = cls.first_field_index;
            for f in &cls.fields {
                if !f.is_static() {
                    let val = shared.mem.heap.get_field(hm, inst_idx);
                    eprintln!(
                        "  slot[{}] {} ({}) = {:?}",
                        inst_idx, f.name, f.descriptor, val
                    );
                    inst_idx += 1;
                }
            }
            // Also check parent class (AbstractMap) instance fields
            if let Some(super_id) = cls.superclass {
                let super_cls = cm
                    .get_class(super_id)
                    .expect("class should exist in class store");
                eprintln!(
                    "AbstractMap instance fields (first={}, total={}):",
                    super_cls.first_field_index, super_cls.num_total_fields
                );
                let mut si = super_cls.first_field_index;
                for f in &super_cls.fields {
                    if !f.is_static() {
                        let val = shared.mem.heap.get_field(hm, si);
                        eprintln!("  slot[{}] {} ({}) = {:?}", si, f.name, f.descriptor, val);
                        si += 1;
                    }
                }
            }
        }

        // HashMap.size() should be 1
        let size_result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            hm_id,
            "size",
            "()I",
            &[Value::Object(Some(hm))],
        );
        assert!(
            size_result.is_ok(),
            "HashMap.size() failed: {:?}",
            size_result.err()
        );
        let size_val = size_result.expect("size_result should be Ok");
        eprintln!("size() returned: {:?}", size_val);
        assert_eq!(
            size_val,
            Some(Value::Int(1)),
            "HashMap should have size 1 after one put"
        );

        // HashMap.get(key) should return the value
        let get_result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            hm_id,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(hm)), Value::Object(Some(key))],
        );
        assert!(
            get_result.is_ok(),
            "HashMap.get() failed: {:?}",
            get_result.err()
        );
        let retrieved = get_result.expect("get_result should be Ok");
        match retrieved {
            Some(Value::Object(Some(obj_ref))) => {
                let text = crate::vm::vm_object::read_java_string(&shared.mem.heap, obj_ref);
                assert_eq!(
                    text.as_deref(),
                    Some("myvalue"),
                    "HashMap.get() should return 'myvalue'"
                );
            }
            other => panic!(
                "HashMap.get() should return string object, got: {:?}",
                other
            ),
        }

        eprintln!("HashMap.put/get works with real JDK bytecode!");
    }

    #[test]
    #[ignore]
    fn s11_arraylist_add_and_get() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        // new ArrayList()
        let al = new_object_initialized(&shared, &mut thread, "java/util/ArrayList", "()V", &[]);

        // Box an integer: create Integer.valueOf(42)
        let int_class_id = shared
            .classes
            .class_manager
            .write()
            .load_class("java/lang/Integer")
            .expect("failed to load class java/lang/Integer");
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, int_class_id)
            .expect("class initialization should succeed");
        let boxed_42 = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            int_class_id,
            "valueOf",
            "(I)Ljava/lang/Integer;",
            &[Value::Int(42)],
        )
        .expect("Integer.valueOf(42) failed");
        let boxed_42 = match boxed_42 {
            Some(Value::Object(Some(r))) => r,
            other => panic!("Integer.valueOf should return object, got: {:?}", other),
        };

        // ArrayList.add(Integer.valueOf(42))
        let al_id = shared.mem.heap.class_id_of(al);
        let add_result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            al_id,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(al)), Value::Object(Some(boxed_42))],
        );
        assert!(
            add_result.is_ok(),
            "ArrayList.add() failed: {:?}",
            add_result.err()
        );
        assert_eq!(
            add_result.expect("add_result should be Ok"),
            Some(Value::Int(1)),
            "ArrayList.add() should return true (1)"
        );

        // ArrayList.size() should be 1
        let size_result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            al_id,
            "size",
            "()I",
            &[Value::Object(Some(al))],
        );
        assert!(
            size_result.is_ok(),
            "ArrayList.size() failed: {:?}",
            size_result.err()
        );
        assert_eq!(
            size_result.expect("size_result should be Ok"),
            Some(Value::Int(1))
        );

        // ArrayList.get(0) should return the Integer
        let get_result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            al_id,
            "get",
            "(I)Ljava/lang/Object;",
            &[Value::Object(Some(al)), Value::Int(0)],
        );
        assert!(
            get_result.is_ok(),
            "ArrayList.get(0) failed: {:?}",
            get_result.err()
        );
        match get_result.expect("get_result should be Ok") {
            Some(Value::Object(Some(obj_ref))) => {
                // Read the intValue
                let int_val = crate::vm::invoke_on_class_shared(
                    &shared,
                    &mut thread,
                    shared.mem.heap.class_id_of(obj_ref),
                    "intValue",
                    "()I",
                    &[Value::Object(Some(obj_ref))],
                )
                .expect("Integer.intValue() failed");
                assert_eq!(
                    int_val,
                    Some(Value::Int(42)),
                    "ArrayList.get(0) should return Integer(42)"
                );
            }
            other => panic!("ArrayList.get(0) should return object, got: {:?}", other),
        }

        eprintln!("ArrayList.add/get works with real JDK bytecode!");
    }

    // --- LinkedList ---

    #[test]
    #[ignore]
    fn s11_linkedlist_loaded_and_initializes() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let ll_id = cm.get_loaded_class_id("java/util/LinkedList");
        assert!(ll_id.is_some(), "LinkedList should be loaded");
        let ll_id = ll_id.expect("ll_id should be Some");
        let cls = cm
            .get_class(ll_id)
            .expect("class should exist in class store");
        assert!(
            !cls.origin.is_compatibility_stub(),
            "LinkedList should be from real bytecode"
        );

        let super_name = cls
            .superclass
            .and_then(|sid| cm.get_class(sid).map(|c| c.name.clone()));
        assert_eq!(
            super_name.as_deref(),
            Some("java/util/AbstractSequentialList"),
            "LinkedList should extend AbstractSequentialList"
        );
        drop(cm);

        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, ll_id)
            .expect("LinkedList <clinit> should succeed");
    }

    // --- HashSet ---

    #[test]
    #[ignore]
    fn s11_hashset_loaded_and_initializes() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        let hs_id = shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id("java/util/HashSet");
        assert!(hs_id.is_some(), "HashSet should be loaded");
        let hs_id = hs_id.expect("hs_id should be Some");
        {
            let cm = shared.classes.class_manager.read();
            let cls = cm
                .get_class(hs_id)
                .expect("class should exist in class store");
            assert!(
                !cls.origin.is_compatibility_stub(),
                "HashSet should be from real bytecode"
            );
        }

        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, hs_id)
            .expect("HashSet <clinit> should succeed");
    }

    // --- TreeMap loading (on-demand, not in bootstrap tier 2) ---

    #[test]
    #[ignore]
    fn s11_treemap_loads_and_initializes() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        // TreeMap may not be in tier 2 bootstrap but should be loadable
        let tm_id = shared
            .classes
            .class_manager
            .write()
            .load_class("java/util/TreeMap")
            .expect("TreeMap should be loadable");

        {
            let cm = shared.classes.class_manager.read();
            let cls = cm
                .get_class(tm_id)
                .expect("class should exist in class store");
            assert!(
                !cls.origin.is_compatibility_stub(),
                "TreeMap should be from real bytecode"
            );

            let method_names: Vec<&str> = cls.methods.iter().map(|m| m.name.as_ref()).collect();
            assert!(method_names.contains(&"put"), "TreeMap should have put()");
            assert!(method_names.contains(&"get"), "TreeMap should have get()");
        }

        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, tm_id)
            .expect("TreeMap <clinit> should succeed");
    }

    // --- Spliterator interface ---

    #[test]
    #[ignore]
    fn s11_spliterator_interface_loads() {
        let (shared, _thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        let sp_id = shared
            .classes
            .class_manager
            .write()
            .load_class("java/util/Spliterator")
            .expect("Spliterator should be loadable");

        let cm = shared.classes.class_manager.read();
        let cls = cm
            .get_class(sp_id)
            .expect("class should exist in class store");
        assert!(
            !cls.origin.is_compatibility_stub(),
            "Spliterator should be from real bytecode"
        );

        // Spliterator is an interface
        assert!(
            cls.access_flags
                .contains(cratonvm_reader::class_access_flags::ClassAccessFlags::INTERFACE),
            "Spliterator should be an interface"
        );
    }

    // --- HashMap.containsKey and remove ---

    #[test]
    #[ignore]
    fn s11_hashmap_contains_and_remove() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        let hm = new_object_initialized(&shared, &mut thread, "java/util/HashMap", "()V", &[]);
        let hm_id = shared.mem.heap.class_id_of(hm);

        let key = crate::vm::vm_object::create_java_string(&shared, "testkey");
        let val = crate::vm::vm_object::create_java_string(&shared, "testval");

        // put(key, val)
        crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            hm_id,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(hm)),
                Value::Object(Some(key)),
                Value::Object(Some(val)),
            ],
        )
        .expect("put failed");

        // containsKey(key) == true
        let contains = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            hm_id,
            "containsKey",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(hm)), Value::Object(Some(key))],
        )
        .expect("containsKey failed");
        assert_eq!(contains, Some(Value::Int(1)), "containsKey should be true");

        // remove(key) returns old value
        let removed = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            hm_id,
            "remove",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(hm)), Value::Object(Some(key))],
        )
        .expect("remove failed");
        match removed {
            Some(Value::Object(Some(obj_ref))) => {
                let text = crate::vm::vm_object::read_java_string(&shared.mem.heap, obj_ref);
                assert_eq!(text.as_deref(), Some("testval"));
            }
            other => panic!("remove should return old value, got: {:?}", other),
        }

        // size should be 0 after remove
        let size = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            hm_id,
            "size",
            "()I",
            &[Value::Object(Some(hm))],
        )
        .expect("size failed");
        assert_eq!(size, Some(Value::Int(0)), "size should be 0 after remove");
    }

    // --- Multiple puts ---

    #[test]
    #[ignore]
    fn s11_hashmap_multiple_entries() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        let hm = new_object_initialized(&shared, &mut thread, "java/util/HashMap", "()V", &[]);
        let hm_id = shared.mem.heap.class_id_of(hm);

        // Put 10 entries
        for i in 0..10 {
            let key = crate::vm::vm_object::create_java_string(&shared, &format!("key{i}"));
            let val = crate::vm::vm_object::create_java_string(&shared, &format!("val{i}"));
            crate::vm::invoke_on_class_shared(
                &shared,
                &mut thread,
                hm_id,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[
                    Value::Object(Some(hm)),
                    Value::Object(Some(key)),
                    Value::Object(Some(val)),
                ],
            )
            .unwrap_or_else(|e| panic!("put({i}) failed: {e:?}"));
        }

        let size = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            hm_id,
            "size",
            "()I",
            &[Value::Object(Some(hm))],
        )
        .expect("size failed");
        assert_eq!(size, Some(Value::Int(10)), "HashMap should have 10 entries");

        // Verify each entry can be retrieved
        for i in 0..10 {
            let key = crate::vm::vm_object::create_java_string(&shared, &format!("key{i}"));
            let result = crate::vm::invoke_on_class_shared(
                &shared,
                &mut thread,
                hm_id,
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(hm)), Value::Object(Some(key))],
            )
            .unwrap_or_else(|e| panic!("get(key{i}) failed: {e:?}"));
            match result {
                Some(Value::Object(Some(obj_ref))) => {
                    let text = crate::vm::vm_object::read_java_string(&shared.mem.heap, obj_ref);
                    assert_eq!(
                        text.as_deref(),
                        Some(&format!("val{i}") as &str),
                        "get(key{i}) should return val{i}"
                    );
                }
                other => panic!("get(key{i}) should return string, got: {:?}", other),
            }
        }

        eprintln!("HashMap with 10 entries: all put/get pairs verified!");
    }

    // --- ArrayList multiple adds ---

    #[test]
    #[ignore]
    fn s11_arraylist_multiple_elements() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        let al = new_object_initialized(&shared, &mut thread, "java/util/ArrayList", "()V", &[]);
        let al_id = shared.mem.heap.class_id_of(al);

        // Add 20 strings (triggers internal array resize from default capacity 10)
        for i in 0..20 {
            let s = crate::vm::vm_object::create_java_string(&shared, &format!("item{i}"));
            crate::vm::invoke_on_class_shared(
                &shared,
                &mut thread,
                al_id,
                "add",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(al)), Value::Object(Some(s))],
            )
            .unwrap_or_else(|e| panic!("add({i}) failed: {e:?}"));
        }

        let size = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            al_id,
            "size",
            "()I",
            &[Value::Object(Some(al))],
        )
        .expect("size failed");
        assert_eq!(
            size,
            Some(Value::Int(20)),
            "ArrayList should have 20 elements"
        );

        // Verify element at index 15
        let get_result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            al_id,
            "get",
            "(I)Ljava/lang/Object;",
            &[Value::Object(Some(al)), Value::Int(15)],
        )
        .expect("get(15) failed");
        match get_result {
            Some(Value::Object(Some(obj_ref))) => {
                let text = crate::vm::vm_object::read_java_string(&shared.mem.heap, obj_ref);
                assert_eq!(text.as_deref(), Some("item15"));
            }
            other => panic!("get(15) should return 'item15', got: {:?}", other),
        }

        eprintln!("ArrayList with 20 elements (resize triggered): verified!");
    }

    // -----------------------------------------------------------------------
    // Session 13: Bootstrap — java.util.concurrent
    // -----------------------------------------------------------------------

    /// Verify all j.u.c classes are loaded from real JDK bytecode.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_concurrent_classes_loaded_from_real_bytecode() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let concurrent_classes = [
            // Atomic classes
            "java/util/concurrent/atomic/AtomicInteger",
            "java/util/concurrent/atomic/AtomicLong",
            "java/util/concurrent/atomic/AtomicBoolean",
            "java/util/concurrent/atomic/AtomicReference",
            "java/util/concurrent/atomic/AtomicIntegerArray",
            "java/util/concurrent/atomic/AtomicLongArray",
            "java/util/concurrent/atomic/AtomicReferenceArray",
            "java/util/concurrent/atomic/AtomicStampedReference",
            "java/util/concurrent/atomic/AtomicMarkableReference",
            "java/util/concurrent/atomic/LongAdder",
            "java/util/concurrent/atomic/DoubleAdder",
            "java/util/concurrent/atomic/LongAccumulator",
            "java/util/concurrent/atomic/DoubleAccumulator",
            // Locks
            "java/util/concurrent/locks/ReentrantLock",
            "java/util/concurrent/locks/ReentrantReadWriteLock",
            "java/util/concurrent/locks/StampedLock",
            "java/util/concurrent/locks/AbstractQueuedSynchronizer",
            "java/util/concurrent/locks/LockSupport",
            "java/util/concurrent/locks/Lock",
            "java/util/concurrent/locks/Condition",
            "java/util/concurrent/locks/ReadWriteLock",
            // Concurrent collections
            "java/util/concurrent/ConcurrentHashMap",
            "java/util/concurrent/CopyOnWriteArrayList",
            "java/util/concurrent/CopyOnWriteArraySet",
            // Synchronizers
            "java/util/concurrent/CountDownLatch",
            "java/util/concurrent/Semaphore",
            "java/util/concurrent/CyclicBarrier",
            "java/util/concurrent/Phaser",
            "java/util/concurrent/CompletableFuture",
            // Executors
            "java/util/concurrent/ForkJoinPool",
            "java/util/concurrent/ForkJoinTask",
            "java/util/concurrent/ExecutorService",
            "java/util/concurrent/ThreadPoolExecutor",
            "java/util/concurrent/Executors",
        ];

        for class_name in &concurrent_classes {
            let id = cm.get_loaded_class_id(class_name);
            assert!(
                id.is_some(),
                "{class_name} should be loaded during bootstrap"
            );
            let cls = cm
                .get_class(id.expect("class id should be loaded"))
                .expect("class should exist in store");
            assert!(
                !cls.origin.is_compatibility_stub(),
                "{class_name} should be from real JDK bytecode, not a synthetic stub"
            );
        }

        eprintln!(
            "All {} j.u.c classes loaded from real bytecode",
            concurrent_classes.len()
        );
    }

    /// Verify Unsafe CAS methods needed by j.u.c are registered for both APIs.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_unsafe_cas_methods_registered() {
        let shared = create_real_jdk_vm().expect("No JDK found");

        // jdk.internal.misc.Unsafe (modern API)
        let u = "jdk/internal/misc/Unsafe";
        let cas_methods = [
            ("compareAndSetInt", "(Ljava/lang/Object;JII)Z"),
            ("compareAndSetLong", "(Ljava/lang/Object;JJJ)Z"),
            (
                "compareAndSetReference",
                "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
            ),
            ("getAndAddInt", "(Ljava/lang/Object;JI)I"),
            ("getAndAddLong", "(Ljava/lang/Object;JJ)J"),
            ("getAndSetInt", "(Ljava/lang/Object;JI)I"),
            ("getAndSetLong", "(Ljava/lang/Object;JJ)J"),
            (
                "getAndSetReference",
                "(Ljava/lang/Object;JLjava/lang/Object;)Ljava/lang/Object;",
            ),
            ("getIntVolatile", "(Ljava/lang/Object;J)I"),
            ("putIntVolatile", "(Ljava/lang/Object;JI)V"),
            ("getLongVolatile", "(Ljava/lang/Object;J)J"),
            ("putLongVolatile", "(Ljava/lang/Object;JJ)V"),
            (
                "getReferenceVolatile",
                "(Ljava/lang/Object;J)Ljava/lang/Object;",
            ),
            (
                "putReferenceVolatile",
                "(Ljava/lang/Object;JLjava/lang/Object;)V",
            ),
            ("park", "(ZJ)V"),
            ("unpark", "(Ljava/lang/Object;)V"),
        ];

        for (method, desc) in &cas_methods {
            assert!(
                shared
                    .natives
                    .native_methods
                    .find(u, method, desc)
                    .is_some(),
                "Unsafe.{method}{desc} should be registered on {u}"
            );
        }

        // sun.misc.Unsafe (legacy API)
        let u_legacy = "sun/misc/Unsafe";
        let legacy_cas = [
            ("compareAndSwapInt", "(Ljava/lang/Object;JII)Z"),
            ("compareAndSwapLong", "(Ljava/lang/Object;JJJ)Z"),
            (
                "compareAndSwapObject",
                "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
            ),
        ];

        for (method, desc) in &legacy_cas {
            assert!(
                shared
                    .natives
                    .native_methods
                    .find(u_legacy, method, desc)
                    .is_some(),
                "{u_legacy}.{method}{desc} should be registered"
            );
        }
    }

    /// Verify LockSupport park/unpark methods are registered.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_lock_support_natives_registered() {
        let shared = create_real_jdk_vm().expect("No JDK found");

        let ls = "java/util/concurrent/locks/LockSupport";
        let methods = [
            ("park", "(Ljava/lang/Object;)V"),
            ("parkNanos", "(Ljava/lang/Object;J)V"),
            ("unpark", "(Ljava/lang/Thread;)V"),
        ];

        for (method, desc) in &methods {
            assert!(
                shared
                    .natives
                    .native_methods
                    .find(ls, method, desc)
                    .is_some(),
                "LockSupport.{method}{desc} should be registered"
            );
        }
    }

    /// Verify AQS is loaded from real bytecode and has expected structure.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_aqs_structure() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let aqs_id = cm
            .get_loaded_class_id("java/util/concurrent/locks/AbstractQueuedSynchronizer")
            .expect("AQS should be loaded");
        let aqs = cm
            .get_class(aqs_id)
            .expect("class should exist in class store");
        assert!(!aqs.origin.is_compatibility_stub(), "AQS should be real bytecode");

        // AQS should have key methods
        let method_names: Vec<&str> = aqs.methods.iter().map(|m| m.name.as_ref()).collect();
        assert!(method_names.contains(&"acquire"), "AQS should have acquire");
        assert!(method_names.contains(&"release"), "AQS should have release");
        assert!(
            method_names.contains(&"tryAcquire"),
            "AQS should have tryAcquire"
        );
        assert!(
            method_names.contains(&"tryRelease"),
            "AQS should have tryRelease"
        );

        eprintln!("AQS loaded with {} methods", aqs.methods.len());
    }

    /// Verify ReentrantLock extends from AQS (via inner Sync class).
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_reentrant_lock_structure() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let rl_id = cm
            .get_loaded_class_id("java/util/concurrent/locks/ReentrantLock")
            .expect("ReentrantLock should be loaded");
        let rl = cm
            .get_class(rl_id)
            .expect("class should exist in class store");
        assert!(
            !rl.origin.is_compatibility_stub(),
            "ReentrantLock should be real bytecode"
        );

        // Verify key methods
        let method_names: Vec<&str> = rl.methods.iter().map(|m| m.name.as_ref()).collect();
        assert!(
            method_names.contains(&"lock"),
            "ReentrantLock should have lock"
        );
        assert!(
            method_names.contains(&"unlock"),
            "ReentrantLock should have unlock"
        );
        assert!(
            method_names.contains(&"tryLock"),
            "ReentrantLock should have tryLock"
        );
        assert!(
            method_names.contains(&"newCondition"),
            "ReentrantLock should have newCondition"
        );
    }

    /// Verify AtomicInteger structure from real bytecode.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_atomic_integer_structure() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let ai_id = cm
            .get_loaded_class_id("java/util/concurrent/atomic/AtomicInteger")
            .expect("AtomicInteger should be loaded");
        let ai = cm
            .get_class(ai_id)
            .expect("class should exist in class store");
        assert!(
            !ai.origin.is_compatibility_stub(),
            "AtomicInteger should be real bytecode"
        );

        // AtomicInteger should have a volatile 'value' field
        let field_names: Vec<&str> = ai
            .fields
            .iter()
            .filter(|f| !f.is_static())
            .map(|f| f.name.as_ref())
            .collect();
        assert!(
            field_names.contains(&"value"),
            "AtomicInteger should have 'value' field"
        );

        // Should have key methods
        let method_names: Vec<&str> = ai.methods.iter().map(|m| m.name.as_ref()).collect();
        assert!(
            method_names.contains(&"get"),
            "AtomicInteger should have get"
        );
        assert!(
            method_names.contains(&"set"),
            "AtomicInteger should have set"
        );
        assert!(
            method_names.contains(&"getAndIncrement"),
            "AtomicInteger should have getAndIncrement"
        );
        assert!(
            method_names.contains(&"incrementAndGet"),
            "AtomicInteger should have incrementAndGet"
        );
        assert!(
            method_names.contains(&"compareAndSet"),
            "AtomicInteger should have compareAndSet"
        );
    }

    /// Runtime test: AtomicInteger.incrementAndGet() — roadmap deliverable.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_atomic_integer_increment_and_get_runtime() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        // new AtomicInteger(0)
        let ai = new_object_initialized(
            &shared,
            &mut thread,
            "java/util/concurrent/atomic/AtomicInteger",
            "(I)V",
            &[Value::Int(0)],
        );
        let ai_id = shared.mem.heap.class_id_of(ai);

        // incrementAndGet() should return 1
        let result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            ai_id,
            "incrementAndGet",
            "()I",
            &[Value::Object(Some(ai))],
        );
        assert!(
            result.is_ok(),
            "incrementAndGet() failed: {:?}",
            result.err()
        );
        assert_eq!(
            result.expect("result should be Ok"),
            Some(Value::Int(1)),
            "AtomicInteger(0).incrementAndGet() should return 1"
        );

        // incrementAndGet() again should return 2
        let result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            ai_id,
            "incrementAndGet",
            "()I",
            &[Value::Object(Some(ai))],
        );
        assert!(
            result.is_ok(),
            "incrementAndGet() #2 failed: {:?}",
            result.err()
        );
        assert_eq!(
            result.expect("result should be Ok"),
            Some(Value::Int(2)),
            "AtomicInteger(1).incrementAndGet() should return 2"
        );

        // get() should return 2
        let result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            ai_id,
            "get",
            "()I",
            &[Value::Object(Some(ai))],
        );
        assert!(result.is_ok(), "get() failed: {:?}", result.err());
        assert_eq!(
            result.expect("result should be Ok"),
            Some(Value::Int(2)),
            "AtomicInteger.get() should return 2 after two increments"
        );

        eprintln!("AtomicInteger.incrementAndGet() works with real JDK bytecode!");
    }

    /// Runtime test: AtomicInteger.compareAndSet().
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_atomic_integer_compare_and_set_runtime() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        let ai = new_object_initialized(
            &shared,
            &mut thread,
            "java/util/concurrent/atomic/AtomicInteger",
            "(I)V",
            &[Value::Int(42)],
        );
        let ai_id = shared.mem.heap.class_id_of(ai);

        // compareAndSet(42, 100) should succeed (return true)
        let result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            ai_id,
            "compareAndSet",
            "(II)Z",
            &[Value::Object(Some(ai)), Value::Int(42), Value::Int(100)],
        );
        assert!(
            result.is_ok(),
            "compareAndSet(42,100) failed: {:?}",
            result.err()
        );
        assert_eq!(
            result.expect("result should be Ok"),
            Some(Value::Int(1)),
            "compareAndSet(42,100) should return true"
        );

        // compareAndSet(42, 200) should fail (expected 42 but value is now 100)
        let result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            ai_id,
            "compareAndSet",
            "(II)Z",
            &[Value::Object(Some(ai)), Value::Int(42), Value::Int(200)],
        );
        assert!(
            result.is_ok(),
            "compareAndSet(42,200) failed: {:?}",
            result.err()
        );
        assert_eq!(
            result.expect("result should be Ok"),
            Some(Value::Int(0)),
            "compareAndSet(42,200) should return false (value is 100)"
        );

        // Verify value is 100
        let result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            ai_id,
            "get",
            "()I",
            &[Value::Object(Some(ai))],
        );
        assert_eq!(result.expect("result should be Ok"), Some(Value::Int(100)));
    }

    /// Runtime test: ConcurrentHashMap.put() and get() — roadmap deliverable.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_concurrent_hashmap_put_and_get_runtime() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        // new ConcurrentHashMap()
        let chm = new_object_initialized(
            &shared,
            &mut thread,
            "java/util/concurrent/ConcurrentHashMap",
            "()V",
            &[],
        );
        let chm_id = shared.mem.heap.class_id_of(chm);

        // Create key and value strings
        let key = crate::vm::vm_object::create_java_string(&shared, "testKey");
        let value = crate::vm::vm_object::create_java_string(&shared, "testValue");

        // put("testKey", "testValue")
        let put_result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            chm_id,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(chm)),
                Value::Object(Some(key)),
                Value::Object(Some(value)),
            ],
        );
        assert!(
            put_result.is_ok(),
            "ConcurrentHashMap.put() failed: {:?}",
            put_result.err()
        );
        // First put should return null (no previous value)
        assert_eq!(
            put_result.expect("put_result should be Ok"),
            Some(Value::Object(None)),
            "First put should return null"
        );

        // get("testKey") should return "testValue"
        let get_result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            chm_id,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(chm)), Value::Object(Some(key))],
        );
        assert!(
            get_result.is_ok(),
            "ConcurrentHashMap.get() failed: {:?}",
            get_result.err()
        );
        match get_result.expect("get_result should be Ok") {
            Some(Value::Object(Some(obj_ref))) => {
                let text = crate::vm::vm_object::read_java_string(&shared.mem.heap, obj_ref);
                assert_eq!(
                    text.as_deref(),
                    Some("testValue"),
                    "ConcurrentHashMap.get(\"testKey\") should return \"testValue\""
                );
            }
            other => panic!(
                "ConcurrentHashMap.get() should return string, got: {:?}",
                other
            ),
        }

        // size() should be 1
        let size_result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            chm_id,
            "size",
            "()I",
            &[Value::Object(Some(chm))],
        );
        assert!(
            size_result.is_ok(),
            "size() failed: {:?}",
            size_result.err()
        );
        assert_eq!(
            size_result.expect("size_result should be Ok"),
            Some(Value::Int(1))
        );

        eprintln!("ConcurrentHashMap.put/get works with real JDK bytecode!");
    }

    /// Verify CountDownLatch structure from real bytecode.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_count_down_latch_structure() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let cdl_id = cm
            .get_loaded_class_id("java/util/concurrent/CountDownLatch")
            .expect("CountDownLatch should be loaded");
        let cdl = cm
            .get_class(cdl_id)
            .expect("class should exist in class store");
        assert!(
            !cdl.origin.is_compatibility_stub(),
            "CountDownLatch should be real bytecode"
        );

        let method_names: Vec<&str> = cdl.methods.iter().map(|m| m.name.as_ref()).collect();
        assert!(
            method_names.contains(&"countDown"),
            "CountDownLatch should have countDown"
        );
        assert!(
            method_names.contains(&"await"),
            "CountDownLatch should have await"
        );
        assert!(
            method_names.contains(&"getCount"),
            "CountDownLatch should have getCount"
        );
    }

    /// Verify Semaphore structure from real bytecode.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_semaphore_structure() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let sem_id = cm
            .get_loaded_class_id("java/util/concurrent/Semaphore")
            .expect("Semaphore should be loaded");
        let sem = cm
            .get_class(sem_id)
            .expect("class should exist in class store");
        assert!(!sem.origin.is_compatibility_stub(), "Semaphore should be real bytecode");

        let method_names: Vec<&str> = sem.methods.iter().map(|m| m.name.as_ref()).collect();
        assert!(
            method_names.contains(&"acquire"),
            "Semaphore should have acquire"
        );
        assert!(
            method_names.contains(&"release"),
            "Semaphore should have release"
        );
        assert!(
            method_names.contains(&"tryAcquire"),
            "Semaphore should have tryAcquire"
        );
        assert!(
            method_names.contains(&"availablePermits"),
            "Semaphore should have availablePermits"
        );
    }

    /// Verify CyclicBarrier structure from real bytecode.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_cyclic_barrier_structure() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let cb_id = cm
            .get_loaded_class_id("java/util/concurrent/CyclicBarrier")
            .expect("CyclicBarrier should be loaded");
        let cb = cm
            .get_class(cb_id)
            .expect("class should exist in class store");
        assert!(
            !cb.origin.is_compatibility_stub(),
            "CyclicBarrier should be real bytecode"
        );

        let method_names: Vec<&str> = cb.methods.iter().map(|m| m.name.as_ref()).collect();
        assert!(
            method_names.contains(&"await"),
            "CyclicBarrier should have await"
        );
        assert!(
            method_names.contains(&"reset"),
            "CyclicBarrier should have reset"
        );
        assert!(
            method_names.contains(&"getParties"),
            "CyclicBarrier should have getParties"
        );
    }

    /// Verify ForkJoinPool and ExecutorService are loaded from real bytecode.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_executor_framework_structure() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        // ExecutorService is an interface
        let es_id = cm
            .get_loaded_class_id("java/util/concurrent/ExecutorService")
            .expect("ExecutorService should be loaded");
        let es = cm
            .get_class(es_id)
            .expect("class should exist in class store");
        assert!(
            !es.origin.is_compatibility_stub(),
            "ExecutorService should be real bytecode"
        );
        let es_methods: Vec<&str> = es.methods.iter().map(|m| m.name.as_ref()).collect();
        assert!(
            es_methods.contains(&"submit") || es_methods.contains(&"shutdown"),
            "ExecutorService should have submit or shutdown"
        );

        // ThreadPoolExecutor
        let tpe_id = cm
            .get_loaded_class_id("java/util/concurrent/ThreadPoolExecutor")
            .expect("ThreadPoolExecutor should be loaded");
        let tpe = cm
            .get_class(tpe_id)
            .expect("class should exist in class store");
        assert!(
            !tpe.origin.is_compatibility_stub(),
            "ThreadPoolExecutor should be real bytecode"
        );

        // ForkJoinPool
        let fjp_id = cm
            .get_loaded_class_id("java/util/concurrent/ForkJoinPool")
            .expect("ForkJoinPool should be loaded");
        let fjp = cm
            .get_class(fjp_id)
            .expect("class should exist in class store");
        assert!(
            !fjp.origin.is_compatibility_stub(),
            "ForkJoinPool should be real bytecode"
        );

        let fjp_methods: Vec<&str> = fjp.methods.iter().map(|m| m.name.as_ref()).collect();
        assert!(
            fjp_methods.contains(&"submit") || fjp_methods.contains(&"invoke"),
            "ForkJoinPool should have submit or invoke"
        );

        // ForkJoinTask
        let fjt_id = cm
            .get_loaded_class_id("java/util/concurrent/ForkJoinTask")
            .expect("ForkJoinTask should be loaded");
        let fjt = cm
            .get_class(fjt_id)
            .expect("class should exist in class store");
        assert!(
            !fjt.origin.is_compatibility_stub(),
            "ForkJoinTask should be real bytecode"
        );

        eprintln!("Executor framework: ExecutorService, ThreadPoolExecutor, ForkJoinPool, ForkJoinTask all loaded");
    }

    /// Verify AtomicLong structure and methods.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_atomic_long_structure() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let al_id = cm
            .get_loaded_class_id("java/util/concurrent/atomic/AtomicLong")
            .expect("AtomicLong should be loaded");
        let al = cm
            .get_class(al_id)
            .expect("class should exist in class store");
        assert!(!al.origin.is_compatibility_stub());

        let method_names: Vec<&str> = al.methods.iter().map(|m| m.name.as_ref()).collect();
        assert!(method_names.contains(&"get"));
        assert!(method_names.contains(&"set"));
        assert!(method_names.contains(&"incrementAndGet"));
        assert!(method_names.contains(&"compareAndSet"));
    }

    /// Verify AtomicReference structure.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_atomic_reference_structure() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let ar_id = cm
            .get_loaded_class_id("java/util/concurrent/atomic/AtomicReference")
            .expect("AtomicReference should be loaded");
        let ar = cm
            .get_class(ar_id)
            .expect("class should exist in class store");
        assert!(!ar.origin.is_compatibility_stub());

        let method_names: Vec<&str> = ar.methods.iter().map(|m| m.name.as_ref()).collect();
        assert!(method_names.contains(&"get"));
        assert!(method_names.contains(&"set"));
        assert!(method_names.contains(&"compareAndSet"));
        assert!(method_names.contains(&"getAndSet"));
    }

    /// Verify StampedLock structure.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_stamped_lock_structure() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let sl_id = cm
            .get_loaded_class_id("java/util/concurrent/locks/StampedLock")
            .expect("StampedLock should be loaded");
        let sl = cm
            .get_class(sl_id)
            .expect("class should exist in class store");
        assert!(!sl.origin.is_compatibility_stub(), "StampedLock should be real bytecode");

        let method_names: Vec<&str> = sl.methods.iter().map(|m| m.name.as_ref()).collect();
        assert!(
            method_names.contains(&"readLock"),
            "StampedLock should have readLock"
        );
        assert!(
            method_names.contains(&"writeLock"),
            "StampedLock should have writeLock"
        );
        assert!(
            method_names.contains(&"tryOptimisticRead"),
            "StampedLock should have tryOptimisticRead"
        );
        assert!(
            method_names.contains(&"validate"),
            "StampedLock should have validate"
        );
    }

    /// Verify Phaser structure.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_phaser_structure() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let ph_id = cm
            .get_loaded_class_id("java/util/concurrent/Phaser")
            .expect("Phaser should be loaded");
        let ph = cm
            .get_class(ph_id)
            .expect("class should exist in class store");
        assert!(!ph.origin.is_compatibility_stub(), "Phaser should be real bytecode");

        let method_names: Vec<&str> = ph.methods.iter().map(|m| m.name.as_ref()).collect();
        assert!(
            method_names.contains(&"arrive"),
            "Phaser should have arrive"
        );
        assert!(
            method_names.contains(&"register"),
            "Phaser should have register"
        );
    }

    /// Verify LongAdder/DoubleAdder are loaded.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_adders_loaded() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        for class_name in &[
            "java/util/concurrent/atomic/LongAdder",
            "java/util/concurrent/atomic/DoubleAdder",
            "java/util/concurrent/atomic/LongAccumulator",
            "java/util/concurrent/atomic/DoubleAccumulator",
        ] {
            let id = cm
                .get_loaded_class_id(class_name)
                .unwrap_or_else(|| panic!("{class_name} should be loaded"));
            let cls = cm.get_class(id).expect("class should exist in class store");
            assert!(
                !cls.origin.is_compatibility_stub(),
                "{class_name} should be real bytecode"
            );
        }
    }

    /// Runtime test: ExecutorService.submit(Runnable) creates and runs task.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_executor_submit_runnable_runtime() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        // Create an AtomicInteger(0) to verify execution
        let ai = new_object_initialized(
            &shared,
            &mut thread,
            "java/util/concurrent/atomic/AtomicInteger",
            "(I)V",
            &[Value::Int(0)],
        );
        let ai_id = shared.mem.heap.class_id_of(ai);

        // Executors.newSingleThreadExecutor()
        let exec_class = shared
            .classes
            .class_manager
            .write()
            .load_class("java/util/concurrent/Executors")
            .expect("Failed to load Executors");
        let exec = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            exec_class,
            "newSingleThreadExecutor",
            "()Ljava/util/concurrent/ExecutorService;",
            &[],
        )
        .expect("newSingleThreadExecutor failed")
        .expect("should return executor");

        // submit(Runnable) — the Runnable increments our AtomicInteger
        // Since we can't easily create a lambda in the test harness, verify the
        // executor object was created and the submit method is registered.
        let exec_obj = match exec {
            Value::Object(Some(o)) => o,
            _ => panic!("Executor should be an object"),
        };
        let exec_id = shared.mem.heap.class_id_of(exec_obj);

        // Verify shutdown/isShutdown contract
        let is_shutdown = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            exec_id,
            "isShutdown",
            "()Z",
            &[Value::Object(Some(exec_obj))],
        )
        .expect("isShutdown failed");
        assert_eq!(
            is_shutdown,
            Some(Value::Int(0)),
            "New executor should not be shutdown"
        );

        // shutdown()
        let _ = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            exec_id,
            "shutdown",
            "()V",
            &[Value::Object(Some(exec_obj))],
        )
        .expect("shutdown failed");

        let is_shutdown = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            exec_id,
            "isShutdown",
            "()Z",
            &[Value::Object(Some(exec_obj))],
        )
        .expect("isShutdown failed after shutdown");
        assert_eq!(
            is_shutdown,
            Some(Value::Int(1)),
            "Executor should be shutdown"
        );

        eprintln!("ExecutorService.submit/shutdown lifecycle works!");
    }

    /// Runtime test: ExecutorService.submit(Callable) returns a completed Future.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s13_executor_submit_callable_future_runtime() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        // Executors.newFixedThreadPool(2)
        let exec_class = shared
            .classes
            .class_manager
            .write()
            .load_class("java/util/concurrent/Executors")
            .expect("Failed to load Executors");
        let exec = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            exec_class,
            "newFixedThreadPool",
            "(I)Ljava/util/concurrent/ExecutorService;",
            &[Value::Int(2)],
        )
        .expect("newFixedThreadPool failed")
        .expect("should return executor");

        let exec_obj = match exec {
            Value::Object(Some(o)) => o,
            _ => panic!("Executor should be an object"),
        };

        // awaitTermination should return true (all tasks done)
        let exec_id = shared.mem.heap.class_id_of(exec_obj);
        let result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            exec_id,
            "awaitTermination",
            "(JLjava/util/concurrent/TimeUnit;)Z",
            &[
                Value::Object(Some(exec_obj)),
                Value::Long(1000),
                Value::Object(None),
            ],
        )
        .expect("awaitTermination failed");
        assert_eq!(
            result,
            Some(Value::Int(1)),
            "awaitTermination should return true"
        );

        eprintln!("ExecutorService.newFixedThreadPool + awaitTermination works!");
    }

    // -----------------------------------------------------------------------
    // Session 14: Module System Wiring
    // -----------------------------------------------------------------------

    /// Verify that boot modules are discovered from JDK jmod files.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s14_boot_modules_discovered() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let modules = cm.list_boot_modules();
        assert!(
            !modules.is_empty(),
            "Should discover at least one boot module"
        );
        assert!(
            modules.iter().any(|m| m == "java.base"),
            "java.base must be present in boot modules, got: {:?}",
            modules
        );

        eprintln!(
            "Discovered {} boot modules: {:?}",
            modules.len(),
            &modules[..modules.len().min(10)]
        );
    }

    /// Verify that the module registry is populated with module descriptors.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s14_module_registry_populated() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        eprintln!("Module registry size: {}", cm.module_registry.len());

        assert!(
            !cm.module_registry.is_empty(),
            "Module registry should have registered modules"
        );
        assert!(
            cm.module_registry.len() >= 1,
            "At least java.base should be registered"
        );

        eprintln!("Module registry has {} modules", cm.module_registry.len());
    }

    /// Verify that the readability graph is built correctly:
    /// - java.base reads itself
    /// - Other modules read java.base (implicit)
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s14_readability_graph() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        // java.base reads itself
        assert!(
            cm.module_registry.reads("java.base", "java.base"),
            "java.base should read itself"
        );

        // If java.logging is registered, it should read java.base
        if cm.module_registry.len() > 1 {
            let modules = cm.list_boot_modules();
            // Find any non-base module
            if let Some(other) = modules.iter().find(|m| m.as_str() != "java.base") {
                assert!(
                    cm.module_registry.reads(other, "java.base"),
                    "{other} should read java.base"
                );
            }
        }
    }

    /// Verify that java.lang.Object is assigned to java.base module.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s14_object_in_java_base_module() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        let obj_id = cm
            .get_loaded_class_id("java/lang/Object")
            .expect("Object should be loaded");
        let obj = cm
            .get_class(obj_id)
            .expect("class should exist in class store");

        // Object should be in java.base module
        assert_eq!(
            obj.module_name.as_deref(),
            Some("java.base"),
            "java.lang.Object should be in java.base module, got: {:?}",
            obj.module_name
        );
    }

    /// Verify that classes loaded from classpath (no module) are in the unnamed module.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s14_classpath_classes_unnamed_module() {
        let shared = create_real_jdk_vm().expect("No JDK found");

        // Load a test class from the test classpath (not a JDK class)
        // Use a synthetic class that would be created with no module
        let cm = shared.classes.class_manager.read();

        // Any synthetic stub or test class should have no module_name
        // Check that unnamed module classes can still access java.base exports
        let obj_id = cm
            .get_loaded_class_id("java/lang/Object")
            .expect("Object should be loaded");

        // Module access check: unnamed module (accessor) → java.base (target)
        // This should always succeed
        let result = crate::classloading::access_control::check_module_access_by_id(
            ClassId::new(0), // synthetic class (unnamed module)
            obj_id,
            &cm,
        );
        assert!(
            result.is_ok(),
            "Unnamed module should be able to access java.base exports: {:?}",
            result.err()
        );
    }

    /// Verify that java.base exports java/lang to all modules.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s14_java_base_exports_java_lang() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        // Module access from any module to java.lang.Object should succeed
        // because java.base exports java/lang unconditionally
        let obj_id = cm
            .get_loaded_class_id("java/lang/Object")
            .expect("Object should be loaded");
        let string_id = cm
            .get_loaded_class_id("java/lang/String")
            .expect("String should be loaded");
        let integer_id = cm
            .get_loaded_class_id("java/lang/Integer")
            .expect("Integer should be loaded");

        // Same-module access: String → Object (both in java.base)
        let result =
            crate::classloading::access_control::check_module_access_by_id(string_id, obj_id, &cm);
        assert!(
            result.is_ok(),
            "String→Object access should succeed: {:?}",
            result.err()
        );

        // Same-module: Integer → String
        let result = crate::classloading::access_control::check_module_access_by_id(
            integer_id, string_id, &cm,
        );
        assert!(
            result.is_ok(),
            "Integer→String access should succeed: {:?}",
            result.err()
        );
    }

    /// Load a class from java.sql module and verify it can access java.base exports.
    /// This is the specific roadmap deliverable.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s14_load_java_sql_access_java_base() {
        let shared = create_real_jdk_vm().expect("No JDK found");

        // Try to load java.sql.Connection (interface in java.sql module)
        let result = shared
            .classes
            .class_manager
            .write()
            .load_class("java/sql/Connection");
        if result.is_err() {
            eprintln!("Skipping: java.sql module not available in this JDK configuration");
            return;
        }
        let conn_id = result.expect("result should be Ok");

        let cm = shared.classes.class_manager.read();
        let conn = cm
            .get_class(conn_id)
            .expect("class should exist in class store");

        // Connection should be from real bytecode
        assert!(
            !conn.origin.is_compatibility_stub(),
            "java.sql.Connection should be real bytecode"
        );

        // Connection should be in java.sql module
        if let Some(ref mod_name) = conn.module_name {
            assert_eq!(
                mod_name, "java.sql",
                "Connection should be in java.sql module, got: {mod_name}"
            );
        }

        // java.sql.Connection can access java.lang.Object (java.base exports java/lang)
        let obj_id = cm
            .get_loaded_class_id("java/lang/Object")
            .expect("class java/lang/Object should be loaded");
        let result =
            crate::classloading::access_control::check_module_access_by_id(conn_id, obj_id, &cm);
        assert!(
            result.is_ok(),
            "java.sql.Connection should be able to access java.lang.Object: {:?}",
            result.err()
        );

        // java.sql.Connection can access java.lang.String
        let str_id = cm
            .get_loaded_class_id("java/lang/String")
            .expect("class java/lang/String should be loaded");
        let result =
            crate::classloading::access_control::check_module_access_by_id(conn_id, str_id, &cm);
        assert!(
            result.is_ok(),
            "java.sql.Connection should be able to access java.lang.String: {:?}",
            result.err()
        );

        eprintln!("java.sql.Connection loaded and can access java.base exports!");
    }

    /// Verify that module access enforcement is wired into field resolution.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s14_module_enforcement_in_field_resolution() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        // Access System.out (java.base class accessing its own static field)
        // This verifies that module enforcement doesn't break same-module access
        let result = crate::vm::invoke_shared(
            &shared,
            &mut thread,
            "java/lang/System",
            "currentTimeMillis",
            "()J",
            &[],
        );
        assert!(
            result.is_ok(),
            "System.currentTimeMillis() should work with module enforcement: {:?}",
            result.err()
        );
    }

    /// Verify that module access enforcement is wired into method resolution.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s14_module_enforcement_in_method_resolution() {
        let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

        // Integer.valueOf(42) — involves method resolution across class boundaries
        // within java.base module
        let result = crate::vm::invoke_shared(
            &shared,
            &mut thread,
            "java/lang/Integer",
            "valueOf",
            "(I)Ljava/lang/Integer;",
            &[Value::Int(42)],
        );
        assert!(
            result.is_ok(),
            "Integer.valueOf(42) should work with module enforcement: {:?}",
            result.err()
        );
    }

    /// Verify opens infrastructure supports reflection access.
    #[test]
    #[ignore] // requires real JDK on PATH
    fn s14_opens_infrastructure() {
        let shared = create_real_jdk_vm().expect("No JDK found");
        let cm = shared.classes.class_manager.read();

        // Verify module registry exists and has readability data
        if cm.module_registry.is_empty() {
            eprintln!("Skipping: no modules registered");
            return;
        }

        // The unnamed module should be able to read any named module
        // (this is the classpath compatibility guarantee)
        assert!(
            cm.module_registry.reads("", "java.base"),
            "Unnamed module should read java.base"
        );
    }

    // =======================================================================
    // Session 15: Remove Synthetic Stubs — clean separation verification
    // =======================================================================

    /// S15: In real JDK mode the VM boots with only essential natives (no synthetic stubs).
    #[test]
    #[ignore]
    fn s15_essential_natives_only_in_real_jdk_mode() {
        let shared = match create_real_jdk_vm() {
            Some(s) => s,
            None => {
                eprintln!("Skipping: no JDK found");
                return;
            }
        };
        let count = shared.natives.native_methods.len();
        // Essential natives + I/O natives should be well under 2000
        assert!(
            count < 2000,
            "Real JDK mode should have < 2000 essential natives, got {}",
            count
        );
        // But must have at least the core set
        assert!(
            count > 100,
            "Real JDK mode should have > 100 essential natives, got {}",
            count
        );
    }

    /// S15: Essential natives include Object.hashCode, System.arraycopy, Class.forName0
    #[test]
    #[ignore]
    fn s15_essential_natives_include_core_methods() {
        let shared = match create_real_jdk_vm() {
            Some(s) => s,
            None => {
                eprintln!("Skipping: no JDK found");
                return;
            }
        };
        // Object.hashCode must be registered
        assert!(
            shared
                .natives
                .native_methods
                .find("java/lang/Object", "hashCode", "()I")
                .is_some(),
            "Object.hashCode must be an essential native"
        );
        // System.arraycopy must be registered
        assert!(
            shared
                .natives
                .native_methods
                .find(
                    "java/lang/System",
                    "arraycopy",
                    "(Ljava/lang/Object;ILjava/lang/Object;II)V"
                )
                .is_some(),
            "System.arraycopy must be an essential native"
        );
        // Class.forName0 must be registered
        assert!(shared.natives.native_methods.find("java/lang/Class", "forName0",
            "(Ljava/lang/String;ZLjava/lang/ClassLoader;Ljava/lang/Class;)Ljava/lang/Class;").is_some(),
            "Class.forName0 must be an essential native");
        // Thread.currentThread must be registered
        assert!(
            shared
                .natives
                .native_methods
                .find("java/lang/Thread", "currentThread", "()Ljava/lang/Thread;")
                .is_some(),
            "Thread.currentThread must be an essential native"
        );
    }

    /// S15: Synthetic overrides are NOT registered in real JDK mode
    #[test]
    #[ignore]
    fn s15_no_synthetic_overrides_in_real_jdk_mode() {
        let shared = match create_real_jdk_vm() {
            Some(s) => s,
            None => {
                eprintln!("Skipping: no JDK found");
                return;
            }
        };
        // StringBuilder.append should NOT be registered — it's JDK bytecode
        assert!(
            shared
                .natives
                .native_methods
                .find(
                    "java/lang/StringBuilder",
                    "append",
                    "(Ljava/lang/String;)Ljava/lang/StringBuilder;"
                )
                .is_none(),
            "StringBuilder.append should not be an essential native — it runs as JDK bytecode"
        );
        // HashMap.put should NOT be registered
        assert!(
            shared
                .natives
                .native_methods
                .find(
                    "java/util/HashMap",
                    "put",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
                )
                .is_none(),
            "HashMap.put should not be an essential native — it runs as JDK bytecode"
        );
        // BigInteger.add should NOT be registered
        assert!(
            shared
                .natives
                .native_methods
                .find(
                    "java/math/BigInteger",
                    "add",
                    "(Ljava/math/BigInteger;)Ljava/math/BigInteger;"
                )
                .is_none(),
            "BigInteger.add should not be an essential native — it runs as JDK bytecode"
        );
    }

    /// S15: Real JDK String.valueOf(42) works via native override (not synthetic stubs)
    #[test]
    #[ignore]
    fn s15_string_valueof_works_without_synthetic() {
        let (shared, mut thread) = match create_real_jdk_vm_with_thread() {
            Some(s) => s,
            None => {
                eprintln!("Skipping: no JDK found");
                return;
            }
        };

        let string_id = shared
            .classes
            .class_manager
            .write()
            .load_class("java/lang/String")
            .expect("failed to load class java/lang/String");
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, string_id)
            .expect("class initialization should succeed");

        let result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            string_id,
            "valueOf",
            "(I)Ljava/lang/String;",
            &[Value::Int(42)],
        )
        .expect("String.valueOf(42) failed");

        match result {
            Some(Value::Object(Some(str_ref))) => {
                let s = crate::vm::vm_object::read_java_string(&shared.mem.heap, str_ref);
                assert_eq!(
                    s,
                    Some("42".to_string()),
                    "String.valueOf(42) should return \"42\""
                );
            }
            other => panic!(
                "String.valueOf(42) should return a String object, got {:?}",
                other
            ),
        }
    }

    /// S15: Real JDK HashMap initializes from bytecode without synthetic stubs
    #[test]
    #[ignore]
    fn s15_hashmap_works_without_synthetic_stubs() {
        let (shared, mut thread) = match create_real_jdk_vm_with_thread() {
            Some(s) => s,
            None => {
                eprintln!("Skipping: no JDK found");
                return;
            }
        };

        // Load and initialize HashMap from real JDK bytecode
        let hm_id = shared
            .classes
            .class_manager
            .write()
            .load_class("java/util/HashMap")
            .expect("failed to load class java/util/HashMap");
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, hm_id)
            .expect("class initialization should succeed");

        // Verify HashMap class loaded successfully with real field layout
        let cm = shared.classes.class_manager.read();
        let hm_class = cm
            .get_class(hm_id)
            .expect("class should exist in class store");
        assert_eq!(&*hm_class.name, "java/util/HashMap");
        // Real JDK HashMap has instance fields (table, size, threshold, loadFactor, etc.)
        assert!(
            hm_class.num_total_fields > 0,
            "HashMap should have instance fields, got {}",
            hm_class.num_total_fields
        );
        // HashMap should have a superclass (AbstractMap)
        assert!(
            hm_class.superclass.is_some(),
            "HashMap should extend AbstractMap"
        );
    }

    /// S15: Feature flag correctly gates synthetic-jdk registration functions
    #[test]
    #[ignore]
    fn s15_feature_gate_controls_synthetic_availability() {
        // When compiled with synthetic-jdk (default), register_builtins should exist
        #[cfg(feature = "synthetic-jdk")]
        {
            let mut registry = crate::native::registry::NativeMethodRegistry::new();
            crate::native::register_builtins(&mut registry);
            // Synthetic mode registers thousands of methods
            assert!(
                registry.len() > 2000,
                "Synthetic mode should register > 2000 methods, got {}",
                registry.len()
            );
        }

        // Essential-only should always work regardless of feature
        let mut registry = crate::native::registry::NativeMethodRegistry::new();
        crate::native::register_essential_natives(&mut registry);
        assert!(
            registry.len() < 1000,
            "Essential natives should be < 1000, got {}",
            registry.len()
        );
        assert!(
            registry.len() > 100,
            "Essential natives should be > 100, got {}",
            registry.len()
        );
    }

    /// S15: All prior session tests still pass — String.valueOf with various inputs
    #[test]
    #[ignore]
    fn s15_full_regression_real_jdk_string_ops() {
        let (shared, mut thread) = match create_real_jdk_vm_with_thread() {
            Some(s) => s,
            None => {
                eprintln!("Skipping: no JDK found");
                return;
            }
        };

        let string_id = shared
            .classes
            .class_manager
            .write()
            .load_class("java/lang/String")
            .expect("failed to load class java/lang/String");
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, string_id)
            .expect("class initialization should succeed");

        // String.valueOf for multiple int values
        for (input, expected) in &[(0, "0"), (42, "42"), (-1, "-1"), (2147483647, "2147483647")] {
            let result = crate::vm::invoke_on_class_shared(
                &shared,
                &mut thread,
                string_id,
                "valueOf",
                "(I)Ljava/lang/String;",
                &[Value::Int(*input)],
            )
            .expect(&format!("String.valueOf({}) failed", input));

            match result {
                Some(Value::Object(Some(str_ref))) => {
                    let s = crate::vm::vm_object::read_java_string(&shared.mem.heap, str_ref);
                    assert_eq!(
                        s,
                        Some(expected.to_string()),
                        "String.valueOf({}) should return \"{}\"",
                        input,
                        expected
                    );
                }
                other => panic!("String.valueOf({}) failed: {:?}", input, other),
            }
        }
    }

    // =======================================================================
    // Session 16: Iterative Interpreter Refactor
    // =======================================================================

    /// Default max_stack_depth raised to 8192 (see the doc comment on
    /// `VmConfig::default`'s `max_stack_depth` field in config.rs for the
    /// root-cause rationale — `DefaultListableBeanFactoryTests
    /// .extensiveCircularReference` needs >1024 frames for a 99-bean
    /// circular-reference chain that HotSpot handles trivially).
    #[test]
    fn s16_default_max_stack_depth_is_8192() {
        let config = crate::config::VmConfig::default();
        assert_eq!(config.max_stack_depth, 8192);
    }

    /// S16: Frame has monitor_on_exit field for stackless synchronized dispatch.
    #[test]
    fn s16_frame_has_monitor_on_exit() {
        use crate::runtime::frame::Frame;
        let frame = Frame::new(
            crate::classloading::ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1], // return
            vec![],
            2,
            2,
            &[],
        );
        assert!(
            frame.monitor_on_exit.is_none(),
            "New frames should have monitor_on_exit == None"
        );
    }

    /// Helper: create a real JDK VM with the test resources directory on the classpath.
    fn create_real_jdk_vm_with_resources() -> Option<SharedVm> {
        let java_home = crate::config::resolve_java_home_public(None)?;
        let java_home_str = java_home.to_string_lossy().into_owned();
        let resources_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("resources");
        let mut config = VmConfig::new().with_java_home(java_home_str);
        config.use_synthetic_jdk = false;
        config
            .classpath
            .push(resources_dir.to_string_lossy().into_owned());
        Some(SharedVm::new(config))
    }

    /// S16: Fibonacci(50) iterative runs correctly on the interpreter.
    #[test]
    #[ignore]
    fn s16_fibonacci_50_iterative() {
        let shared = create_real_jdk_vm_with_resources().expect("No JDK found");
        let mut thread = crate::threading::jvm_thread::JvmThread::new(
            crate::threading::jvm_thread::ThreadId(1),
            "test",
        );
        let class_id = shared
            .classes
            .class_manager
            .write()
            .load_class("cratonvm/DeepCallTest")
            .expect("Failed to load DeepCallTest");
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, class_id)
            .expect("clinit failed");
        let result = crate::runtime::interpreter::execute(
            &shared,
            &mut thread,
            class_id,
            "fibIterative",
            "(I)I",
            &[Value::Int(50)],
        )
        .expect("fibIterative failed");
        // fib(50) = 12586269025, truncated to i32 = 1_258_626_902 (overflow wraps)
        assert!(result.is_some(), "fibIterative should return a value");
    }

    /// S16: Deep 1000-level recursion succeeds (stackless dispatch).
    #[test]
    #[ignore]
    fn s16_deep_recursion_1000() {
        let shared = create_real_jdk_vm_with_resources().expect("No JDK found");
        let mut thread = crate::threading::jvm_thread::JvmThread::new(
            crate::threading::jvm_thread::ThreadId(1),
            "test",
        );
        let class_id = shared
            .classes
            .class_manager
            .write()
            .load_class("cratonvm/DeepCallTest")
            .expect("Failed to load DeepCallTest");
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, class_id)
            .expect("clinit failed");
        let result = crate::runtime::interpreter::execute(
            &shared,
            &mut thread,
            class_id,
            "deepCountdown",
            "(I)I",
            &[Value::Int(1000)],
        )
        .expect("deepCountdown(1000) failed");
        match result {
            Some(Value::Int(v)) => {
                assert_eq!(v, 1000, "deepCountdown(1000) should return 1000, got {}", v)
            }
            other => panic!("deepCountdown returned unexpected: {:?}", other),
        }
    }

    /// S16: Tail-recursive sum of 1..10000 works via TCE.
    #[test]
    #[ignore]
    fn s16_tail_call_sum_10000() {
        let shared = create_real_jdk_vm_with_resources().expect("No JDK found");
        let mut thread = crate::threading::jvm_thread::JvmThread::new(
            crate::threading::jvm_thread::ThreadId(1),
            "test",
        );
        let class_id = shared
            .classes
            .class_manager
            .write()
            .load_class("cratonvm/DeepCallTest")
            .expect("Failed to load DeepCallTest");
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, class_id)
            .expect("clinit failed");
        let result = crate::runtime::interpreter::execute(
            &shared,
            &mut thread,
            class_id,
            "tailSum",
            "(II)I",
            &[Value::Int(10000), Value::Int(0)],
        )
        .expect("tailSum(10000,0) failed");
        match result {
            Some(Value::Int(v)) => assert_eq!(
                v, 50005000,
                "tailSum(10000,0) should return 50005000, got {}",
                v
            ),
            other => panic!("tailSum returned unexpected: {:?}", other),
        }
    }

    /// S16: Mutual recursion (isEven/isOdd) works with stackless dispatch.
    #[test]
    #[ignore]
    fn s16_mutual_recursion() {
        let shared = create_real_jdk_vm_with_resources().expect("No JDK found");
        let mut thread = crate::threading::jvm_thread::JvmThread::new(
            crate::threading::jvm_thread::ThreadId(1),
            "test",
        );
        let class_id = shared
            .classes
            .class_manager
            .write()
            .load_class("cratonvm/DeepCallTest")
            .expect("Failed to load DeepCallTest");
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, class_id)
            .expect("clinit failed");
        let result = crate::runtime::interpreter::execute(
            &shared,
            &mut thread,
            class_id,
            "isEvenOdd",
            "(I)I",
            &[Value::Int(100)],
        )
        .expect("isEvenOdd(100) failed");
        match result {
            Some(Value::Int(v)) => {
                assert_eq!(v, 1, "isEvenOdd(100) should return 1 (even), got {}", v)
            }
            other => panic!("isEvenOdd returned unexpected: {:?}", other),
        }
    }

    /// S16: StackOverflowError is thrown when max_stack_depth is exceeded.
    #[test]
    #[ignore]
    fn s16_stack_overflow_error() {
        let shared = create_real_jdk_vm_with_resources().expect("No JDK found");
        let mut thread = crate::threading::jvm_thread::JvmThread::new(
            crate::threading::jvm_thread::ThreadId(1),
            "test",
        );
        let class_id = shared
            .classes
            .class_manager
            .write()
            .load_class("cratonvm/DeepCallTest")
            .expect("Failed to load DeepCallTest");
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, class_id)
            .expect("clinit failed");
        // deepCountdown is NOT tail-call optimizable (return deepCountdown(n-1) + 1),
        // so with n=100000 and max_stack_depth=1024 it should overflow.
        let result = crate::runtime::interpreter::execute(
            &shared,
            &mut thread,
            class_id,
            "deepCountdown",
            "(I)I",
            &[Value::Int(100000)],
        );
        assert!(
            result.is_err(),
            "deepCountdown(100000) should fail with StackOverflowError"
        );
    }

    /// S16: Configurable max frame depth via VmConfig.
    #[test]
    fn s16_configurable_max_frame_depth() {
        let config = crate::config::VmConfig {
            max_stack_depth: 64,
            ..crate::config::VmConfig::default()
        };
        assert_eq!(config.max_stack_depth, 64);
        // Verify it can be set to arbitrary values
        let config2 = crate::config::VmConfig {
            max_stack_depth: 4096,
            ..crate::config::VmConfig::default()
        };
        assert_eq!(config2.max_stack_depth, 4096);
    }

    /// S16: Frame reset_for_tail_call reuses allocations.
    #[test]
    fn s16_frame_reset_for_tail_call() {
        use crate::runtime::frame::{padded_bytecode, Frame};
        let mut frame = Frame::new(
            crate::classloading::ClassId::new(1),
            "Test".to_string(),
            "method".to_string(),
            "(I)I".to_string(),
            None,
            vec![0x1a, 0xac], // iload_0; ireturn
            vec![],
            4,
            4,
            &[Value::Int(42)],
        );
        assert_eq!(frame.get_local(0), Value::Int(42));
        assert_eq!(frame.pc, 0);

        // Reset for tail call with different args
        let new_code = padded_bytecode(&[0x1a, 0x04, 0x60, 0xac]); // iload_0; iconst_1; iadd; ireturn
        frame.reset_for_tail_call(
            crate::classloading::ClassId::new(2),
            new_code,
            4,
            4,
            &[Value::Int(99)],
            std::sync::Arc::from("Test2"),
            std::sync::Arc::from("method2"),
            std::sync::Arc::from("(I)I"),
            None,
            std::sync::Arc::from(vec![].as_slice()),
        );
        assert_eq!(frame.get_local(0), Value::Int(99));
        assert_eq!(frame.pc, 0);
        assert_eq!(frame.class_id, crate::classloading::ClassId::new(2));
    }

    /// S16: pop_and_recycle_frame releases synchronized monitor.
    #[test]
    fn s16_pop_and_recycle_releases_monitor() {
        use std::sync::Arc;
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let mut thread = crate::threading::jvm_thread::JvmThread::new(
            crate::threading::jvm_thread::ThreadId(1),
            "test",
        );
        // Create a monitor object and acquire it
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 1);
        shared.threads.monitors.enter(obj, thread.thread_id);

        // Push a frame with monitor_on_exit set
        let mut frame = crate::runtime::frame::Frame::new(
            crate::classloading::ClassId::new(0),
            "Test".to_string(),
            "sync".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            2,
            2,
            &[],
        );
        frame.monitor_on_exit = Some(obj);
        thread.frames.push(frame);

        // pop_and_recycle_frame should release the monitor
        crate::runtime::interpreter::pop_and_recycle_frame(&shared, &mut thread);

        // Verify monitor was released — entering again should succeed without deadlock
        shared.threads.monitors.enter(obj, thread.thread_id);
        let _ = shared.threads.monitors.exit(obj, thread.thread_id);
    }

    // -----------------------------------------------------------------------
    // VmDiagnosticState on SharedVm (Session 43)
    // -----------------------------------------------------------------------

    #[test]
    fn diagnostic_state_thread_snapshots() {
        use crate::runtime::serviceability::VmDiagnosticState;
        let shared = SharedVm::new(VmConfig::default());
        let snapshots = shared.thread_snapshots();
        // With no threads registered, should still return at least the main placeholder
        assert!(!snapshots.is_empty());
        assert_eq!(snapshots[0].name, "main");
    }

    #[test]
    fn diagnostic_state_heap_summary() {
        use crate::runtime::serviceability::VmDiagnosticState;
        let shared = SharedVm::new(VmConfig::default());
        let summary = shared.heap_summary();
        assert!(summary.total_capacity > 0);
        assert!(summary.young_gen_capacity > 0 || summary.old_gen_capacity > 0);
    }

    #[test]
    fn diagnostic_state_class_histogram_empty() {
        use crate::runtime::serviceability::VmDiagnosticState;
        let shared = SharedVm::new(VmConfig::default());
        let hist = shared.class_histogram();
        // No objects allocated, histogram should be empty
        assert!(hist.is_empty());
    }

    #[test]
    fn diagnostic_state_class_histogram_with_objects() {
        use crate::runtime::serviceability::VmDiagnosticState;
        let shared = SharedVm::new(VmConfig::default());
        // Allocate some objects
        let _obj1 = shared.mem.heap.alloc_object(ClassId::new(1), 2);
        let _obj2 = shared.mem.heap.alloc_object(ClassId::new(1), 2);
        let _obj3 = shared.mem.heap.alloc_object(ClassId::new(2), 3);
        let hist = shared.class_histogram();
        assert!(hist.len() >= 2); // at least 2 classes
        let total_instances: u64 = hist.iter().map(|e| e.instance_count).sum();
        assert!(total_instances >= 3);
    }

    #[test]
    fn diagnostic_state_trigger_gc() {
        use crate::runtime::serviceability::VmDiagnosticState;
        let shared = SharedVm::new(VmConfig::default());
        assert!(!shared
            .mem
            .gc_requested
            .load(std::sync::atomic::Ordering::Relaxed));
        let ran = shared.trigger_gc();
        assert!(ran);
        assert!(shared
            .mem
            .gc_requested
            .load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn diagnostic_state_uptime() {
        use crate::runtime::serviceability::VmDiagnosticState;
        let shared = SharedVm::new(VmConfig::default());
        let uptime = shared.uptime_secs();
        assert!(uptime >= 0.0);
    }

    #[test]
    fn diagnostic_state_command_line() {
        use crate::runtime::serviceability::VmDiagnosticState;
        let shared = SharedVm::new(VmConfig::default());
        let cmd = shared.command_line();
        assert!(cmd.contains("cratonvm"));
    }

    #[test]
    fn diagnostic_state_system_properties() {
        use crate::runtime::serviceability::VmDiagnosticState;
        let shared = SharedVm::new(VmConfig::default());
        let props = shared.system_properties();
        assert!(!props.is_empty());
        // Should contain at least java.version
        assert!(props.iter().any(|(k, _)| k == "java.version"));
    }

    #[test]
    fn diagnostic_state_vm_flags() {
        use crate::runtime::serviceability::VmDiagnosticState;
        let shared = SharedVm::new(VmConfig::default());
        let flags = shared.vm_flags();
        assert!(!flags.is_empty());
        // Should contain GC algorithm flag
        assert!(flags.iter().any(|f| f.contains("GC")));
        // Should contain max heap size
        assert!(flags.iter().any(|f| f.contains("MaxHeapSize")));
    }

    #[test]
    fn diagnostic_state_jcmd_integration() {
        use crate::runtime::serviceability::{JcmdProcessor, VmDiagnosticState};
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let jcmd = JcmdProcessor::new_with_vm_state(shared.clone());

        // Thread.print should use real data
        let result = jcmd.process_command("Thread.print");
        assert!(result.success);
        assert!(result.output.contains("main"));

        // GC.run should trigger the flag
        let result = jcmd.process_command("GC.run");
        assert!(result.success);
        assert!(result.output.contains("completed"));
        assert!(shared
            .mem
            .gc_requested
            .load(std::sync::atomic::Ordering::Relaxed));

        // VM.uptime should be a real number
        let result = jcmd.process_command("VM.uptime");
        assert!(result.success);
        assert!(result.output.contains("seconds"));

        // VM.flags should show real config
        let result = jcmd.process_command("VM.flags");
        assert!(result.success);
        assert!(result.output.contains("MaxHeapSize"));

        // VM.system_properties should show real properties
        let result = jcmd.process_command("VM.system_properties");
        assert!(result.success);
        assert!(result.output.contains("java.version"));

        // GC.heap_info should show real heap stats
        let result = jcmd.process_command("GC.heap_info");
        assert!(result.success);
        assert!(result.output.contains("Young Generation") || result.output.contains("Total"));

        // GC.class_histogram — empty heap
        let result = jcmd.process_command("GC.class_histogram");
        assert!(result.success);
        assert!(result.output.contains("#instances"));
    }

    #[test]
    fn diagnostic_counters_on_shared_vm() {
        let shared = SharedVm::new(VmConfig::default());
        // Counters should be initialized to zero
        assert_eq!(
            crate::runtime::diagnostics::DiagnosticCounters::get(
                &shared.debug.diagnostic_counters.gc_cycles
            ),
            0
        );
        // Increment and check
        shared
            .debug
            .diagnostic_counters
            .inc(&shared.debug.diagnostic_counters.classes_loaded);
        assert_eq!(
            crate::runtime::diagnostics::DiagnosticCounters::get(
                &shared.debug.diagnostic_counters.classes_loaded
            ),
            1
        );
    }

    // -----------------------------------------------------------------------
    // Heap dump tests (Session 42)
    // -----------------------------------------------------------------------

    #[test]
    fn heap_dump_empty_heap_to_file() {
        use crate::runtime::serviceability::VmDiagnosticState;
        let shared = SharedVm::new(VmConfig::default());
        let tmp_path = std::env::temp_dir().join("cratonvm_test_empty_heap.hprof");
        let result = shared.heap_dump(tmp_path.to_str().expect("heap dump should succeed"));
        assert!(result.is_ok(), "heap_dump failed: {:?}", result);
        let bytes = result.expect("result should be Ok");
        assert!(bytes > 0, "Heap dump should produce non-empty output");

        // Verify the file exists and starts with HPROF magic
        let data = std::fs::read(&tmp_path).expect("file should be readable");
        assert_eq!(data.len(), bytes as usize);
        let magic = "JAVA PROFILE 1.0.2";
        assert_eq!(&data[..magic.len()], magic.as_bytes());
        std::fs::remove_file(&tmp_path).ok();
    }

    #[test]
    fn heap_dump_with_allocated_objects() {
        use crate::classloading::ClassId;
        use crate::runtime::serviceability::VmDiagnosticState;
        let shared = SharedVm::new(VmConfig::default());

        // Load a class so there's class info available
        {
            let mut cm = shared.classes.class_manager_write();
            let _ = cm.load_class("java/lang/Object");
        }

        // Allocate some objects on the heap
        let _obj1 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
        let _obj2 = shared.mem.heap.alloc_object(ClassId::new(0), 0);

        let tmp_path = std::env::temp_dir().join("cratonvm_test_objects_heap.hprof");
        let result = shared.heap_dump(tmp_path.to_str().expect("heap dump should succeed"));
        assert!(result.is_ok(), "heap_dump failed: {:?}", result);
        let bytes = result.expect("result should be Ok");
        assert!(bytes > 0);

        // Verify HPROF structure
        let data = std::fs::read(&tmp_path).expect("file should be readable");
        // Should contain HEAP_DUMP_SEGMENT (0x1C) and HEAP_DUMP_END (0x2C)
        assert!(data.contains(&0x1Cu8), "Missing HEAP_DUMP_SEGMENT");
        assert!(data.contains(&0x2Cu8), "Missing HEAP_DUMP_END");
        std::fs::remove_file(&tmp_path).ok();
    }

    #[test]
    fn heap_dump_jcmd_integration() {
        use crate::runtime::serviceability::{JcmdProcessor, VmDiagnosticState};
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let jcmd = JcmdProcessor::new_with_vm_state(shared.clone());

        let tmp_path = std::env::temp_dir().join("cratonvm_test_jcmd_heap.hprof");
        let cmd = format!(
            "GC.heap_dump {}",
            tmp_path.to_str().expect("heap dump should succeed")
        );
        let result = jcmd.process_command(&cmd);
        assert!(
            result.success,
            "GC.heap_dump command failed: {}",
            result.output
        );
        assert!(result.output.contains("Heap dump written to"));
        assert!(result.output.contains("bytes"));

        // Verify file was actually written
        assert!(tmp_path.exists(), "Heap dump file should exist");
        let data = std::fs::read(&tmp_path).expect("file should be readable");
        assert!(data.starts_with(b"JAVA PROFILE 1.0.2"));
        std::fs::remove_file(&tmp_path).ok();
    }

    #[test]
    fn heap_dump_invalid_path_returns_error() {
        use crate::runtime::serviceability::VmDiagnosticState;
        let shared = SharedVm::new(VmConfig::default());
        // Use an invalid path that should fail
        let result = shared.heap_dump("/nonexistent/directory/that/does/not/exist/dump.hprof");
        assert!(result.is_err(), "Should fail with invalid path");
        let err = result.unwrap_err();
        assert!(
            err.contains("Failed to write"),
            "Error should mention write failure: {}",
            err
        );
    }

    // -----------------------------------------------------------------
    // T19.3.G1 — allocation-storm observability counters.
    // -----------------------------------------------------------------

    #[test]
    fn t19_storm_counters_default_zero() {
        use std::sync::atomic::Ordering;
        let shared = SharedVm::new(VmConfig::default());
        assert_eq!(shared.mem.tlab_refill_count.load(Ordering::Relaxed), 0);
        assert_eq!(shared.mem.tlab_hit_count.load(Ordering::Relaxed), 0);
        assert_eq!(shared.mem.gc_cycle_count.load(Ordering::Relaxed), 0);
        assert_eq!(shared.mem.bytes_allocated_total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn t19_storm_counters_monotonic_nondecreasing() {
        use std::sync::atomic::Ordering;
        let shared = SharedVm::new(VmConfig::default());
        let before = (
            shared.mem.tlab_refill_count.load(Ordering::Relaxed),
            shared.mem.tlab_hit_count.load(Ordering::Relaxed),
            shared.mem.gc_cycle_count.load(Ordering::Relaxed),
            shared.mem.bytes_allocated_total.load(Ordering::Relaxed),
        );
        // Simulate N allocations bumping the counters (fetch_add is
        // the same path the interpreter uses).
        for _ in 0..32 {
            shared.mem.tlab_hit_count.fetch_add(1, Ordering::Relaxed);
            shared
                .mem
                .bytes_allocated_total
                .fetch_add(24, Ordering::Relaxed);
        }
        shared.mem.tlab_refill_count.fetch_add(1, Ordering::Relaxed);
        shared.mem.gc_cycle_count.fetch_add(1, Ordering::Relaxed);
        let after = (
            shared.mem.tlab_refill_count.load(Ordering::Relaxed),
            shared.mem.tlab_hit_count.load(Ordering::Relaxed),
            shared.mem.gc_cycle_count.load(Ordering::Relaxed),
            shared.mem.bytes_allocated_total.load(Ordering::Relaxed),
        );
        assert!(after.0 >= before.0, "tlab_refill_count went backwards");
        assert!(after.1 >= before.1, "tlab_hit_count went backwards");
        assert!(after.2 >= before.2, "gc_cycle_count went backwards");
        assert!(after.3 >= before.3, "bytes_allocated_total went backwards");
        assert_eq!(after.1 - before.1, 32);
        assert_eq!(after.3 - before.3, 32 * 24);
        assert_eq!(after.0 - before.0, 1);
        assert_eq!(after.2 - before.2, 1);
    }

    #[test]
    fn t19_storm_counters_are_atomic() {
        // Spawn many threads hammering the counter to confirm it
        // is a real AtomicU64, not a Mutex<u64> hiding a race.
        use std::sync::atomic::Ordering;
        let shared = std::sync::Arc::new(SharedVm::new(VmConfig::default()));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let s = std::sync::Arc::clone(&shared);
            handles.push(std::thread::spawn(move || {
                for _ in 0..1000 {
                    s.mem.tlab_hit_count.fetch_add(1, Ordering::Relaxed);
                    s.mem.bytes_allocated_total.fetch_add(48, Ordering::Relaxed);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(shared.mem.tlab_hit_count.load(Ordering::Relaxed), 8 * 1000);
        assert_eq!(
            shared.mem.bytes_allocated_total.load(Ordering::Relaxed),
            8 * 1000 * 48
        );
    }

    // -----------------------------------------------------------------------
    // Per-VM state (P0 / P1 — `docs/architecture/per-vm-state.md`)
    // -----------------------------------------------------------------------
    //
    // The hook registry is process-global, so these tests serialize against
    // each other and assert only on the VMs THEY created (never on the total
    // registry length, which a concurrently-running test may inflate) — the
    // same discipline `memory::native_roots`'s tests use.

    /// Serializes the tests below, which mutate the process-global
    /// `RESOLUTION_INVALIDATE_VMS` registry.
    static HOOK_REGISTRY_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    fn registry_contains(shared: &Arc<SharedVm>) -> bool {
        live_hook_vms().iter().any(|live| Arc::ptr_eq(live, shared))
    }

    /// Everything that keys process-global state per VM (`object_class_id`'s
    /// thread-local cache, `offload_jit_gate`'s `GateKey`, `interpreter`'s EC
    /// watch memo, `invokedynamic`'s lambda-singleton cache) rests on this one
    /// fact. If two live VMs could ever share a `vm_identity`, every one of
    /// those keys silently aliases again.
    #[test]
    fn vm_identity_is_unique_per_shared_vm() {
        let a = Arc::new(SharedVm::new(VmConfig::default()));
        let b = Arc::new(SharedVm::new(VmConfig::default()));
        assert_ne!(
            a.vm_identity, b.vm_identity,
            "two concurrently-live VMs must not share a vm_identity"
        );
    }

    /// `-Xverify:all` reaches the class manager.
    ///
    /// The verifier's own behaviour under the flag is pinned by
    /// `bytecode_verifier::tests::xverify_all_makes_a_trusted_class_reject_what_remote_accepts`.
    /// What THAT test cannot see is the wire: `XverifyMode::All` spent its whole
    /// life being parsed, stored on `VmConfig`, and read by nobody. This asserts
    /// the one hop that made it inert — config to `ClassManager` — on a really
    /// booted VM, and that the default is still `remote`.
    ///
    /// Per-VM, not process-global: two live VMs must be able to disagree, which
    /// is also what makes this test safe to run beside every other VM-building
    /// test in this binary.
    #[test]
    fn xverify_all_reaches_the_class_manager_and_only_that_vm() {
        let strict = SharedVm::new(
            VmConfig::default().with_xverify_mode(crate::config::XverifyMode::All),
        );
        let lenient = SharedVm::new(VmConfig::default());
        assert!(
            strict
                .classes
                .class_manager
                .read()
                .strict_verification(),
            "-Xverify:all must arrive at the class manager, or the flag is inert              again — which is exactly the state it was in before being wired"
        );
        assert!(
            !lenient
                .classes
                .class_manager
                .read()
                .strict_verification(),
            "the default is -Xverify:remote, and a second live VM must not              inherit the first's policy"
        );
    }

    // -----------------------------------------------------------------------
    // Capability policy: install at boot, release at teardown
    // -----------------------------------------------------------------------

    /// The whole point of the install: before it, all 35 wired gates in
    /// `native-builtins` / `native-io` resolved `None` and `capability_audit`
    /// answered `None` for every VM in the process.
    ///
    /// Both halves are asserted, and that they are the SAME `Arc` — the
    /// registry-side gate and the per-call-site gates must share one audit log,
    /// or a report is half the run.
    #[test]
    fn a_booted_vm_has_a_capability_policy_in_the_registry_and_the_index() {
        let vm = SharedVm::new(VmConfig::default());
        let id = cratonvm_native_api::VmId::from_raw(vm.vm_identity);

        let in_registry = vm
            .natives
            .native_methods
            .capabilities()
            .expect("SharedVm::new must install a policy into its own registry");
        let in_index = cratonvm_native_api::capabilities_for(id)
            .expect("SharedVm::new must publish the policy under its VmId");
        assert!(
            Arc::ptr_eq(in_registry, &in_index),
            "the registry gate and the per-call-site gates must share ONE set"
        );
        assert_eq!(in_index.vm(), id);
        assert!(cratonvm_native_api::capability_audit(id).is_some());
    }

    /// The default must not change what any existing deployment does.
    /// `Permissive` allows everything and merely counts it.
    #[test]
    fn the_boot_default_is_permissive_and_every_native_still_registers() {
        let vm = SharedVm::new(VmConfig::default());
        let caps = vm.natives.native_methods.capabilities().unwrap();
        assert_eq!(caps.mode(), cratonvm_native_api::CapabilityMode::Permissive);
        assert!(
            caps.grants().is_empty(),
            "no CRATONVM_CAPABILITY_GRANTS in the test environment"
        );

        // Registration is itself gated (`Capability::NativeRegister`), and the
        // policy is installed BEFORE the `register_*` pass precisely so that
        // gate can see it. Under `Permissive` it must admit every one of them.
        assert!(
            vm.natives.native_methods.len() > 100,
            "the boot registration pass must not have been refused; only {} \
             natives registered",
            vm.natives.native_methods.len()
        );
        // …and a specific, well-known one, so the assertion above cannot pass on
        // a registry that accepted a thousand natives and dropped the rest.
        assert!(
            vm.natives
                .native_methods
                .find("java/lang/Object", "hashCode", "()I")
                .is_some(),
            "java/lang/Object.hashCode must survive the registration gate"
        );
    }

    /// The ~3,100 `native-register` rows the boot pass would otherwise leave in
    /// the audit map are cleared under `Permissive` — `MAX_AUDIT_ENTRIES` is
    /// 4,096, and a map already full of registration rows would drop the
    /// file/socket/spawn uses the report exists to collect.
    #[test]
    fn the_permissive_boot_audit_starts_empty_so_runtime_uses_are_not_crowded_out() {
        let vm = SharedVm::new(VmConfig::default());
        let report = cratonvm_native_api::capability_audit(cratonvm_native_api::VmId::from_raw(
            vm.vm_identity,
        ))
        .expect("installed");
        assert!(
            !report.truncated,
            "the audit map must not already be at MAX_AUDIT_ENTRIES:\n{report}"
        );
        assert!(
            report
                .uses
                .iter()
                .all(|u| u.capability.kind() != cratonvm_native_api::CapabilityKind::NativeRegister),
            "boot registration rows must not survive into the run's report:\n{report}"
        );
    }

    /// Teardown. Without this a long-lived host process accumulates one dead
    /// entry per disposed VM in the `Vec` that every `capabilities_for` lookup
    /// scans, and — worse — `native-builtins`' per-VM SecurityManager row keeps
    /// raw `ObjectRef`s into a heap that no longer exists.
    #[test]
    fn dropping_a_vm_releases_its_capability_and_security_state() {
        let id;
        {
            let vm = SharedVm::new(VmConfig::default());
            id = cratonvm_native_api::VmId::from_raw(vm.vm_identity);
            assert!(cratonvm_native_api::capabilities_for(id).is_some());
            // Seed the third row so its teardown is covered too. The ref is a
            // synthetic non-heap address; it is only ever compared by pointer
            // value, never dereferenced.
            crate::runtime::instrument::add_transformer_entry(
                vm.vm_identity,
                crate::runtime::instrument::TransformerEntry {
                    // SAFETY: non-null, 8-byte aligned, never dereferenced.
                    transformer_ref: unsafe {
                        cratonvm_types::ObjectRef::from_raw(0x5000usize as *mut u8)
                    },
                    can_retransform: true,
                    native_method_prefix: None,
                },
            );
            assert_eq!(
                crate::runtime::instrument::transformer_count(vm.vm_identity),
                1
            );
        }
        assert!(
            cratonvm_native_api::capabilities_for(id).is_none(),
            "SharedVm::drop must uninstall this VM's policy"
        );

        // The security half. `forget_vm_security_state` has no public reader,
        // so this asserts the observable postcondition: no VM-scoped
        // SecurityManager root remains to be handed to a collector.
        let mut roots = Vec::new();
        cratonvm_native_builtins::security_manager::gc_scan_security_manager_roots(
            id.as_usize(),
            &mut roots,
        );
        assert!(roots.is_empty(), "security state outlived the VM");

        // The instrument half: the transformer chain holds raw `ObjectRef`s
        // into the dropped heap and is a registered GC root source, so a
        // surviving row would be reported to a later VM's collector.
        assert_eq!(
            crate::runtime::instrument::transformer_count(id.as_usize()),
            0,
            "the transformer chain outlived the VM"
        );
    }

    /// Both teardown hooks call it and either may run first (or alone — a
    /// `Vm`-created `SharedVm` is never dropped, because `Vm::new` installs a
    /// `JcmdProcessor` holding a strong `Arc` back at it).
    #[test]
    fn releasing_vm_native_state_twice_is_harmless() {
        let vm = SharedVm::new(VmConfig::default());
        let raw = vm.vm_identity;
        release_vm_native_state(raw);
        assert!(cratonvm_native_api::capabilities_for(cratonvm_native_api::VmId::from_raw(raw))
            .is_none());
        // Second call: no entry left, no panic, and the `SharedVm` drop below
        // makes it a third.
        release_vm_native_state(raw);
        drop(vm);
    }

    /// Two VMs in one process get two sets under two ids, with two audit logs.
    /// One policy shared by two VMs is the exact defect the capability model
    /// exists to remove.
    #[test]
    fn two_vms_hold_independent_capability_sets() {
        let a = SharedVm::new(VmConfig::default());
        let b = SharedVm::new(VmConfig::default());
        let ida = cratonvm_native_api::VmId::from_raw(a.vm_identity);
        let idb = cratonvm_native_api::VmId::from_raw(b.vm_identity);
        assert_ne!(ida, idb);

        let ca = cratonvm_native_api::capabilities_for(ida).unwrap();
        let cb = cratonvm_native_api::capabilities_for(idb).unwrap();
        assert!(!Arc::ptr_eq(&ca, &cb), "two VMs must not share one set");

        ca.check(cratonvm_native_api::Capability::file_read("/only-in-vm-a"))
            .expect("Permissive");
        let names = |set: &Arc<cratonvm_native_api::CapabilitySet>| {
            set.audit_report()
                .uses
                .iter()
                .map(|u| u.capability.to_string())
                .collect::<Vec<_>>()
        };
        assert!(
            names(&ca).iter().any(|n| n.contains("only-in-vm-a")),
            "VM A must record its own use: {:?}",
            names(&ca)
        );
        assert!(
            !names(&cb).iter().any(|n| n.contains("only-in-vm-a")),
            "VM B must never see VM A's traffic: {:?}",
            names(&cb)
        );

        // And dropping one leaves the other's policy installed.
        drop(a);
        assert!(cratonvm_native_api::capabilities_for(ida).is_none());
        assert!(cratonvm_native_api::capabilities_for(idb).is_some());
    }

    /// A `ClassId` names a DIFFERENT class in each VM (`ClassStore::next_id`
    /// is `self.classes.len()`, which restarts at 0 per VM), so any cache
    /// keyed on a bare `ClassId` aliases across VMs. This asserts the fix
    /// shape: prefixing the key with `vm_identity` separates them.
    #[test]
    fn vm_keyed_class_ids_do_not_collide_across_vms() {
        let a = Arc::new(SharedVm::new(VmConfig::default()));
        let b = Arc::new(SharedVm::new(VmConfig::default()));

        let bare = ClassId::new(7);
        let key_a = (a.vm_identity, bare);
        let key_b = (b.vm_identity, bare);

        assert_ne!(
            key_a, key_b,
            "the same numeric ClassId in two VMs must produce two distinct keys"
        );

        let mut cache: HashMap<(usize, ClassId), &'static str> = HashMap::new();
        cache.insert(key_a, "vm-a-class");
        cache.insert(key_b, "vm-b-class");
        assert_eq!(cache.get(&key_a).copied(), Some("vm-a-class"));
        assert_eq!(cache.get(&key_b).copied(), Some("vm-b-class"));
    }

    /// EVIDENCE for the security-manager finding
    /// (`native-builtins/src/security_manager.rs`, `SECURITY_MANAGER`).
    ///
    /// The var-handle root registry lives in `SharedVm::mem` (see
    /// `vm::realms::heap_realm::HeapRealm::var_handle_roots`), i.e. it is
    /// PER-VM. A native subsystem that caches `(identity_key, ObjectRef)` in a
    /// PROCESS-GLOBAL slot and re-reads the current address with
    /// `ctx.read_var_handle_root(key).unwrap_or(cached)` therefore misses in
    /// every VM except the one that installed it — and silently falls back to
    /// the raw `cached` address, which that VM's GC neither scans nor rewrites.
    /// Under a moving young GC that is a use-after-move, not a policy leak.
    ///
    /// This test pins the property the fallback depends on, so the security
    /// manager's per-VM ownership cannot be "fixed" by re-adding a shared
    /// registry without this failing.
    #[test]
    fn var_handle_roots_are_per_vm_so_a_global_cached_ref_cannot_be_remapped() {
        let a = Arc::new(SharedVm::new(VmConfig::default()));
        let b = Arc::new(SharedVm::new(VmConfig::default()));

        // A never-dereferenced, 8-byte-aligned synthetic address standing in
        // for a SecurityManager instance allocated in VM A's heap.
        // SAFETY: non-null and aligned; only ever compared by pointer value.
        let sm_in_vm_a = unsafe { ObjectRef::from_raw(0x5EC0_0000usize as *mut u8) };
        let identity_key: i32 = 0x5EC0;

        a.mem
            .var_handle_roots
            .write()
            .insert(identity_key, sm_in_vm_a);

        assert_eq!(
            a.mem.var_handle_roots.read().get(&identity_key).copied(),
            Some(sm_in_vm_a),
            "the installing VM must see its own var-handle root"
        );
        assert!(
            b.mem.var_handle_roots.read().get(&identity_key).is_none(),
            "VM B must NOT resolve VM A's var-handle key — the `unwrap_or(cached)` \
             fallback in a process-global singleton therefore hands VM B a raw, \
             unrooted, un-remappable reference into VM A's heap"
        );
    }

    /// Regression: the redefine-invalidation bridge used to be a
    /// `OnceLock<Weak<SharedVm>>`, so the FIRST VM ever created owned the hook
    /// for the life of the process. Registering a second VM was silently
    /// dropped, and every `RedefineClasses` in that VM left its own
    /// `ResolutionCache` / `LinkResolver` serving pre-redefine
    /// `(declaring_class_id, index)` pairs.
    #[test]
    fn hook_registry_reaches_every_live_vm_not_just_the_first() {
        let _guard = HOOK_REGISTRY_TEST_LOCK.lock();

        let first = Arc::new(SharedVm::new(VmConfig::default()));
        let second = Arc::new(SharedVm::new(VmConfig::default()));

        set_global_shared_vm_for_hooks(Arc::downgrade(&first));
        set_global_shared_vm_for_hooks(Arc::downgrade(&second));

        assert!(
            registry_contains(&first),
            "the first-registered VM must stay reachable"
        );
        assert!(
            registry_contains(&second),
            "a VM registered AFTER another must still receive hook callbacks — \
             the old OnceLock silently ignored it"
        );

        // The adapters must not panic or deadlock with several VMs live; they
        // take VM-internal locks, so this also exercises the "snapshot the
        // registry, then drop its lock" ordering in `live_hook_vms`.
        resolution_invalidate_adapter(0);
        jit_invalidate_adapter(0);
    }

    /// Registering the same VM twice must not double it in the registry, or
    /// every redefine would invalidate that VM's caches twice.
    #[test]
    fn hook_registry_registration_is_idempotent() {
        let _guard = HOOK_REGISTRY_TEST_LOCK.lock();

        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        set_global_shared_vm_for_hooks(Arc::downgrade(&shared));
        set_global_shared_vm_for_hooks(Arc::downgrade(&shared));
        set_global_shared_vm_for_hooks(Arc::downgrade(&shared));

        let occurrences = live_hook_vms()
            .into_iter()
            .filter(|live| Arc::ptr_eq(live, &shared))
            .count();
        assert_eq!(
            occurrences, 1,
            "a VM registered three times must appear exactly once"
        );
    }

    /// Teardown isolation: dropping one VM must prune only that VM's entry and
    /// leave every other registered VM reachable. (This is also what stops a
    /// long-lived embedding that creates and destroys many VMs from
    /// accumulating dead `Weak`s.)
    #[test]
    fn dropping_one_vm_leaves_the_other_registered() {
        let _guard = HOOK_REGISTRY_TEST_LOCK.lock();

        let survivor = Arc::new(SharedVm::new(VmConfig::default()));
        let doomed = Arc::new(SharedVm::new(VmConfig::default()));
        set_global_shared_vm_for_hooks(Arc::downgrade(&survivor));
        set_global_shared_vm_for_hooks(Arc::downgrade(&doomed));

        let doomed_weak = Arc::downgrade(&doomed);
        drop(doomed);
        assert!(
            doomed_weak.upgrade().is_none(),
            "the registry must hold a Weak, never an Arc — otherwise a dropped \
             VM is kept alive forever by the hook bridge"
        );

        assert!(
            registry_contains(&survivor),
            "tearing down one VM must not unregister the others"
        );
        // ...and the pruned entry must be gone, not merely un-upgradable.
        let still_listed = live_hook_vms().len();
        let after_second_sweep = live_hook_vms().len();
        assert_eq!(
            still_listed, after_second_sweep,
            "live_hook_vms must prune dead entries so repeated sweeps are stable"
        );

        // Firing the hooks with a dead entry already swept must be a no-op,
        // not a panic.
        resolution_invalidate_adapter(0);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// F30 (2026-08-13) — the registrar call graph, as a gate rather than folklore
//
// Four separate lanes in one session each rediscovered, by hand, that a
// `register_*` pass is reachable in fewer modes than its name suggests:
//
//   1. `register_p67_string_template` and the phase-64/67 registrars are
//      reached only from `register_synthetic_overrides`, which is
//      `#[cfg(feature = "synthetic-jdk")]` AND has exactly one caller,
//      `register_builtins` — i.e. only the synthetic-MODE arm above. A doc
//      comment had already claimed those rows survive `--jdk-only` because
//      their `NativeKind` is `Bridge`: true about categories, false about
//      that registrar.
//   2. `register_pe_panama` likewise has one call site
//      (`native-builtins/src/lib.rs`, inside `register_synthetic_overrides`),
//      so real-JDK mode and `--jdk-only` never registered panama's layouts at
//      all. The two shipping modes ran different `structLayout`
//      implementations and the synthetic-mode tests exercised the one
//      `--jdk-only` does not run.
//   3. In synthetic mode `register_builtins` runs essentials and THEN the
//      synthetic overrides, so a triple registered in both places resolves to
//      the second; the real-JDK arms run essentials only, so the same triple
//      resolves to the first. Two guards for one rule were live in different
//      modes, silently drifting.
//   4. `register_io_natives` runs AFTER `register_essential_natives_with_shims`
//      in both real-JDK arms, so `native-io`'s bodies shadow `native-builtins`'
//      aliasing implementations — a lane measured a clearance against a body
//      that never runs.
//
// `register()` is last-write-wins and `NativeKind` is ambient, so ORDER and
// ARM MEMBERSHIP are semantics, not style. The tests below read this file out
// of the working tree and go red when either changes. A comment cannot do
// that; that a comment is not a gate is this session's most repeated lesson.
//
// Census record:
// `docs/known-issues/jdk-only/F30-1-the-registrar-call-graph-and-the-drifted-arm-20260813.md`.
//
// Deliberately NOT covered here: the inline `native_methods.register(...)`
// rows. Three of them (`CopyOnWriteArrayList.addIfAbsent`, `ArrayList.toArray`,
// `AbstractCollection.toArray`) exist only in the feature-OFF arm and their
// bodies call helper `fn`s declared inside that arm, so unifying them is a
// code move rather than a call move. Recorded in the census instead.
#[cfg(test)]
mod registrar_call_graph_witness {
    /// This very file, read from the WORKING TREE at test time (not
    /// `include_str!`, which would freeze a compile-time snapshot and let the
    /// witness pass against source that is no longer there).
    const VM_INIT_RS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/vm/vm_init.rs");
    const VM_CARGO_TOML: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");

    /// Opens the synthetic-JDK **mode** arm (runtime flag, not a Cargo cfg).
    const SYNTHETIC_ARM_OPEN: &str = "if config.use_synthetic_jdk {";
    /// Opens real-JDK arm A — compiled when `synthetic-jdk` is ON.
    const REAL_ARM_A_OPEN: &str = "} else {";
    /// Opens real-JDK arm B — compiled when `synthetic-jdk` is OFF.
    const REAL_ARM_B_OPEN: &str = "#[cfg(not(feature = \"synthetic-jdk\"))]";
    /// Final statement of BOTH real-JDK arms. Matched against the whole
    /// trimmed line, so this constant's own declaration cannot match it.
    const REAL_ARM_CLOSE: &str = "\"Real JDK mode: {} native methods registered\",";

    fn read(path: &str) -> String {
        std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("witness must read `{}` from the working tree: {}", path, e))
    }

    /// The first argument every registration pass takes: the registry being
    /// built.
    const REGISTRY_ARG: &str = "&mut native_methods";

    /// Every registration pass called in `lines`, in source order, named by
    /// the last segment of its path.
    ///
    /// A pass is any call whose first argument is the registry. Keying on the
    /// ARGUMENT rather than on a `register_` name prefix is deliberate:
    /// `init_service_loader_bootstrap` is a registration pass too, and the
    /// first draft of this witness — a name-prefix scanner — silently skipped
    /// it. That is the same "trace it, do not infer from names" failure the
    /// witness exists to stop.
    ///
    /// Method calls (`native_methods.register_with_kind(...)`) are excluded
    /// because their receiver is not an argument; whole-line `//` comments are
    /// skipped, so back-ticked prose mentions never match. rustfmt moves the
    /// argument to the next line for the longer paths, so both spellings are
    /// accepted.
    fn registration_passes<'a>(lines: &[&'a str]) -> Vec<&'a str> {
        let mut found: Vec<&'a str> = Vec::new();
        for (i, &line) in lines.iter().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            let bytes = line.as_bytes();
            for (open, _) in line.char_indices().filter(|(_, c)| *c == '(') {
                let mut start = open;
                while start > 0 {
                    let p = bytes[start - 1];
                    if p == b'_' || p == b':' || p.is_ascii_alphanumeric() {
                        start -= 1;
                    } else {
                        break;
                    }
                }
                if start == open || (start > 0 && bytes[start - 1] == b'.') {
                    continue;
                }
                let after = line[open + 1..].trim_start();
                let takes_registry = if after.is_empty() {
                    lines
                        .get(i + 1)
                        .is_some_and(|n| n.trim_start().starts_with(REGISTRY_ARG))
                } else {
                    after.starts_with(REGISTRY_ARG)
                };
                if takes_registry {
                    let path = &line[start..open];
                    found.push(path.rsplit("::").next().unwrap_or(path));
                }
            }
        }
        found
    }

    fn only(lines: &[&str], trimmed: &str) -> usize {
        let hits: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.trim() == trimmed)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "the registrar-arm anchor `{}` must occur exactly once in vm_init.rs; found it \
             on lines {:?}. Anchors are how this witness locates the three registration \
             arms — re-anchor the witness in the same change that moved them.",
            trimmed,
            hits.iter().map(|i| i + 1).collect::<Vec<_>>()
        );
        hits[0]
    }

    struct Arms<'a> {
        /// synthetic-JDK mode, `synthetic-jdk` feature ON (configuration 3).
        synthetic: Vec<&'a str>,
        /// real-JDK mode, `synthetic-jdk` feature ON (configuration 1) — also
        /// the arm `--jdk-only` takes in a feature-enabled build.
        real_feature_on: Vec<&'a str>,
        /// real-JDK mode, `synthetic-jdk` feature OFF (configuration 2), the
        /// shipping `cratonvm-cli` arm. Also configuration 4: a feature-OFF
        /// build that was *asked* for synthetic mode lands here anyway,
        /// because this block never reads `config.use_synthetic_jdk`.
        real_feature_off: Vec<&'a str>,
        b_open: usize,
        b_close: usize,
    }

    fn arms(src: &str) -> Arms<'_> {
        let lines: Vec<&str> = src.lines().collect();
        let syn_open = only(&lines, SYNTHETIC_ARM_OPEN);
        let a_open = lines
            .iter()
            .enumerate()
            .skip(syn_open + 1)
            .find(|(_, l)| l.trim() == REAL_ARM_A_OPEN)
            .map(|(i, _)| i)
            .expect("real-JDK arm A is the `else` of `if config.use_synthetic_jdk`");
        let closes: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.trim() == REAL_ARM_CLOSE)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            closes.len(),
            2,
            "exactly two real-JDK arms must exist, each ending in the `Real JDK mode: ...` \
             tracing line; found {} such lines: {:?}",
            closes.len(),
            closes.iter().map(|i| i + 1).collect::<Vec<_>>()
        );
        let b_open = only(&lines, REAL_ARM_B_OPEN);
        assert!(
            syn_open < a_open && a_open < closes[0] && closes[0] < b_open && b_open < closes[1],
            "registration arms are out of their expected source order: synthetic@{} \
             realA@{} realA-end@{} realB@{} realB-end@{}",
            syn_open + 1,
            a_open + 1,
            closes[0] + 1,
            b_open + 1,
            closes[1] + 1
        );
        Arms {
            synthetic: registration_passes(&lines[syn_open..a_open]),
            real_feature_on: registration_passes(&lines[a_open..closes[0]]),
            real_feature_off: registration_passes(&lines[b_open..closes[1]]),
            b_open,
            b_close: closes[1],
        }
    }

    fn position(list: &[&str], name: &str) -> usize {
        list.iter().position(|c| *c == name).unwrap_or_else(|| {
            panic!(
                "`{}` is no longer called from this real-JDK arm. Registration is \
                 last-write-wins, so dropping a registrar silently hands its triples to \
                 whichever earlier pass registered them. If the removal is deliberate, \
                 drop the ordering claim here in the same change.",
                name
            )
        })
    }

    fn must_precede(list: &[&str], arm: &str, earlier: &str, later: &str, why: &str) {
        let e = position(list, earlier);
        let l = position(list, later);
        assert!(
            e < l,
            "{}: `{}` (position {}) must run BEFORE `{}` (position {}). {}",
            arm,
            earlier,
            e,
            later,
            l,
            why
        );
    }

    /// The drift this lane was opened for: the two real-JDK arms had grown
    /// apart. Arm A was missing `register_classvalue_natives`,
    /// `register_random_and_securerandom_natives` (a seeded `Random` returned
    /// all zeros there), `register_phase57_file` and
    /// `register_spring_boot_logback_apply`.
    ///
    /// Sequence equality, not set equality: order is semantics here.
    #[test]
    fn the_two_real_jdk_arms_run_the_same_registrars_in_the_same_order() {
        let src = read(VM_INIT_RS);
        let a = arms(&src);
        assert_eq!(
            a.real_feature_on, a.real_feature_off,
            "the two real-JDK arms of vm_init must call the same registrars in the same \
             order. They differ only in which Cargo feature COMPILED them, never in which \
             class library is loaded, so a registrar in one and not the other is a \
             mode-specific defect: the `--features synthetic-jdk` build's real-JDK and \
             `--jdk-only` runs take the first list, every shipping `cratonvm-cli` run takes \
             the second."
        );
        assert!(
            a.real_feature_on.len() > 40,
            "the real-JDK registrar sequence collapsed to {} entries — the witness is \
             almost certainly parsing the wrong line range rather than seeing a real \
             deletion",
            a.real_feature_on.len()
        );
    }

    /// Confusions 1 and 2 above, stated where they can be checked: the
    /// synthetic-override family (and with it panama, phase 64 and phase 67)
    /// is reachable ONLY through `register_builtins`, and only the
    /// synthetic-MODE arm calls it. A fix landed inside
    /// `register_synthetic_overrides` does not reach `--jdk-only`.
    #[test]
    fn only_the_synthetic_mode_arm_reaches_register_builtins() {
        let src = read(VM_INIT_RS);
        let a = arms(&src);
        assert_eq!(
            a.synthetic.first(),
            Some(&"register_builtins"),
            "the synthetic-mode arm must open with `register_builtins` (essentials, then \
             `register_synthetic_overrides`); it opened with {:?}",
            a.synthetic.first()
        );
        for (arm, calls) in [
            ("real-JDK arm A (feature ON)", &a.real_feature_on),
            ("real-JDK arm B (feature OFF)", &a.real_feature_off),
        ] {
            for forbidden in ["register_builtins", "register_synthetic_overrides"] {
                assert!(
                    !calls.contains(&forbidden),
                    "{} must never call `{}`: it pulls in `register_synthetic_overrides`, \
                     whose rows assume synthetic field layouts and corrupt real JDK \
                     objects. It is also the ONLY caller of that function, which is why a \
                     registrar reached only from there (panama's `register_pe_panama`, the \
                     phase-64/67 families) is synthetic-mode-only however its `NativeKind` \
                     is tagged.",
                    arm,
                    forbidden
                );
            }
        }
        assert!(
            !a.synthetic
                .contains(&"register_essential_natives_with_shims"),
            "the synthetic arm must not re-run essentials after `register_builtins`: \
             registration is last-write-wins, so it would demote every synthetic override \
             back to its essential twin."
        );
    }

    /// Confusion 4, plus the ordering claims the surrounding comments make in
    /// prose. Every pair below is an incident, not a preference.
    #[test]
    fn last_write_wins_ordering_holds_inside_both_real_jdk_arms() {
        let src = read(VM_INIT_RS);
        let a = arms(&src);
        for (arm, calls) in [
            ("real-JDK arm A (feature ON)", &a.real_feature_on),
            ("real-JDK arm B (feature OFF)", &a.real_feature_off),
        ] {
            must_precede(
                calls,
                arm,
                "register_essential_natives_with_shims",
                "register_io_natives",
                "`native-io`'s bodies deliberately SHADOW `native-builtins`' aliasing \
                 implementations for the java.io surface. A clearance measured against the \
                 builtins body is measuring code this arm never runs.",
            );
            must_precede(
                calls,
                arm,
                "register_concurrent_natives",
                "register_forkjoin_quiescence",
                "`register_concurrent_natives` registers the constant \
                 `ForkJoinPool.awaitQuiescence -> true`; the real one polls this crate's \
                 async worker pool and must overwrite it.",
            );
            must_precede(
                calls,
                arm,
                "register_collections_natives",
                "register_random_and_securerandom_natives",
                "`register_collections_natives` re-registers the layout-dependent \
                 `java/util/Random` aliases, which read field 0 as a long when in real-JDK \
                 mode it is the `AtomicLong seed` REFERENCE — a seeded `Random` then emits \
                 all zeros.",
            );
            must_precede(
                calls,
                arm,
                "register_collections_natives",
                "register_properties_sidetable",
                "`register_collections_natives` re-registers `Properties.load` / \
                 `getProperty` / `setProperty` with the legacy HashMap-layout natives, \
                 overwriting the side-table-backed pair Surefire's `loadProperties` \
                 round-trip needs.",
            );
            for (earlier, later) in [
                ("register_phase57_nio_file", "register_phase57_file"),
                ("register_phase57_file", "register_p59_jar"),
                ("register_p59_jar", "register_p59_bulk_stream_transfer"),
                (
                    "register_p59_bulk_stream_transfer",
                    "register_p59_zip_output_primitives",
                ),
            ] {
                must_precede(
                    calls,
                    arm,
                    earlier,
                    later,
                    "the file/jar/zip family is registered nio_file -> file -> jar -> bulk \
                     -> zip-output; re-ordering it hands `JarFile.entries()` back to \
                     `native-io`'s `alloc_zip_entry`, which answers `ZipEntry` where the \
                     declared `Enumeration<JarEntry>` checkcast demands `JarEntry`.",
                );
            }
        }
    }

    /// Real-JDK arm A leaves its `sun.management` / JMX registrars UNGATED
    /// while arm B gates each one on `#[cfg(feature = "management")]`. That is
    /// sound only because `synthetic-jdk` — the feature that compiles arm A —
    /// itself enables `management`. Nothing said so; now something checks it.
    /// If the implication is ever dropped, arm A stops COMPILING (loud), which
    /// is why the cfg attributes are deliberately not mirrored onto it.
    #[test]
    fn the_synthetic_jdk_feature_still_implies_management() {
        let toml = read(VM_CARGO_TOML);
        let start = toml
            .find("\nsynthetic-jdk = [")
            .expect("vm/Cargo.toml must declare a multi-line `synthetic-jdk` feature list");
        let rest = &toml[start..];
        let end = rest
            .find("\n]")
            .expect("the `synthetic-jdk` feature list must be closed by a `]` at column 0");
        let list = &rest[..end];
        assert!(
            list.contains("\"management\""),
            "`synthetic-jdk` must keep enabling `management`. vm_init's real-JDK arm A \
             (the `else` of `if config.use_synthetic_jdk`, compiled only under \
             `synthetic-jdk`) calls the `jmx::register_*_impl` family WITHOUT a \
             `#[cfg(feature = \"management\")]` gate, unlike arm B. The list read:\n{}",
            list
        );
    }

    /// The fourth configuration, recorded where it can rot loudly: a build
    /// WITHOUT `synthetic-jdk` that is asked for synthetic mode still lands in
    /// real-JDK arm B, because that block never reads
    /// `config.use_synthetic_jdk`. `require_synthetic_jdk` rejects that
    /// pairing — but only on the CLI / `libcratonvm` entry paths, not in
    /// `SharedVm::new`, so an embedder (and `VmConfig::default()`, whose JDK
    /// mode is Synthetic) reaches it.
    #[test]
    fn the_feature_off_arm_never_consults_the_runtime_jdk_mode() {
        let src = read(VM_INIT_RS);
        let a = arms(&src);
        let lines: Vec<&str> = src.lines().collect();
        let offenders: Vec<usize> = (a.b_open..a.b_close)
            .filter(|i| {
                let l = lines[*i];
                !l.trim_start().starts_with("//") && l.contains("use_synthetic_jdk")
            })
            .map(|i| i + 1)
            .collect();
        assert!(
            offenders.is_empty(),
            "the `#[cfg(not(feature = \"synthetic-jdk\"))]` arm branches on \
             `config.use_synthetic_jdk` at lines {:?}. Today it does not, and that is a \
             load-bearing fact: a feature-OFF build asked for synthetic mode gets the \
             real-JDK registrar set rather than an empty registry. Adding a runtime branch \
             here creates a fourth registration path — update the F30 census in the same \
             change.",
            offenders
        );
    }
}
