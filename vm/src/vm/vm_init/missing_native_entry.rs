#[allow(unused_imports)]
use super::*;

// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// VM construction and core structs: SharedVm and Vm.

/// Maximum number of lambda proxy classes before new registrations are silently dropped.
pub(crate) const MAX_LAMBDA_PROXIES: usize = 100_000;

// `NEXT_VM_IDENTITY`, the process counter `SharedVm::vm_identity` was drawn
// from (starting at 1), was retired in interpreter round i1 wave 21 (lane L5):
// a VM's identity is now derived from its class store's layout domain
// ([`vm_identity_for_layout_domain`]). The properties comments elsewhere cite
// it for still hold: identities start at 1 (0 is "no VM"), are unique per
// live VM, and are never reused.

/// A VM's `vm_identity` (and so its metadata-realm `VmId`) from its class
/// store's layout domain: `domain + 1`, so 0 stays "no VM" (`UNBOUND_VM`,
/// `UNATTRIBUTED_VM`).
///
/// One token per VM (stage 2 of
/// `i18-L5-proposal-one-owner-token-for-every-classloading-hook`): the heap,
/// the `StoreEpochs` slot, the classloading hooks' owner token and the hook
/// registry name a VM by its layout domain, and the vtable routing, the
/// capability policy and every `vm_identity`-keyed table by this number; the
/// two are now the same number, off by one. Layout domains come from one
/// process counter (`cratonvm_types::next_layout_domain`), one per
/// `ClassStore`, and every `SharedVm::new` builds its own store, so two live
/// VMs never share one and a dropped VM's identity is never handed out again.
#[inline]
pub(crate) const fn vm_identity_for_layout_domain(layout_domain: u32) -> usize {
    layout_domain as usize + 1
}


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
pub(in crate::vm::vm_init) fn truncate_ascii(s: &str, max: usize) -> String {
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
pub(in crate::vm::vm_init) fn quarkus_runner_class_loader_close(
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
pub(in crate::vm::vm_init) fn canonical_os_name() -> String {
    #[cfg(target_os = "windows")]
    {
        // Interpreter round i7 wave 138, lane L1b
        // (`CRATONVM_WINDOWS_OS_VERSION_FROM_OS`, parity, default ON, `=0`
        // the old constant build 22000): the real version, named as HotSpot
        // 25's `java_props_md.c` names it (workstation vs server).
        if windows_os_version_from_os_enabled() {
            if let Some(v) = windows_os_version() {
                return windows_os_name_for(v.major, v.minor, v.build, v.workstation).to_string();
            }
        }
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
pub(in crate::vm::vm_init) fn canonical_os_arch() -> String {
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
pub(in crate::vm::vm_init) fn canonical_os_version() -> String {
    #[cfg(target_os = "windows")]
    {
        // Wave 138, L1b: HotSpot's `"%d.%d"` of the major and minor version
        // (`10.0` on Windows 10 / 11 / Server 2016+, `6.3` on 8.1 / 2012 R2).
        if windows_os_version_from_os_enabled() {
            if let Some(v) = windows_os_version() {
                return format!("{}.{}", v.major, v.minor);
            }
        }
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
        // Interpreter round i7 wave 139, lane L1b (page
        // `i138-L1b-windows-os-name-is-always-windows-11`, macOS part): the
        // product version (`15.3.1`), as HotSpot's `NSProcessInfo
        // operatingSystemVersion` answers it; `sysctl kern.osproductversion`
        // (macOS 10.13.4+) is the same string. The old constant `14.0` stays
        // the fallback. Not run on macOS by the lane.
        if let Some(v) = macos_product_version() {
            return v;
        }
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
pub(in crate::vm::vm_init) fn windows_build_number() -> Option<u32> {
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

/// `kern.osproductversion` (wave 139, lane L1b): the macOS product version,
/// or `None` when the sysctl is missing or empty.
#[cfg(target_os = "macos")]
pub(in crate::vm::vm_init) fn macos_product_version() -> Option<String> {
    let mut buf = [0u8; 64];
    let mut len: libc::size_t = buf.len();
    // SAFETY: a NUL-terminated name, a buffer of `len` bytes and its length;
    // `sysctlbyname` writes at most `len` bytes and updates `len`.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.osproductversion".as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len == 0 {
        return None;
    }
    let text = std::str::from_utf8(&buf[..len.min(buf.len())]).ok()?;
    let text = text.trim_end_matches('\0').trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// `CRATONVM_WINDOWS_OS_VERSION_FROM_OS` (interpreter round i7 wave 138,
/// lane L1b; HotSpot parity, default ON, `=0` the old constants): `os.name`
/// and `os.version` from the running Windows (`RtlGetVersion`), not from the
/// constant build 22000 that named every host -- Windows 10 and every
/// Windows Server included -- `Windows 11`. Read once per VM, at property
/// seeding. A `CRATONVM_FORCE_WIN_BUILD` test override keeps the old path.
#[cfg(target_os = "windows")]
pub(in crate::vm::vm_init) fn windows_os_version_from_os_enabled() -> bool {
    cratonvm_types::flags::runtime_var("CRATONVM_FORCE_WIN_BUILD").is_err()
}

/// The running Windows version, as `java_props_md.c` reads it (major, minor,
/// build, and `wProductType == VER_NT_WORKSTATION`).
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::vm::vm_init) struct WindowsOsVersion {
    pub(in crate::vm::vm_init) major: u32,
    pub(in crate::vm::vm_init) minor: u32,
    pub(in crate::vm::vm_init) build: u32,
    pub(in crate::vm::vm_init) workstation: bool,
}

/// `RtlGetVersion` (ntdll): the true version whatever the executable's
/// manifest says (`GetVersionEx` lies to an unmanifested process). HotSpot
/// reads the major / minor / build from `kernel32.dll`'s product version for
/// the same reason; the two agree on the major and minor, and on the build to
/// the precision the name table needs. `None` on failure.
#[cfg(target_os = "windows")]
pub(in crate::vm::vm_init) fn windows_os_version() -> Option<WindowsOsVersion> {
    #[repr(C)]
    #[allow(dead_code)]
    struct RtlOsVersionInfoExW {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform_id: u32,
        csd_version: [u16; 128],
        service_pack_major: u16,
        service_pack_minor: u16,
        suite_mask: u16,
        product_type: u8,
        reserved: u8,
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut RtlOsVersionInfoExW) -> i32;
    }
    const VER_NT_WORKSTATION: u8 = 1;
    // SAFETY: the struct is plain integers and arrays (all-zero is valid);
    // `RtlGetVersion` writes at most `size` bytes of it and returns an
    // NTSTATUS (0 = success).
    unsafe {
        let mut info: RtlOsVersionInfoExW = std::mem::zeroed();
        info.size = std::mem::size_of::<RtlOsVersionInfoExW>() as u32;
        if RtlGetVersion(&mut info) != 0 || info.major == 0 {
            return None;
        }
        Some(WindowsOsVersion {
            major: info.major,
            minor: info.minor,
            build: info.build,
            workstation: info.product_type == VER_NT_WORKSTATION,
        })
    }
}

/// HotSpot 25's Windows `os.name` table (`java_props_md.c`, the
/// `VER_PLATFORM_WIN32_NT` arm), for NT 6 and later. Pure, all targets (so a
/// unit test runs everywhere).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(in crate::vm::vm_init) fn windows_os_name_for(major: u32, minor: u32, build: u32, workstation: bool) -> &'static str {
    match (major, minor, workstation) {
        (6, 0, true) => "Windows Vista",
        (6, 1, true) => "Windows 7",
        (6, 2, true) => "Windows 8",
        (6, 3, true) => "Windows 8.1",
        (6, 0, false) => "Windows Server 2008",
        (6, 1, false) => "Windows Server 2008 R2",
        (6, 2, false) => "Windows Server 2012",
        (6, 3, false) => "Windows Server 2012 R2",
        // Windows 11 21H2 (the original release) is build 22000.
        (10, 0, true) if build >= 22000 => "Windows 11",
        (10, 0, true) => "Windows 10",
        // Server 2019 GA is build 17763, 2022 is 20348, 2025 is 26100
        // (HotSpot's thresholds: `> 26039`, `> 20347`, `> 17762`).
        (10, 0, false) if build > 26039 => "Windows Server 2025",
        (10, 0, false) if build > 20347 => "Windows Server 2022",
        (10, 0, false) if build > 17762 => "Windows Server 2019",
        (10, 0, false) => "Windows Server 2016",
        _ => "Windows NT (unknown)",
    }
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
        // `java_props_md.c` substitutes `en_US` only for the EXACT names `C`
        // and `POSIX` (`!strcmp(lc, "C")`). `C.UTF-8` / `C.utf8` get past that
        // test and are parsed: language `C` maps to `en` and there is no
        // country, so HotSpot 25 reports `en` with `user.country` unset
        // (measured, LANG=C.UTF-8 on the linux audit host).
        let exact = head.eq_ignore_ascii_case("C") || head.eq_ignore_ascii_case("POSIX");
        let bare = exact && raw.len() == head.len();
        return LocaleSubtags {
            language: "en".to_string(),
            country: if bare {
                "US".to_string()
            } else {
                String::new()
            },
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
pub(in crate::vm::vm_init) fn derive_host_locale() -> HostLocale {
    if let Some(host) = platform_host_locale() {
        return host;
    }

    // SBR wave 4 LOCALE2 (2026-09-29): the NAME the environment asks for is
    // not what HotSpot reports. `java_props_md.c` calls `setlocale(LC_ALL, "")`
    // and parses what `setlocale(<category>, NULL)` then answers, and glibc's
    // `LC_ALL` call is all-or-nothing: if ANY category names a locale that is
    // not installed it fails, the process stays in `C`, and the JDK maps `C`
    // to `en_US`. Measured on the linux audit host (only `C`, `C.utf8`,
    // `POSIX`, `en_US.utf8` installed): HotSpot 25 reports `en_US` for
    // `LANG=de_DE.UTF-8`, `fr_FR`, `xx_YY.UTF-8` and `en`, where this function
    // used to report `de_DE`, `fr_FR`, `xx_YY` and `en`.
    let (display, format) = if posix_env_locales_loadable() {
        (
            posix_locale_category("LC_MESSAGES"),
            posix_locale_category("LC_CTYPE"),
        )
    } else {
        ("C".to_string(), "C".to_string())
    };
    HostLocale {
        display: parse_locale_name(&display),
        format: parse_locale_name(&format),
    }
}

/// Would `setlocale(LC_ALL, "")` succeed on this host with this environment?
///
/// It fails -- leaving every category in `C` -- when any category resolves
/// (`LC_ALL` ▸ `LC_<category>` ▸ `LANG`) to a locale glibc cannot load. Asked
/// with `newlocale`, which builds a private locale object and touches no
/// process-global state, so this keeps the promise the `platform_host_locale`
/// doc makes about `setlocale`. An empty name selects `C`, which always loads.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(in crate::vm::vm_init) fn posix_env_locales_loadable() -> bool {
    const CATEGORIES: [&str; 12] = [
        "LC_CTYPE",
        "LC_NUMERIC",
        "LC_TIME",
        "LC_COLLATE",
        "LC_MONETARY",
        "LC_MESSAGES",
        "LC_PAPER",
        "LC_NAME",
        "LC_ADDRESS",
        "LC_TELEPHONE",
        "LC_MEASUREMENT",
        "LC_IDENTIFICATION",
    ];
    CATEGORIES.iter().all(|cat| {
        let name = posix_locale_category(cat);
        let name = name.trim();
        if name.is_empty() {
            return true;
        }
        let Ok(c_name) = std::ffi::CString::new(name) else {
            return false;
        };
        // SAFETY: `newlocale` with a null base allocates a fresh locale object
        // (or returns null); the object is freed before returning and nothing
        // else sees it.
        unsafe {
            let loc = libc::newlocale(libc::LC_ALL_MASK, c_name.as_ptr(), std::ptr::null_mut());
            if loc.is_null() {
                false
            } else {
                libc::freelocale(loc);
                true
            }
        }
    })
}

/// Hosts without glibc's `newlocale`: keep reading the environment's names as
/// before (no measurement says what HotSpot does there).
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(in crate::vm::vm_init) fn posix_env_locales_loadable() -> bool {
    true
}

/// Resolve ONE POSIX locale category the way `setlocale(<category>, "")` does:
/// `LC_ALL` ▸ the category's own variable ▸ `LANG`.
///
/// Lifted out of `derive_host_locale` with the precedence unchanged — it was
/// an inline `pick` closure there — when [`derive_native_encoding`] needed the same precedence
/// for `LC_CTYPE`. A second copy is precisely how the locale answer and the
/// encoding answer would drift apart on a host that sets only `LC_CTYPE`, or
/// only `LC_ALL`: the two shapes this precedence exists to get right.
///
/// Returns the RAW locale name, `.codeset` suffix and `@modifier` included.
/// `parse_locale_name` discards both because a language tag has neither.
///
/// It had a second caller for one day: a `native_encoding_from_locale_name`
/// that read the codeset out of this string. That was replaced on 2026-09-01 by
/// `cratonvm_native_api::os_encoding`, which asks libc instead — a locale NAME
/// cannot tell an installed locale from a merely requested one, and HotSpot
/// reports what `setlocale` actually gave it.
pub(in crate::vm::vm_init) fn posix_locale_category(category: &str) -> String {
    let env = |name: &str| cratonvm_types::flags::runtime_var(name).unwrap_or_default();
    let lc_all = env("LC_ALL");
    if !lc_all.trim().is_empty() {
        return lc_all;
    }
    let own = env(category);
    if !own.trim().is_empty() {
        return own;
    }
    env("LANG")
}

/// The charset the host announces, for the `native.encoding` property.
///
/// # Why this is derived and not a constant
///
/// Until 2026-09-01 both of CratonVM's property tables answered the literal
/// `"UTF-8"` under a comment reading "Encodings — JDK 18+ pinned to UTF-8 for
/// stdout/stderr/file/native". **That premise was false.** Read from the JDK
/// 25 sources on the audit host (`lib/src.zip`), not from memory:
/// `jdk/internal/util/SystemProps` *assigns* `file.encoding = "UTF-8"` — that
/// is JEP 400's pin, and it is the only one — and then does an unconditional
/// `put` of `native.encoding` from the value the platform native code
/// computed. `java.lang.System`'s own property table specifies
/// `native.encoding` as derived from the host environment and says setting it
/// on the command line has no effect. A constant satisfies neither clause, so
/// this key — unlike `stdout.encoding`, which is a compatibility judgement —
/// was simply non-conforming. See
/// docs/feature-designs/real-jdk-mode.md.
///
/// # What this deliberately does NOT do
///
/// * **It does not move `stdout.encoding`, `stderr.encoding`,
///   `stdin.encoding`, `file.encoding` or `sun.jnu.encoding`**, nor the
///   charset stamped on `System.out`. In real-JDK mode that is not an accident
///   of ordering: `SystemProps.initProperties` reaches for `native.encoding`
///   only as a `putIfAbsent` FALLBACK for the three stream keys, and all three
///   are already present in `native-builtins/src/system_bootstrap.rs`'s
///   `vmProperties()` table, so this change cannot reach them. Whether
///   `System.out` should follow the console is the judgement the page above
///   declines to make; `sun.jnu.encoding` is riskier still, because it decides
///   how FILE NAMES are encoded and moving it would change class loading
///   rather than printing.
/// * **It does not move `sun.jnu.encoding`.** That key decides how FILE NAMES
///   are encoded, so moving it changes class loading rather than printing —
///   a different blast radius, and still deferred.
///
/// Two bullets that stood here for one day are gone, both closed by
/// `cratonvm_native_api::os_encoding` on 2026-09-01: this DOES read a Windows
/// code page now (`GetACP`, and the attached console's code page for a stream
/// that has one), and it DOES call `setlocale`/`nl_langinfo` rather than
/// parsing the locale NAME — for `LC_CTYPE` only, on one thread, restoring the
/// previous locale, which leaves the process-global state `derive_host_locale`
/// declines to touch exactly as it was found. The name-parse it replaced could
/// not tell an installed locale from a requested one: measured on the audit
/// host, `LANG=en_US.ISO-8859-1` makes HotSpot answer `ANSI_X3.4-1968`
/// (`setlocale` failed, so the process stayed in `C`) where the parse answered
/// `ISO-8859-1`.
pub(in crate::vm::vm_init) fn derive_native_encoding() -> String {
    // The A/B for this change, in one binary, on every platform:
    // `CRATONVM_NATIVE_ENCODING=UTF-8` restores the pre-2026-09-01 constant
    // exactly. Any other value is used verbatim. The default is derivation
    // rather than the constant because, unlike `stdout.encoding`, the
    // specification for this key is not ambiguous.
    if let Some(pin) = cratonvm_types::flags::runtime_var("CRATONVM_NATIVE_ENCODING")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        return pin;
    }
    platform_native_encoding()
}

/// The host-derived half of [`derive_native_encoding`], split out so the flag
/// check above reads as the one-line override it is.
///
/// `cfg!` rather than `#[cfg]` on purpose: both arms then type-check on both
/// platforms, so a Unix-side edit cannot silently break the Windows build that
/// nobody working on this can run.
pub(in crate::vm::vm_init) fn platform_native_encoding() -> String {
    // Delegated to `cratonvm_native_api::os_encoding`, which asks libc
    // (`setlocale(LC_CTYPE, "")` then `nl_langinfo(CODESET)`) on Unix and
    // `GetACP` on Windows, since 2026-09-01.
    //
    // It replaces a locale-NAME parse that lived here for one day and whose own
    // doc named both of its gaps: a name cannot tell a locale that is INSTALLED
    // from one that is merely requested (measured: `LANG=en_US.ISO-8859-1`
    // makes HotSpot answer `ANSI_X3.4-1968`, because `setlocale` failed and the
    // process stayed in `C`, where the parse answers `ISO-8859-1`), and the
    // Windows leg answered a constant `UTF-8` because no code page was read
    // anywhere in this tree. Both are closed there.
    //
    // The `setlocale` this was written to avoid is called once, on one thread,
    // for `LC_CTYPE` only, and the previous locale is restored — so the
    // process-global state `derive_host_locale` declines to touch is left
    // exactly as it was found.
    cratonvm_native_api::os_encoding::native_encoding().to_string()
}

/// The host's locales from a platform API, or `None` where there is no such
/// API and the environment is the only source.
///
/// Unix/macOS deliberately return `None` rather than calling `setlocale`:
/// `setlocale` mutates process-global state we do not otherwise touch, and the
/// `LC_ALL` ▸ category ▸ `LANG` precedence it would apply is the one the caller
/// reads directly.
#[cfg(target_os = "windows")]
pub(in crate::vm::vm_init) fn platform_host_locale() -> Option<HostLocale> {
    windows_host_locale()
}

#[cfg(not(target_os = "windows"))]
pub(in crate::vm::vm_init) fn platform_host_locale() -> Option<HostLocale> {
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
pub(in crate::vm::vm_init) fn windows_host_locale() -> Option<HostLocale> {
    // LOCALE_NAME_MAX_LENGTH.
    const LOCALE_NAME_MAX_LENGTH: usize = 85;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetUserDefaultLocaleName(lp_locale_name: *mut u16, cch_locale_name: i32) -> i32;
        fn GetUserDefaultUILanguage() -> u16;
        fn LCIDToLocaleName(locale: u32, lp_name: *mut u16, cch_name: i32, dw_flags: u32) -> i32;
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
pub(in crate::vm::vm_init) fn fill_i18n_props(
    sys_props: &mut HashMap<String, String>,
    cmdline: &[(String, String)],
    base: &str,
    display: &str,
    format: &str,
) {
    if cmdline.iter().any(|(k, _)| k == base) {
        return; // Rule 1: do not override, and do not derive the overlay.
    }
    // Which platform values count as "set" differs by host, and both halves
    // are measured rather than assumed. `SystemProps.fillI18nProps` writes a
    // key only for a NON-NULL platform value. The Windows platform layer hands
    // back `""` for an absent script/variant, so there all four keys appear,
    // empty string included. The Unix layer (`java_props_md.c`) hands back
    // NULL, so on linux HotSpot 25 reports `user.script`, `user.variant` -- and
    // `user.country` under `LANG=C.UTF-8` -- as null (SBR wave 4 LOCALE2,
    // 2026-09-29; this used to publish `""` for them on every host).
    let publish_empty = cfg!(target_os = "windows");
    if publish_empty || !display.is_empty() {
        sys_props.insert(base.to_string(), display.to_string());
    }
    if format != display && (publish_empty || !format.is_empty()) {
        sys_props.insert(format!("{base}.format"), format.to_string());
    }
}

/// `SystemProps.fillI18nProps` with the platform values NULLABLE, as
/// `SystemProps$Raw.platformProperties()` hands them over: a `None` display
/// value publishes nothing (the key is absent and `System.getProperty`
/// answers `null`), and `.format` is written when the format value is non-null
/// and differs from the base -- which includes "the base is null".
pub(in crate::vm::vm_init) fn fill_i18n_props_platform(
    sys_props: &mut HashMap<String, String>,
    cmdline: &[(String, String)],
    base: &str,
    display: Option<&str>,
    format: Option<&str>,
) {
    if cmdline.iter().any(|(k, _)| k == base) {
        return; // Do not override, and do not derive the overlay.
    }
    if let Some(display) = display {
        sys_props.insert(base.to_string(), display.to_string());
    }
    if let Some(format) = format {
        if Some(format) != display {
            sys_props.insert(format!("{base}.format"), format.to_string());
        }
    }
}

/// `CRATONVM_HOST_LOCALE_PARSELOCALE=0` restores the pre-round-16 Linux
/// derivation (`derive_host_locale` + `parse_locale_name`). Default ON, and
/// only ever on for Linux: `parse_locale_java_props_md` reproduces the LINUX
/// build of `java_props_md.c`; Windows has its own Win32 path and macOS reads
/// `CFLocale`.
pub(in crate::vm::vm_init) fn linux_parselocale_enabled() -> bool {
    cfg!(target_os = "linux")
        && !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0")
        )
}

/// One locale category as the Linux `java_props_md.c` `ParseLocale` reports it.
/// `None` is the C `NULL` it leaves in `sprops`: no property is published.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ParsedPosixLocale {
    pub language: String,
    pub script: Option<String>,
    pub country: Option<String>,
    pub variant: Option<String>,
}

/// `locale_str.h`'s `locale_aliases` (Linux build): partial names mapped to a
/// full one. The two Latin-1 spellings (`bokm\xE5l`, `fran\xE7ais`) are left
/// out: they cannot arrive through a UTF-8 environment read.
pub(in crate::vm::vm_init) const PARSELOCALE_ALIASES: &[(&str, &str)] = &[
    ("ar", "ar_EG"),
    ("be", "be_BY"),
    ("bg", "bg_BG"),
    ("br", "br_FR"),
    ("ca", "ca_ES"),
    ("cs", "cs_CZ"),
    ("cz", "cs_CZ"),
    ("da", "da_DK"),
    ("de", "de_DE"),
    ("el", "el_GR"),
    ("en", "en_US"),
    ("eo", "eo"),
    ("es", "es_ES"),
    ("et", "et_EE"),
    ("eu", "eu_ES"),
    ("fi", "fi_FI"),
    ("fr", "fr_FR"),
    ("ga", "ga_IE"),
    ("gl", "gl_ES"),
    ("he", "iw_IL"),
    ("hr", "hr_HR"),
    ("hs", "en_US"),
    ("hu", "hu_HU"),
    ("id", "in_ID"),
    ("in", "in_ID"),
    ("is", "is_IS"),
    ("it", "it_IT"),
    ("iw", "iw_IL"),
    ("ja", "ja_JP"),
    ("kl", "kl_GL"),
    ("ko", "ko_KR"),
    ("lt", "lt_LT"),
    ("lv", "lv_LV"),
    ("mk", "mk_MK"),
    ("nl", "nl_NL"),
    ("no", "no_NO"),
    ("pl", "pl_PL"),
    ("pt", "pt_PT"),
    ("ro", "ro_RO"),
    ("ru", "ru_RU"),
    ("se", "se_NO"),
    ("sk", "sk_SK"),
    ("sl", "sl_SI"),
    ("sq", "sq_AL"),
    ("sr", "sr_CS"),
    ("su", "fi_FI"),
    ("sv", "sv_SE"),
    ("th", "th_TH"),
    ("tr", "tr_TR"),
    ("ua", "en_US"),
    ("uk", "uk_UA"),
    ("vi", "vi_VN"),
    ("wa", "wa_BE"),
    ("zh", "zh_CN"),
    ("bokmal", "nb_NO"),
    ("catalan", "ca_ES"),
    ("croatian", "hr_HR"),
    ("czech", "cs_CZ"),
    ("danish", "da_DK"),
    ("dansk", "da_DK"),
    ("deutsch", "de_DE"),
    ("dutch", "nl_NL"),
    ("eesti", "et_EE"),
    ("estonian", "et_EE"),
    ("finnish", "fi_FI"),
    ("french", "fr_FR"),
    ("galego", "gl_ES"),
    ("galician", "gl_ES"),
    ("german", "de_DE"),
    ("greek", "el_GR"),
    ("hebrew", "iw_IL"),
    ("hrvatski", "hr_HR"),
    ("hungarian", "hu_HU"),
    ("icelandic", "is_IS"),
    ("italian", "it_IT"),
    ("japanese", "ja_JP"),
    ("korean", "ko_KR"),
    ("lithuanian", "lt_LT"),
    ("norwegian", "no_NO"),
    ("nynorsk", "nn_NO"),
    ("polish", "pl_PL"),
    ("portuguese", "pt_PT"),
    ("romanian", "ro_RO"),
    ("russian", "ru_RU"),
    ("slovak", "sk_SK"),
    ("slovene", "sl_SI"),
    ("slovenian", "sl_SI"),
    ("spanish", "es_ES"),
    ("swedish", "sv_SE"),
    ("thai", "th_TH"),
    ("turkish", "tr_TR"),
];

/// `locale_str.h`'s `language_names` (Linux build).
pub(in crate::vm::vm_init) const PARSELOCALE_LANGUAGES: &[(&str, &str)] = &[
    ("C", "en"),
    ("POSIX", "en"),
    ("cz", "cs"),
    ("he", "iw"),
    ("hs", "en"),
    ("id", "in"),
    ("sh", "sr"),
    ("su", "fi"),
    ("ua", "en"),
    ("catalan", "ca"),
    ("croatian", "hr"),
    ("czech", "cs"),
    ("danish", "da"),
    ("dansk", "da"),
    ("deutsch", "de"),
    ("dutch", "nl"),
    ("finnish", "fi"),
    ("french", "fr"),
    ("german", "de"),
    ("greek", "el"),
    ("hebrew", "he"),
    ("hrvatski", "hr"),
    ("hungarian", "hu"),
    ("icelandic", "is"),
    ("italian", "it"),
    ("japanese", "ja"),
    ("norwegian", "no"),
    ("polish", "pl"),
    ("portuguese", "pt"),
    ("romanian", "ro"),
    ("russian", "ru"),
    ("slovak", "sk"),
    ("slovene", "sl"),
    ("slovenian", "sl"),
    ("spanish", "es"),
    ("swedish", "sv"),
    ("turkish", "tr"),
];

/// `locale_str.h`'s `script_names` (Linux build): the `@modifier` spellings
/// that name a script. Every other modifier (`@euro`) is DROPPED.
pub(in crate::vm::vm_init) const PARSELOCALE_SCRIPTS: &[(&str, &str)] = &[
    ("cyrillic", "Cyrl"),
    ("devanagari", "Deva"),
    ("iqtelif", "Latn"),
    ("latin", "Latn"),
    ("Arab", "Arab"),
    ("Cyrl", "Cyrl"),
    ("Deva", "Deva"),
    ("Ethi", "Ethi"),
    ("Hans", "Hans"),
    ("Hant", "Hant"),
    ("Latn", "Latn"),
    ("Sund", "Sund"),
    ("Syrc", "Syrc"),
    ("Tfng", "Tfng"),
];

/// `locale_str.h`'s `country_names` (Linux build).
pub(in crate::vm::vm_init) const PARSELOCALE_COUNTRIES: &[(&str, &str)] = &[("RN", "US"), ("YU", "CS")];

/// `locale_str.h`'s `variant_names`: the only modifier that becomes a variant.
pub(in crate::vm::vm_init) const PARSELOCALE_VARIANTS: &[(&str, &str)] = &[("nynorsk", "NY")];

pub(in crate::vm::vm_init) fn parselocale_lookup(map: &[(&'static str, &'static str)], key: &str) -> Option<&'static str> {
    map.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// The Linux `java_props_md.c` `ParseLocale`, for the locale NAME
/// `setlocale(category, NULL)` would report (`posix_locale_category`'s
/// `LC_ALL ▸ category ▸ LANG` answer; empty means the process stayed in `C`).
///
/// Step for step:
/// 1. exactly `C` or `POSIX` (or no locale) is replaced by `en_US` -- so
///    `C.UTF-8` is NOT, and reaches step 4 as language `C` with no country;
/// 2. the name is cut at the first `.`, else at the first `@`, into the
///    locale part and the `encoding_variant` part;
/// 3. a locale part that is a `locale_aliases` key is replaced by its value;
/// 4. language = up to the first `_`, country = everything after it (or
///    NULL);
/// 5. the encoding is the text after `.`; the modifier ("variant") is the text
///    after `@` within it;
/// 6. the language goes through `language_names`, the country through
///    `country_names`, and the modifier through `script_names` and
///    `variant_names` -- a modifier in neither table (`@euro`) is ignored.
///
/// No case is normalised: the JDK publishes the subtags as spelled and
/// `Locale` normalises when it reads them.
///
/// Whether the named locale is INSTALLED is not this function's question:
/// HotSpot asks `setlocale(LC_ALL, "")`, which fails for a locale the host does
/// not have and leaves every category in `C` (`en_US`); the caller passes `C`
/// then (`posix_env_locales_loadable`), and this reads the name.
pub(crate) fn parse_locale_java_props_md(raw: &str) -> ParsedPosixLocale {
    fn split_encoding_variant(s: &str) -> Option<(&str, &str)> {
        s.find('.').or_else(|| s.find('@')).map(|i| s.split_at(i))
    }
    let lc = if raw.is_empty() || raw == "C" || raw == "POSIX" {
        "en_US"
    } else {
        raw
    };
    let (mut temp, mut encoding_variant) = split_encoding_variant(lc).unwrap_or((lc, ""));
    if let Some(alias) = parselocale_lookup(PARSELOCALE_ALIASES, temp) {
        temp = alias;
        if let Some((t, ev)) = split_encoding_variant(alias) {
            temp = t;
            encoding_variant = ev;
        }
    }
    let (language, country) = match temp.split_once('_') {
        Some((l, c)) => (l, Some(c)),
        None => (temp, None),
    };
    // `encoding_variant` starts with `.` or `@` (or is empty).
    let after_encoding = match encoding_variant.find('.') {
        Some(i) => &encoding_variant[i + 1..],
        None => encoding_variant,
    };
    let variant = after_encoding.find('@').map(|i| &after_encoding[i + 1..]);

    ParsedPosixLocale {
        language: parselocale_lookup(PARSELOCALE_LANGUAGES, language)
            .unwrap_or(language)
            .to_string(),
        country: country.map(|c| {
            parselocale_lookup(PARSELOCALE_COUNTRIES, c)
                .unwrap_or(c)
                .to_string()
        }),
        script: variant
            .and_then(|v| parselocale_lookup(PARSELOCALE_SCRIPTS, v))
            .map(str::to_string),
        variant: variant
            .and_then(|v| parselocale_lookup(PARSELOCALE_VARIANTS, v))
            .map(str::to_string),
    }
}

/// gci i45/sc (`gci-i42ta-launcher-drops-survivor-ratio-and-tlab-flags-RETIRED-UNFIXED-20261018`,
/// the TLAB consumer half): this VM's TLAB sizing from the launcher's TLAB
/// flags, read once per VM at construction and kept on
/// `HeapRealm::tlab_sizing` for the refill site. `CRATONVM_TLAB_HOTSPOT_FLAGS`
/// (default ON, `=0` kills) off gives `TlabSizing::DEFAULT` -- the old
/// constants -- whatever was typed.
///
/// gci i48/zl3 (`zx48-collector-scope-audit.md` A8): every collector, ZGC
/// included (HotSpot's ZGC honours `TLABSize`, `MinTLABSize`, `-UseTLAB` and
/// `-ResizeTLAB` too). On ZGC the refill site sizes the request exactly as on
/// the other heaps, and `ZgcRealHeap::refill_tlab_at_least` keeps a floored
/// request below its 8 KiB chunk floor (a typed `MinTLABSize`, or the object
/// alone under `-UseTLAB`). The match is exhaustive so a new collector has to
/// opt in here.
pub(in crate::vm::vm_init) fn gci_i45_sc_tlab_sizing(config: &VmConfig) -> cratonvm_gc::tlab::TlabSizing {
    let applies = match config.gc_algorithm {
        crate::config::GcAlgorithm::Generational | crate::config::GcAlgorithm::G1 => true,
        #[cfg(feature = "zgc")]
        crate::config::GcAlgorithm::Zgc => true,
    };
    if !applies {
        return cratonvm_gc::tlab::TlabSizing::DEFAULT;
    }
    let mut sizing = gci_i45_sc_tlab_sizing_from(&config.tlab_flags);
    let generational = matches!(config.gc_algorithm, crate::config::GcAlgorithm::Generational);
    // gcj j3/yg3 (`gcj-j2yg2-proposal-underpredicted-raise-counts-granted-bytes`,
    // opt-in `CRATONVM_GEN_TLAB_RAISE_BY_GRANTED_BYTES`): the share sizer's
    // underpredicted raise counts granted bytes, on the Generational heap only.
    if generational
    {
        sizing = sizing.with_raise_by_granted_bytes();
    }
    // gcj j2/yg2 (`gci-i45sc-proposal-tlab-refill-waste-limit-per-fill`, opt-in
    // `CRATONVM_TLAB_HOTSPOT_WASTE_LIMIT`): HotSpot's per-refill waste limit and
    // `-XX:TLABWasteIncrement`, on the Generational heap only (G1's share sizer
    // keeps this VM's deviation; ZGC never keeps a buffer on a miss).
    if generational && true {
        return sizing.with_hotspot_waste_limit(config.tlab_flags.waste_increment);    }
    sizing}

/// gci i45/sc: [`gci_i45_sc_tlab_sizing`]'s conversion. Pure.
pub(in crate::vm::vm_init) fn gci_i45_sc_tlab_sizing_from(flags: &crate::config::TlabFlags) -> cratonvm_gc::tlab::TlabSizing {
    cratonvm_gc::tlab::TlabSizing::from_hotspot_flags(
        flags.use_tlab,
        flags.resize_tlab,
        flags.tlab_size,
        flags.min_tlab_size,
        flags.waste_target_percent,
        flags.refill_waste_fraction,
        flags.allocation_weight,
    )
}

/// The lazy young object-start proof works at allocator anchors. A 1 MiB
/// ergonomic TLAB makes one conservative word pay for up to a MiB of dead
/// objects. Bound default Generational carves at 64 KiB; explicit HotSpot
/// sizes, frozen sizing and the existing process cap remain operator choices.
/// The cap travels with this VM's sizing, never through process state.
pub(in crate::vm::vm_init) fn pinned_copy_tlab_sizing(
    config: &VmConfig,
    sizing: cratonvm_gc::tlab::TlabSizing,
) -> cratonvm_gc::tlab::TlabSizing {
    if matches!(config.gc_algorithm, crate::config::GcAlgorithm::Generational)
        && sizing.use_tlab()
        && sizing.resize_tlab()
        && config.tlab_flags.tlab_size.is_none_or(|size| size == 0)
        && None::<std::ffi::OsString>.is_none()
    {
        sizing.with_maximum(64 * 1024)
    } else {
        sizing
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
pub(in crate::vm::vm_init) const DEFAULT_MAX_HEAP_SIZE: usize = 256 * 1024 * 1024;

pub(in crate::vm::vm_init) fn apply_container_default_heap(config: &mut VmConfig) {
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

/// gci i46/ts: is `gc+ergo` at `warning` enabled on stdout once `xlog_spec`'s
/// options are applied in order over HotSpot's default `all=warning:stdout`?
/// Tag sets match order-insensitively; `a+b*` matches every tag set that
/// contains `a` and `b` (so it hits `gc+ergo` iff its tags are among `gc`,
/// `ergo`); no level is `info`; another output (`stderr`, `file=`) leaves
/// stdout alone.
pub(in crate::vm::vm_init) fn gc_ergo_warning_on_stdout(xlog_spec: Option<&str>) -> bool {
    let mut on = true;
    for option in xlog_spec.into_iter().flat_map(|s| s.split(';')) {
        let o = option.trim().to_ascii_lowercase();
        if o == "disable" {
            on = false;
            continue;
        }
        let (what, output) = match o.split_once(':') {
            Some((what, rest)) => (what, rest.split(':').next().unwrap_or("")),
            None => (o.as_str(), ""),
        };
        if !matches!(output, "" | "stdout" | "#0") {
            continue;
        }
        let what = if what.is_empty() { "all" } else { what };
        for sel in what.split(',') {
            let (tags, level) = sel.split_once('=').unwrap_or((sel, "info"));
            let wildcard = tags.ends_with('*');
            let tags: Vec<&str> = tags.trim_end_matches('*').split('+').collect();
            let hits = tags == ["all"]
                || if wildcard {
                    tags.iter().all(|t| *t == "gc" || *t == "ergo")
                } else {
                    tags.len() == 2 && tags.contains(&"gc") && tags.contains(&"ergo")
                };
            if hits {
                on = matches!(level, "warning" | "info" | "debug" | "trace");
            }
        }
    }
    on
}

/// gci i43/gy (`gci-i42ta-g1-ignores-xmn-newsize-maxnewsize-newratio-FIXED-20261016`)
/// -- the `G1YoungGenSizer` warnings as HotSpot 25 prints them: `log_warning(gc,
/// ergo)` on the default `all=warning:stdout` output, decorated
/// `uptime,level,tags`: `[<uptime>s][warning][gc,ergo] <text>`, once each,
/// before the GC startup lines (HotSpot logs them while the heap's policy is
/// constructed). An `-Xlog:disable` option (one of the `;`-joined options of
/// `xlog_spec`) turns that output off, so then nothing. Pure. Also the
/// launcher's shape for `VmConfig::hotspot_gc_ergo_warnings` when it ends the
/// launch before a VM exists (Serial's bootstrap failure).
pub fn g1_young_sizer_warning_lines(
    xlog_spec: Option<&str>,
    uptime_secs: f64,
    warnings: &[String],
) -> Vec<String> {
    // gci i46/ts: the in-order, wildcard-aware selection test.
    let disabled = !gc_ergo_warning_on_stdout(xlog_spec);
    if disabled {
        return Vec::new();
    }
    warnings
        .iter()
        .map(|w| format!("[{uptime_secs:.3}s][warning][gc,ergo] {w}"))
        .collect()
}

/// gci i47w/xl47 (`gci-i45rv-waves-i41-i44-review-findings-FIXED-20261016` item 2's
/// residual): do the G1YoungGenSizer warnings wait for the `-Xlog` logger
/// (`unified_logging::log_on_default_stdout`, after `init_unified_logging`)
/// instead of printing in [`g1_young_sizer_warning_lines`]' fixed default
/// shape? Exactly when an `-Xlog` spec is given: only an option can
/// re-decorate stdout, and only a logger pads the lines after them. Pure.
pub(in crate::vm::vm_init) fn g1_sizer_warnings_wait_for_the_logger(xlog_spec: Option<&str>) -> bool {
    xlog_spec.is_some()
}

/// zki (page `zgc-zt23hunt4-*` item 4): the ZGC heap's uncommit probe on
/// explicit hugetlbfs pages was refused (`false` on any other heap).
pub(in crate::vm::vm_init) fn zgc_uncommit_unsupported_fact(heap: &VmHeap) -> bool {
    if heap.is_zgc() {
        return heap.zgc_explicit_uncommit_unsupported();
    }
    let _ = heap;
    false
}

/// zt t24/hunt7, moved to VM creation by zt t26/x21
/// (`zgc-zt23hunt4-transparent-huge-pages-flag-ignored-*`,
/// `CRATONVM_ZGC_TRANSPARENT_HUGE_PAGES`): HotSpot 25 ZGC's large-page
/// decision for `config` ([`crate::runtime::unified_logging::zgc_large_pages`]:
/// `-XX:+UseTransparentHugePages`, an explicit `-XX:+UseLargePages` and the
/// kernel's THP modes), `None` off ZGC, with the switch `=0`, and when neither
/// flag nor an `-Xlog` (which may print the `gc,init` block) asks for it, so
/// the sysfs files are read only then. Linux only (both flags read `false`
/// elsewhere). Reads only the configuration and the kernel's files, so it runs
/// before the heap exists.
#[cfg(feature = "zgc")]
pub(in crate::vm::vm_init) fn zgc_large_pages_at_vm_creation(
    config: &VmConfig,
) -> Option<crate::runtime::unified_logging::ZgcLargePages> {
    use crate::runtime::unified_logging as ul;
    if config.gc_algorithm != crate::config::GcAlgorithm::Zgc {
        return None;
    }
    let use_thp = cfg!(target_os = "linux") && config.use_transparent_huge_pages;
    // zt t25/hunt7b: and an explicit `-XX:+UseLargePages` (HotSpot's
    // no-pages warning, host row `h7_T8`).
    let use_large_pages = cfg!(target_os = "linux") && config.use_large_pages;
    if !(use_thp || use_large_pages || config.xlog_spec.is_some())
        || !ul::zgc_transparent_huge_pages_enabled()
    {
        return None;
    }
    let (anon, shmem) = ul::kernel_thp_modes();
    // zki (page item 4): `os::large_page_init`'s hugetlbfs sanity mapping.
    let explicit_pages = use_large_pages && !use_thp && ul::hugetlb_sanity_check();
    let decided = ul::zgc_large_pages(
        use_thp,
        use_large_pages,
        anon.as_deref(),
        shmem.as_deref(),
        explicit_pages,
    );
    // `CRATONVM_LARGE_PAGES_EXPLICIT=0`: the request is ignored silently.
    if decided.explicit && !ul::large_pages_explicit_enabled() {
        return Some(ul::ZgcLargePages {
            line: "Disabled",
            advise: false,
            warning: None,
            explicit: false,
        });
    }
    Some(decided)
}

/// zki (page `zgc-zt23hunt4-transparent-huge-pages-flag-ignored` item 5(c)):
/// the `Large Page Support:` value of a G1 or Generational VM's `gc,init`
/// block ([`crate::runtime::unified_logging::gc_init_large_pages_line`]):
/// `-XX:+UseTransparentHugePages` on a kernel with THP, or an explicit
/// `-XX:+UseLargePages` whose hugetlbfs pool passes the sanity mapping
/// (`CRATONVM_LARGE_PAGES_EXPLICIT`). `None` (= `Disabled`) on ZGC (its own
/// decision), off Linux, without either flag, and with
/// `CRATONVM_GC_INIT_LARGE_PAGES_ANY_COLLECTOR=0`.
pub(in crate::vm::vm_init) fn gc_init_large_pages_line_at_vm_creation(config: &VmConfig) -> Option<&'static str> {
    use crate::runtime::unified_logging as ul;
    #[cfg(feature = "zgc")]
    if config.gc_algorithm == crate::config::GcAlgorithm::Zgc {
        return None;
    }
    let use_thp = cfg!(target_os = "linux") && config.use_transparent_huge_pages;
    let use_large_pages = cfg!(target_os = "linux") && config.use_large_pages;
    if !(use_thp || use_large_pages)
    {
        return None;
    }
    let (anon, _shmem) = ul::kernel_thp_modes();
    let explicit_pages = use_large_pages
        && !use_thp
        && ul::large_pages_explicit_enabled()
        && ul::hugetlb_sanity_check();
    Some(ul::gc_init_large_pages_line(
        use_thp,
        use_large_pages,
        anon.as_deref(),
        explicit_pages,
    ))
}

/// zt t26/x21 (lane hunt9's review, F3; `CRATONVM_LARGE_PAGE_WARNINGS_ANY_COLLECTOR`,
/// default ON, `=0` kill): `os::large_page_init`'s `[warning][pagesize]` line
/// on a G1 or Generational VM -- HotSpot 25 prints it from the OS layer
/// whatever the collector ([`crate::runtime::unified_logging::os_large_page_warning`]:
/// `-XX:+UseLargePages` with no hugetlbfs page, `-XX:+UseTransparentHugePages`
/// on a kernel without THP). ZGC's own decision
/// ([`zgc_large_pages_at_vm_creation`]) covers a ZGC VM, so `None` there, off
/// Linux, without either flag, and with the switch `=0`. The shared-memory THP
/// warning stays ZGC's (its heap is shmem; host row `h7_W3_g1`: HotSpot G1
/// prints nothing under `-XX:+UseTransparentHugePages`).
pub(in crate::vm::vm_init) fn os_large_page_warning_at_vm_creation(config: &VmConfig) -> Option<&'static str> {
    use crate::runtime::unified_logging as ul;
    #[cfg(feature = "zgc")]
    if config.gc_algorithm == crate::config::GcAlgorithm::Zgc {
        return None;
    }
    let use_thp = cfg!(target_os = "linux") && config.use_transparent_huge_pages;
    let use_large_pages = cfg!(target_os = "linux") && config.use_large_pages;
    if !(use_thp || use_large_pages)
    {
        return None;
    }
    let (anon, _shmem) = ul::kernel_thp_modes();
    let explicit_pages = use_large_pages && !use_thp && ul::hugetlb_sanity_check();
    ul::os_large_page_warning(use_thp, use_large_pages, anon.as_deref(), explicit_pages)
}




/// zt t21/tg10: this VM's effective `-XX:±ClassUnloading` / `-Xnoclassgc`
/// (`VmConfig::class_unloading`): `CRATONVM_CLASS_UNLOADING_OFF_KEEPS_CLASSES=0`
/// is the kill (the option is accepted and ignored again: classes unload).
/// zt t23/x19: one function, read by the VM's `loader_identity` and by the
/// OnLoad scope of its startup agents (JVMTI `IsClassUnloadingEnabled` before
/// the VM exists), so the two answers cannot disagree.
pub(in crate::vm::vm_init) fn effective_class_unloading(config: &VmConfig) -> bool {
    config.class_unloading
}

#[cfg(feature = "experimental-debug")]
pub(in crate::vm::vm_init) fn load_startup_jvmti_agents(config: &VmConfig, env: &mut crate::jvmti::JvmtiEnv) {
    // The C `jvmtiEnv`s the agents obtain in `Agent_OnLoad` act on this VM,
    // which `Vm::new` binds them to once it exists
    // (`jvmti::native_env::bind_startup_envs`, interpreter round i1 wave 13).
    let agent_registry = &mut env.c_table.agent_registry;
    // zt t23/x19 (review page item 28e): the class-unloading bit the agents'
    // envs answer in `Agent_OnLoad`, noted inside the scope below before any
    // agent runs. Only with an agent to ask (one flag read).
    let onload_class_unloading =
        (!config.jvmti_agent_options.is_empty()).then(|| effective_class_unloading(config));
    // Interpreter round i2 wave 61, lane L1: the VM properties
    // `GetSystemProperty` answers in `Agent_OnLoad`, before the property map
    // exists; what the agents set or add is applied when the envs are bound.
    // Built only when a native agent is there to read them (startup cost).
    let onload_properties = if config.jvmti_agent_options.is_empty() {
        Vec::new()
    } else {
        crate::jvmti::native_env::onload_vm_properties(config)
    };
    let (startup_envs, onload_effects) =
        crate::jvmti::native_env::collect_startup_envs_with(onload_properties, || {
            if let Some(on) = onload_class_unloading {
                crate::jvmti::native_env::note_onload_class_unloading(on);
            }
            for option in &config.jvmti_agent_options {
                if option.starts_with("-javaagent:") {
                    panic!(
                        "failed to load JVMTI startup agent `{option}`: \
                     -javaagent must run through runtime::agent_loader::invoke_premains"
                    );
                }
                if !option.starts_with("-agentlib:") && !option.starts_with("-agentpath:") {
                    panic!(
                        "failed to load JVMTI startup agent `{option}`: unsupported agent option"
                    );
                }

                let (path, options) = crate::jvmti::parse_agent_arg(option);
                // Interpreter round i2 wave 55, lane L1: `-agentlib:` looks in
                // the JDK's library directory first, `-agentpath:` loads the path
                // as given (`AgentRegistry::load_startup_agent`).
                let agentpath = option.starts_with("-agentpath:");
                let loaded = agent_registry.load_startup_agent(
                    &path,
                    options.as_deref(),
                    !agentpath,
                    config.java_home.as_deref(),
                );
                if let Err(err) = loaded {
                    let detail = startup_agent_failure_detail(&path, agentpath, &err);
                    std::panic::panic_any(crate::error::VmInitializationError { detail });
                }
            }
        });
    env.c_table.startup_native_envs = startup_envs;
    env.startup_onload_effects = onload_effects;
}

/// HotSpot's second line for a startup agent that failed to load
/// (`JvmtiAgent::load` / `invoke_Agent_OnLoad`, `vm_exit_during_initialization`),
/// `name` as the command line gave it. MEASURED (HotSpot 25.0.3, Windows):
/// `agent library failed Agent_OnLoad: <name>` for a non-zero return, and
/// `Could not find agent library <name> in absolute path, with error: <os text>`
/// for a library that does not load, `on the library path` in place of `in
/// absolute path` for `-agentlib:` (`-agentpath:` says `in absolute path`
/// even for a relative path, and for an empty one). A library without
/// `Agent_OnLoad` hangs HotSpot 25 on Windows; the line is from
/// `invoke_Agent_OnLoad` (not measured). Interpreter round i2 wave 54, lane
/// L1 (it panicked with a Rust message before).
#[cfg(feature = "experimental-debug")]
pub(in crate::vm::vm_init) fn startup_agent_failure_detail(
    name: &str,
    agentpath: bool,
    err: &crate::jvmti::JvmtiError,
) -> String {
    use crate::jvmti::JvmtiError;
    let not_found = |cause: &str| {
        let where_ = if agentpath {
            "in absolute path"
        } else {
            "on the library path"
        };
        format!("Could not find agent library {name} {where_}, with error: {cause}")
    };
    match err {
        JvmtiError::AgentEntryPointFailed { .. } => {
            format!("agent library failed Agent_OnLoad: {name}")
        }
        JvmtiError::AgentEntryPointMissing { .. } => {
            format!("Could not find Agent_OnLoad function in the agent library: {name}")
        }
        JvmtiError::AgentLibraryLoadFailed { cause, .. } => not_found(cause),
        // `load_agent` refuses an empty path before trying it (HotSpot
        // tries it and reports the loader's refusal); the options cannot
        // hold a NUL from a command line.
        other => not_found(&other.to_string()),
    }
}

/// A build without `experimental-debug` has no JVMTI, so it cannot run a
/// startup agent: it stops the boot as HotSpot stops it for an agent library
/// it cannot load -- `Error occurred during initialization of VM`, then
/// [`startup_agent_unsupported_detail`]'s line, on stdout, exit status 1 --
/// with the reason in place of the loader's text. Interpreter round i2 wave
/// 57, lane L1 (host run of `L1W54JvmtiAgentInitError` on a release build:
/// it panicked with `JVMTI support is not compiled in` on stderr).
#[cfg(not(feature = "experimental-debug"))]
pub(in crate::vm::vm_init) fn reject_startup_jvmti_agents_when_disabled(config: &VmConfig) {
    if let Some(option) = config.jvmti_agent_options.first() {
        let detail = startup_agent_unsupported_detail(option);
        std::panic::panic_any(crate::error::VmInitializationError { detail });
    }
}

/// HotSpot's `Could not find agent library <name> in absolute path, with
/// error: <cause>` (`on the library path` for `-agentlib:`), `name` as the
/// command line gave it (the option's value up to its first `=`), the cause
/// that this build has no JVMTI. Any other option is named whole.
#[cfg_attr(feature = "experimental-debug", allow(dead_code))]
pub(in crate::vm::vm_init) fn startup_agent_unsupported_detail(option: &str) -> String {
    const CAUSE: &str = "JVMTI support is not compiled into this CratonVM build \
                         (it needs the experimental-debug feature)";
    let (value, where_) = if let Some(value) = option.strip_prefix("-agentpath:") {
        (value, "in absolute path")
    } else if let Some(value) = option.strip_prefix("-agentlib:") {
        (value, "on the library path")
    } else {
        return format!("Could not load agent {option}: {CAUSE}");
    };
    let name = value.split_once('=').map_or(value, |(name, _)| name);
    format!("Could not find agent library {name} {where_}, with error: {CAUSE}")
}

/// Boot precondition for `--real-jdk`
/// (`docs/book/src/user-guide/real-jdk-mode.md` §1.1, §8): strict mode means real
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
///     rejects `--real-jdk --synthetic-jdk` at argv-parse time, but embedders
///     (`libcratonvm`, `cratonvm-embed`) and in-process harnesses construct a
///     `VmConfig` by hand and never see argv. `SharedVm::new` is the one choke
///     point every boot passes through, so the config-consistency check lives
///     here too rather than only in the CLI.
///  3. **One boot check, not two.** The JDK search is
///     [`crate::config::require_real_jdk`]'s job; its message already carries
///     [`crate::config::describe_jdk_search`]'s per-probe account and the
///     accepted image layouts (`jmods/java.base.jmod` or `lib/modules`). A
///     second, `--real-jdk`-flavoured copy of that search would drift out of
///     sync with the first the moment either is edited. We delegate and wrap.
///
/// The wrapper preamble exists because `require_real_jdk`'s stock text offers
/// `--synthetic-jdk` as one of its four fixes, and under `--real-jdk` that
/// escape is unavailable by construction (§9: the two flags conflict) — a
/// strict-mode operator handed that suggestion would follow it into a second,
/// more confusing error.
/// One `--real-jdk` application class-path element, as HotSpot's app loader
/// holds it.
///
/// `jdk.internal.loader.URLClassPath` turns every `java.class.path` element
/// into `new File(s).getCanonicalFile()` before it builds a URL, so a class's
/// code source and every resource URL name an ABSOLUTE path, while the
/// property keeps whatever the user typed. This VM used the typed spelling,
/// and a relative one broke both: `java -jar app.jar` gave the fat jar's
/// classes the code source `file:/app.jar` (the directory dropped), so Spring
/// Boot's `ApplicationHome` found no source and the startup line lost its
/// `(/dir/app.jar started by ...)`. See
/// `fatjar-nested-classes-directory-url-openstream-FIXED-20260924.md`.
///
/// Canonical where the element exists, as HotSpot's is. Windows takes
/// `std::path::absolute` instead: `fs::canonicalize` answers a `\\?\C:\…`
/// verbatim path there, which no URL spells. Empty elements, launcher
/// wildcards (`lib/*`) and `<archive>!/<dir>` tokens pass through unchanged;
/// the class path expands and resolves those itself.
pub(in crate::vm::vm_init) fn jdk_only_app_class_path_entry(entry: &str) -> String {
    if entry.is_empty() || entry.contains('*') || entry.contains("!/") {
        return entry.to_string();
    }
    let path = std::path::Path::new(entry);
    #[cfg(not(windows))]
    let absolute = std::fs::canonicalize(path).or_else(|_| std::path::absolute(path));
    #[cfg(windows)]
    let absolute = std::path::absolute(path);
    match absolute {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => entry.to_string(),
    }
}


pub(in crate::vm::vm_init) fn require_jdk_image_for_jdk_only(config: &VmConfig) -> Result<Option<PathBuf>, VmError> {
    // Covers embedders and in-process harnesses that never parse argv.
    config.validate_compatibility()?;

    if !config.is_jdk_only() {
        // Default path: no host probing. See property 1 above.
        return Ok(None);
    }

    match crate::config::require_real_jdk(config.java_home.as_deref()) {
        Ok(home) => Ok(Some(home)),
        Err(detail) => Err(VmError::InvalidConfiguration(format!(
            "--real-jdk requires a real JDK runtime image, and none was found.\n\
             \n\
             In Real-JDK mode real class bytes are authoritative: no class may be \
             fabricated and no synthetic-stub native may be registered, so there is \
             no class library left to boot without a runtime image. The \
             --synthetic-jdk escape hatch does NOT apply here — it conflicts with \
             --real-jdk by construction. If you want today's compatibility \
             behaviour instead, re-run with --compatible.\n\
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
/// `ensure_synthetic_class`, which records the `--real-jdk` violation and then
/// fabricates anyway, so a strict run reported a violation while continuing in
/// the state contract §5 forbids.
///
/// # Under `--real-jdk` the question is not asked, because the answer is a constant
///
/// From 2026-08-05 to 2026-08-12 this asked anyway and absorbed the refusal.
/// Measured 2026-08-12 under `--real-jdk --explain-jdk-only --real-jdk-report`
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
/// `--fast-boot`. `None`, the caller skips the wiring, and the `warn!` states
/// the *consequence* — the natives bound to the class are unreachable —
/// rather than just the fact.
///
/// The boot deliberately continues in either mode. Under `--real-jdk` a real
/// `java.util.Collections` / `Enumeration` / `Comparator` is on the boot
/// classpath and runs its own bytecode. Failing the boot instead would refuse
/// a run that is otherwise conforming.
pub(in crate::vm::vm_init) fn ensure_bootstrap_compat_class(
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
    // one, and `--real-jdk` has no quarrel with real bytes.
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

/// Where the boot runs JDK 25's `System.initPhase1` tail (`Finalizer`
/// initialised, `startThreads()`, `VM.initLevel(1)`) under
/// `CRATONVM_INIT_FINALIZER_BEFORE_PHASE1`; see
/// [`Vm::init_phase1_tail_placement`] (nio21 wave 18 L5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitPhase1TailPlacement {
    /// `Finalizer` initialised before `initPhase1`, which then runs the tail
    /// itself (HotSpot's order; i3 wave 79).
    BeforePhase1,
    /// The launcher runs the tail right after `initPhase1`, before the boot
    /// layer (diagnostic arm).
    AfterPhase1,
    /// The launcher runs the tail after `build_boot_layer` /
    /// `finish_boot_layer` (the `--real-jdk` default).
    AfterBootLayer,
}

impl InitPhase1TailPlacement {
    /// `CRATONVM_INIT_FINALIZER_TAIL`'s value (`None` = unset) under
    /// `--real-jdk`: `0` / `off` / `false` / `no` / `early` -> before
    /// `initPhase1`; `phase1` -> right after it; anything else -> after the
    /// boot layer.
    pub fn from_word(word: Option<&str>) -> Self {
        let Some(word) = word else {
            return Self::AfterBootLayer;
        };
        let w = word.trim().to_ascii_lowercase();
        if cratonvm_types::flags::value_is_off(&w) || w == "early" || w == "before-phase1" {
            Self::BeforePhase1
        } else if w == "phase1" || w == "after-phase1" {
            Self::AfterPhase1
        } else {
            Self::AfterBootLayer
        }
    }
}

/// What [`Vm::boot_init_phase1`] found, for [`Vm::boot_module_system`]
/// (nio21 wave 21 L5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootPhase1 {
    /// `System.initPhase1` returned normally (it ran its own tail).
    pub completed: bool,
    /// Where the deferred tail goes ([`Vm::init_phase1_tail_placement`]).
    pub tail: Option<InitPhase1TailPlacement>,
}

/// nio21 wave 21 L5: `CRATONVM_EMBED_LAUNCHER_BOOT` (default ON, `=0` restores
/// the old embedded boot). Whether `libcratonvm`'s bootstrap runs the
/// launcher's real-JDK sequence -- [`Vm::boot_init_phase1`],
/// [`Vm::boot_module_system`], and after the level reaches 4
/// [`Vm::boot_publish_jdk_booted`] -- instead of a bare `initPhase1`. Read per
/// VM creation (cold). The launcher always runs the sequence; this switch
/// never reaches it.
pub fn embed_launcher_boot_enabled() -> bool {
    true
}

/// Whether the JDK's `VM.initLevel` static, read as `level`, still needs the
/// `VM.initLevel(target)` write. `VM.initLevel(int)` throws `InternalError("Bad
/// level")` unless the level rises, so a level at or past `target` (or one that
/// could not be read) is left alone. The one decision behind every boot write
/// of the JDK level (j27 lanes FBLEVEL, FBTAIL: [`Vm::raise_jdk_init_level`]).
pub(in crate::vm::vm_init) fn jdk_level_below(level: Option<Value>, target: i32) -> bool {
    matches!(level, Some(Value::Int(l)) if (0..target).contains(&l))
}

/// A loaded class's static, by name; `None` when the class or field is not
/// there (nothing is loaded or initialised).
pub(in crate::vm::vm_init) fn loaded_static_value(
    ctx: &crate::vm::vm_exec::NativeContextImpl<'_>,
    class: &str,
    field: &str,
) -> Option<Value> {
    use cratonvm_native_api::{NativeClassAccess as _, NativeSystemAccess as _};
    let id = ctx.class_id_by_name(class)?;
    let index = ctx.static_field_index_by_name(id, field)?;
    Some(ctx.get_static_field(id, index))
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
///
/// `frames` are [`BacktraceFrame`]s, materialised into `StackTraceEntry`s only
/// by the readers (`throwable_stack_trace_for` and siblings).
#[derive(Debug, Clone)]
pub(crate) struct ThrowableStackTrace {
    pub(in crate::vm::vm_init) throwable: ObjectRef,
    /// Shared (nio21 wave 15 L4): a capture-memo hit
    /// (`stackwalker::ThrowableCaptureMemo`) stores the memo's own frames by
    /// one refcount instead of cloning every frame, and the record's drop is
    /// one decrement. The frames are never mutated in place; a rewrite
    /// stores a new record.
    pub(in crate::vm::vm_init) frames: Arc<[BacktraceFrame]>,
    /// `cratonvm_classloading::class_redefinition_count()` when the trace was
    /// stored: the class versions its frames ran. A reader resolves the line
    /// of a frame whose class was redefined since from the table that
    /// version had (`obsolete_frames::resolve_lines_as_captured`,
    /// interpreter round i1 wave 22, lane L3).
    pub(in crate::vm::vm_init) captured_at: u64,
}

/// Each frame's `BacktraceFrame::compile_cp_stamp` (the version a compiled
/// frame ran), or `None` -- no allocation -- when no frame of the trace has
/// one. For `obsolete_frames::resolve_lines_as_captured` (interpreter round
/// i1 wave 38, lane L3).
pub(in crate::vm::vm_init) fn compiled_stamps(frames: &[BacktraceFrame]) -> Option<Vec<Option<u64>>> {
    frames
        .iter()
        .any(|frame| frame.compile_cp_stamp().is_some())
        .then(|| {
            frames
                .iter()
                .map(BacktraceFrame::compile_cp_stamp)
                .collect()
        })
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

    /// Whether lazy FFM claims may publish exact interpreter roots. This
    /// process environment switch is snapshotted once for this VM, rather
    /// than queried on every FFM downcall.
    pub ffm_lazy_precise_claim_enabled: bool,

    /// `CRATONVM_FINALIZER_ANCESTOR_WALK_PAST_FINALIZABLE` (JIT round 21 wave
    /// 17, lane finalize-reinit; default ON, `=0` restores the defect; item 1
    /// of `docs/feature-designs/real-jdk-mode.md`),
    /// latched per VM: the finalizable-define walk
    /// (`exception_dispatch::finalizable_define_stale_ancestors`) goes on past
    /// a finalizable superclass, whose own define may never have reached a
    /// listener (it was loaded recursively as a superclass).
    pub finalizer_ancestor_walk_past_finalizable: bool,

    /// `CRATONVM_FINALIZER_ANCESTOR_RECORD_REACHABLE` (JIT round 21 wave 17,
    /// lane finalize-reinit; default ON, `=0` restores the defect; item 2 of
    /// the same page), latched per VM: every non-finalizable ancestor a
    /// finalizable define can reach is recorded, whatever its constructors do
    /// now, so the bar reads a redefined ancestor's new bytes.
    pub finalizer_ancestor_record_reachable: bool,

    /// `CRATONVM_FINALIZER_BAR_REFUSAL_EPOCH` (JIT round 21 wave 18, lane
    /// finalize-reinit; opt-in perf, default OFF; item 2 of
    /// `docs/feature-designs/real-jdk-mode.md`, its perf
    /// half), latched per VM: a finalizer-bar refusal row
    /// (`JitRealm::finalizer_bar_refusals`) of an older redefine epoch is
    /// re-asked instead of answering.
    pub finalizer_bar_refusal_epoch: bool,

    /// How many JNI re-`<init>` calls of a registered finalizable object are
    /// running in this VM with their `Object.<init>`-return witness armed
    /// (JIT round 22 wave 1, lane review; `VmConfig::jni_reinit_witness_interprets`,
    /// `jni.rs` `JniReinitWitnessWindow`). Non-zero: every door runs bodies
    /// interpreted and keeps frames, and the `<init>`-return hook counts
    /// `Object.<init>`'s own return. Zero outside such a call, so the doors
    /// pay one relaxed load.
    pub jni_reinit_witness_window: std::sync::atomic::AtomicU32,

    /// gci i11/jd: `CRATONVM_DBG_BASE_BODY_ENTRY`'s class-name prefix, latched
    /// once for this VM (`None` = off, the frame-push witness then costs one
    /// load). See `runtime/interpreter/gci_i11jd_witness.rs`.
    pub dbg_base_body_entry_filter: Option<Box<str>>,

    /// gci i14/sx: the lambda paths' six default-ON switches, read once for
    /// this VM (they were process `OnceLock` latches). See
    /// `runtime::interpreter::lambda::LambdaSwitches`.
    pub(crate) lambda_switches: crate::runtime::interpreter::lambda::LambdaSwitches,

    /// gci i14/sx: `CRATONVM_DEOPT_CHAIN_RUN_FROM_OUTERMOST` (default ON),
    /// read once for this VM; `interpreter::run_pushed_frame_to_completion`
    /// reads it on every nested interpreter entry. It was a process
    /// `OnceLock` latch outside tests and a per-call read in tests.
    pub(crate) deopt_chain_run_from_outermost: bool,

    /// gci i14/sx: `CRATONVM_JIT_FFM_DIRECT_DOOR` (default ON), read once for
    /// this VM (`jit::helpers::ffm_direct_door_enabled`; was a process latch).
    pub(crate) jit_ffm_direct_door: bool,

    /// JIT round 22 (lane memory, FFM22-2): `CRATONVM_JIT_FFM_DOOR_TRIM`
    /// (**default ON**, `=0` off), read once for this VM
    /// (`jit::helpers::try_ffm_direct_door`).
    pub(crate) jit_ffm_door_trim: bool,

    /// gcj j3/rt3 residual 1: `CRATONVM_MH_COMPILED_LINK_UNBOUND` (default ON),
    /// read once for this VM (`jit::helpers::mh_compiled_link_unbound_enabled`).
    /// It was read through the flag snapshot on every compiled `invokeExact` /
    /// `invoke` call while the constant bind is off.
    pub(crate) mh_compiled_link_unbound: bool,

    /// gci i14/sx: `CRATONVM_G1_DBG_GRAY_PROV` (default OFF), read once for
    /// this VM (`vm_exec::g1_gray_prov_dbg`; was a process latch).
    pub(crate) dbg_g1_gray_prov: bool,

    /// gci i27/kd (`gch-h2jr-review-findings-RETIRED-RULING-20261018` item 9): the JNI
    /// in-native switch package, read once for this VM (it was a process
    /// `OnceLock` in `native::jni::jni_switches`, so a second VM inherited
    /// the first reader's answer). Read per call through `jni_switches`.
    pub(crate) jni_switches: crate::native::jni::JniSwitches,

    /// gci i27/kd: `CRATONVM_ZGC_MULTIARRAY_TLAB` (default ON), read once for
    /// this VM (`interpreter::zgc_multiarray_tlab_enabled`; was a process
    /// `OnceLock`).
    pub(crate) zgc_multiarray_tlab: bool,

    /// gci i23/ep (`common-w14d-proposal-ephemeron-native-side-tables`):
    /// `CRATONVM_GEN_SIDE_ROW_EPHEMERON` / `CRATONVM_ZGC_SIDE_ROW_EPHEMERON`
    /// (both default ON), read once per VM instead of twice per GC root scan.
    pub(crate) side_row_ephemeron_engaged: bool,

    /// gci i27/kd: `CRATONVM_JIT_VARHANDLE_ARRAY_ELEMENT_FAST` (default ON,
    /// `0` / `false` / `off` / `no` kill it), read once for this VM
    /// (`jit::helpers::varhandle_array_element_fast_enabled`; was a process
    /// `OnceLock`).
    pub(crate) jit_varhandle_array_element_fast: bool,

    /// JIT round 21 wave 4, lane compat (COMPAT21-1):
    /// `CRATONVM_JIT_THIN_HELPER_ASKS_RESOLVER` (default ON, `0` kills it),
    /// read once for this VM (`jit::helpers::thin_helper_asks_resolver_enabled`).
    pub(crate) jit_thin_helper_asks_resolver: bool,
    /// JIT round 23 wave 1, lane L6 (SYNC22-3):
    /// `CRATONVM_JIT_COMPAT_BIND_ASKS_REGISTRY` (default ON, `0` kills it),
    /// read once for this VM
    /// (`jit::helpers::compat_bind_asks_registry_enabled`): under
    /// `--compatible` a thin direct helper is not bound for a triple the
    /// registry holds no row for.
    pub(crate) jit_compat_bind_asks_registry: bool,
    /// zt t27/jd1 item 2: `CRATONVM_JIT_COMPAT_BIND_ASKS_RESOLVER` (default
    /// ON, `0` kills it), read once for this VM
    /// (`jit::helpers::compat_bind_yields_to_bytecode`): under `--compatible`
    /// a thin direct helper is not bound for a registered `Bridge` /
    /// `SyntheticStub` row the generic tail yields to the loaded bytecode.
    pub(crate) jit_compat_bind_asks_resolver: bool,
    /// JIT round 22 wave 4b, lane sync: `CRATONVM_HW_FLOAT_CAS` (default ON,
    /// `0` restores the lock path), read once for this VM
    /// (`vm_exec::hw_cas_primitive_field`'s `float` / `double` arms).
    pub(crate) hw_float_cas: bool,
    /// JIT round 22 wave 4c, lane sync: `CRATONVM_HW_SUBWORD_CAS` (default
    /// ON, `0` restores the lock path), read once for this VM
    /// (`vm_exec::hw_cas_subword`).
    pub(crate) hw_subword_cas: bool,
    /// JIT round 22 wave 5b, lane sync: `CRATONVM_HW_STATIC_CAS` (default ON,
    /// `0` restores the mirror-monitor lock path), read once for this VM
    /// (`vm_exec::hw_static_cell_update`).
    pub(crate) hw_static_cas: bool,
    /// JIT round 23 wave 1, lane L4 (W8-L3-1): `CRATONVM_REF_CAS_SLOT_LOOP`
    /// (default ON, `0` restores the lock path), read once for this VM
    /// (`vm_exec::ref_slot_cas_fallback`: a reference CAS the hardware arms
    /// decline is one compare-exchange on the slot, not the lock path).
    pub(crate) hw_ref_cas_slot_loop: bool,
    /// COMPAT21-3: `CRATONVM_JIT_WILL_RUN_ASKS_RESOLVER` (default ON since the
    /// r23 A/B, R23-AB-FLIP; `=0` off), read once
    /// for this VM (`interpreter::will_run_asks_resolver_enabled`).
    pub(crate) jit_will_run_asks_resolver: bool,
    /// COMPAT21-4: `CRATONVM_JIT_FORCED_SEAL_ASKS_RESOLVER` (opt-in), read once
    /// for this VM (`interpreter::forced_seal_asks_resolver_enabled`).
    pub(crate) jit_forced_seal_asks_resolver: bool,
    /// zt t27/jd1: `CRATONVM_JIT_STATIC_OWNER_LOADS_FIRST` (default ON, `0`
    /// kills it), read once for this VM (`vm_exec::static_owner_load_pending`).
    pub(crate) jit_static_owner_loads_first: bool,
    /// zt t29/jd: `CRATONVM_JIT_NATIVE_SITE_OWNER_BY_ID` (default ON, `0`
    /// kills it), read once for this VM (`jit::helpers::native_site_admission_owner`).
    pub(crate) jit_native_site_owner_by_id: bool,
    /// zt t27/jd1: owner names whose load `load_static_owner_before_dispatch`
    /// already tried and failed for this VM (a registered native with no class
    /// file). Such an owner is not owed a load again, so its compiled sites
    /// cache their native after one tail call instead of retrying the load (a
    /// class-path scan) on every call.
    pub(crate) jit_static_owner_load_refused:
        parking_lot::Mutex<std::collections::HashSet<Box<str>>>,
    /// This VM's loader-identity tables: the defining-loader rows, the
    /// orphaned-class markers, the non-strong hidden classes, the loader meta
    /// and namespace rows, and their lock-free screens. Process globals keyed
    /// by `vm_identity` until interpreter round i5 wave 101, lane L5
    /// (`i100-L5-loader-identity-tables-are-process-globals`); now dropped
    /// with the VM. The natives reach it through
    /// `NativeSystemAccess::loader_identity_tables`.
    pub loader_identity: cratonvm_native_builtins::classloader::LoaderIdentityTables,

    /// The next user-defined loader namespace id this VM hands out
    /// (`NativeClassAccess::allocate_loader_id`); starts at
    /// `ClassLoaderId::NATIVE_FIRST_USER_DEFINED` and only grows. A process
    /// `static` until interpreter round i5 wave 105, lane L5
    /// (`i101-L5-remaining-loader-family-process-globals`): every table keyed
    /// by a namespace id is per VM now (the class manager, the loader-identity
    /// tables, `UserLoaderParents`) or carries the VM's identity in its key.
    pub(crate) next_loader_id: std::sync::atomic::AtomicU32,

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
    /// RT-7 — did the compiled-tier GPU array barrier arm *for this VM*?
    ///
    /// `cratonvm_jit::gpu_barrier`'s two addresses are process-global and have
    /// to be: the dirty table is drained by bucket across every VM. The
    /// DECISION that reads them is not. Whether a given method may be compiled
    /// while the input-residency cache is live depends on whether the barrier
    /// was armed before *this* VM compiled anything, and
    /// `offload_jit_gate::array_writer_policy` used to answer that from the
    /// process-wide `gpu_barrier::is_armed()` — so a second VM's arming could
    /// flip the policy underneath a first VM whose array writers had already
    /// been compiled with no barrier in them.
    ///
    /// This is that answer, taken once, from this VM's own `--gpu` state and
    /// from `gpu_barrier::arm`'s return value, which nothing used to read.
    ///
    /// `false` on every CPU-only build, on every non-x86_64 target, and under
    /// `CRATONVM_JIT_GPU_ARRAY_BARRIER=0` -- and `false` selects
    /// `ArrayWriterPolicy::KeepCache`, the historical behaviour and the safe
    /// direction.
    ///
    /// It used to be `false` also "when `arm` refused": a `--gpu` VM created
    /// after another VM had compiled code. That refusal was removed on
    /// 2026-09-17 (RT-7), with the proof at `gpu_barrier::arm`; every `--gpu`
    /// VM on x86_64 now records `true` unless the kill switch is set.
    pub gpu_array_barrier_armed: bool,
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

    /// `System.out` / `err` / `in` (in that order) written by a `set*0`
    /// native, null included: from then on a null field reads as null, not as
    /// the canonical stream above. See
    /// `runtime::interpreter::SystemStreamField::null_reads_as_null`.
    pub system_stream_written: [std::sync::atomic::AtomicBool; 3],

    /// System properties: populated with platform defaults + user overrides.
    pub system_properties: RwLock<HashMap<String, String>>,

    /// Weak self-reference so native methods can obtain `Arc<SharedVm>` for
    /// spawning new threads. Set by `Vm::new()` after wrapping in `Arc`.
    pub self_arc: RwLock<Option<Weak<SharedVm>>>,
    /// Thread starts this VM's boot held back for a boot image's capture run
    /// (Phase 5 fast boot, WP3): empty, and never touched past one atomic
    /// load per starter, outside a capture run. The restore path drains it
    /// with [`crate::runtime::deferred_starts::run_deferred_starts`].
    pub deferred_starts: crate::runtime::deferred_starts::DeferredStarts,
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

    /// zs s7/nd6 (`zgc-zs2nd2-native-door-residuals-SUPERSEDED-20261019.md` §8): this
    /// VM's "boot finished" latch, for an embedding that never raises the
    /// init level to 4 (a bare `Vm::new` + `Vm::invoke`, the `cratonvm-embed`
    /// lifecycle). Set by [`Self::mark_boot_finished`]; level 4 implies it
    /// ([`Self::boot_finished`]). A refused start-up object is VM
    /// initialisation's failure only while this reads `false`.
    pub boot_finished_latch: std::sync::atomic::AtomicBool,
}

// ---------------------------------------------------------------------------
// Typed bootstrap state machine
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(in crate::vm::vm_init) struct Allocated;
#[derive(Debug)]
pub(in crate::vm::vm_init) struct ClassesReady;
#[derive(Debug)]
pub(in crate::vm::vm_init) struct NativesReady;
#[derive(Debug)]
pub(in crate::vm::vm_init) struct RuntimeReady;

/// Compile-time ordering plus testable runtime invariants for VM startup.
///
/// The payload is intentionally small: the large subsystem values remain local
/// to `SharedVm::new`, while this token is the only value allowed to cross each
/// phase boundary. A new initialization step therefore cannot be reordered
/// past classes/native/runtime readiness without changing the token type.
#[must_use = "a bootstrap phase must be advanced or explicitly finished"]
pub(in crate::vm::vm_init) struct BootstrapPhase<State> {
    pub(in crate::vm::vm_init) started: std::time::Instant,
    pub(in crate::vm::vm_init) _state: std::marker::PhantomData<State>,
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::vm::vm_init) enum BootstrapInvariantError {
    NoClasses,
    CoreObjectMissing,
    NoNatives,
    RuntimeNotWired,
}

impl BootstrapPhase<Allocated> {
    pub(in crate::vm::vm_init) fn begin() -> Self {
        Self {
            started: std::time::Instant::now(),
            _state: std::marker::PhantomData,
        }
    }

    pub(in crate::vm::vm_init) fn classes_ready(
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
    pub(in crate::vm::vm_init) fn natives_ready(
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
    pub(in crate::vm::vm_init) fn runtime_ready(
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
    pub(in crate::vm::vm_init) fn finish(self) -> std::time::Duration {
        self.started.elapsed()
    }
}


/// Choose one retained-trace shard from a Throwable's raw (GC-remapped)
/// heap address. Heap objects are at least 8-byte aligned, so the low 3
/// bits carry no entropy -- drop them before masking down to the shard
/// count so adjacent allocations still spread across shards.
#[inline]
pub(in crate::vm::vm_init) fn throwable_stack_shard(address: usize) -> usize {
    let shards = crate::vm::realms::thread_realm::THROWABLE_STACK_SHARDS;
    debug_assert!(shards.is_power_of_two());
    (address >> 3) & (shards - 1)
}

/// The Throwable backtrace table's post-collection pass: forward every trace
/// whose Throwable moved, drop every trace whose Throwable died, leave the
/// rest in place. `is_live` answers for a PRE-remap address that is not in
/// `pointer_map`.
///
/// # Two phases, and why
///
/// Every entry is looked up in `pointer_map` exactly ONCE. A sliding
/// compactor's map is not a set of disjoint moves: survivors slide into space
/// their predecessors vacated, so `A -> B` and `B -> C` in one map is the
/// normal case (ZGC's slide, the Generational young+compact composition). The
/// one-pass loop this replaced (gc-common w2-g, 2026-09-23) inserted a moved
/// entry straight into its destination shard; when that shard had a higher
/// index it was drained LATER in the same pass and the entry was looked up a
/// second time -- `A`'s trace was moved on to `C`, `B`'s own trace collided
/// with it, and `getStackTrace()` answered with the other exception's frames
/// or none.
///
/// 1. Per shard, under its own lock: filter in place, keeping unmoved live
///    entries where they are and pulling every moved entry out. Nothing is
///    inserted anywhere during this phase.
/// 2. Insert the moved entries at their new addresses. A moved entry WINS its
///    destination: an unmoved entry already keyed there belonged to an object
///    that died (a live object cannot share an address with the survivor that
///    moved onto it -- the `addr_keyed::remap_and_sweep` "destination
///    collision" rule), so it is overwritten.
///
/// A non-moving cycle is phase 1 only, and phase 1 is a `retain`: no drain, no
/// rehash of every surviving entry. The old loop drained and re-inserted the
/// whole table on every collection, moving or not.
///
/// Never holds two shard locks at once.
pub(in crate::vm::vm_init) fn remap_and_sweep_throwable_shards(
    shards: &[RwLock<crate::vm::realms::thread_realm::ThrowableStackShard>],
    pointer_map: &cratonvm_types::PointerMap,
    is_live: &dyn Fn(usize) -> bool,
) {
    debug_assert_eq!(
        shards.len(),
        crate::vm::realms::thread_realm::THROWABLE_STACK_SHARDS,
        "throwable_stack_shard() indexes by THROWABLE_STACK_SHARDS"
    );
    if pointer_map.is_empty() {
        for shard in shards {
            let mut traces = shard.write();
            if !traces.is_empty() {
                traces
                    .settle_pending(is_live)
                    .retain(|&addr, _| is_live(addr));
            }
        }
        return;
    }
    // Phase 1.
    let mut moved: Vec<(usize, ThrowableStackTrace)> = Vec::new();
    let mut moved_keys: Vec<(usize, usize)> = Vec::new();
    for shard in shards {
        let mut shard = shard.write();
        if shard.is_empty() {
            continue;
        }
        // A pending row is indexed only if its throwable moved or survived.
        let traces = shard.settle_pending(|addr| pointer_map.get(&addr).is_some() || is_live(addr));
        traces.retain(|&addr, _| match pointer_map.get(&addr) {
            Some(&new_addr) => {
                moved_keys.push((addr, new_addr));
                true // removed below, by value, so the trace can move
            }
            None => is_live(addr),
        });
        for (old_addr, new_addr) in moved_keys.drain(..) {
            if let Some(mut trace) = traces.remove(&old_addr) {
                // SAFETY: a relocation-map destination; the collector just
                // wrote the moved object's header there.
                trace.throwable = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                moved.push((new_addr, trace));
            }
        }
    }
    // Phase 2, grouped by destination shard so each lock is taken once.
    if moved.is_empty() {
        return;
    }
    let mut buckets: Vec<Vec<(usize, ThrowableStackTrace)>> =
        (0..shards.len()).map(|_| Vec::new()).collect();
    for (addr, trace) in moved {
        buckets[throwable_stack_shard(addr)].push((addr, trace));
    }
    for (index, bucket) in buckets.into_iter().enumerate() {
        if bucket.is_empty() {
            continue;
        }
        let mut traces = shards[index].write();
        for (addr, trace) in bucket {
            traces.insert_now(addr, trace);
        }
    }
}


impl SharedVm {
    /// Retain a captured Throwable trace independently of the producing Java
    /// thread. The entry is non-owning and is swept by the GC remap hook.
    /// Test-only since production stores compact `BacktraceFrame`s (wave 7).
    #[cfg(test)]
    pub fn store_throwable_stack_trace(
        &self,
        throwable: ObjectRef,
        frames: Arc<[StackTraceEntry]>,
    ) {
        let frames = frames.iter().cloned().map(BacktraceFrame::Entry).collect();
        self.store_throwable_backtrace(throwable, frames);
    }

    /// [`Self::store_throwable_stack_trace`] for frames already in the
    /// registry's own form -- the construction-time capture
    /// (`stackwalker::capture_throwable_backtrace_without_store`) and any
    /// caller that owns its entries.
    pub(crate) fn store_throwable_backtrace(
        &self,
        throwable: ObjectRef,
        frames: Box<[BacktraceFrame]>,
    ) {
        self.store_throwable_backtrace_captured_at(
            throwable,
            frames,
            cratonvm_classloading::class_redefinition_count(),
        );
    }

    /// [`Self::store_throwable_backtrace`] under an EARLIER stamp: a trace
    /// rewritten after its capture (the compiled-trap splice) keeps the class
    /// versions its frames ran, which is what a reader resolves lines against.
    pub(crate) fn store_throwable_backtrace_captured_at(
        &self,
        throwable: ObjectRef,
        frames: Box<[BacktraceFrame]>,
        captured_at: u64,
    ) {
        self.store_throwable_backtrace_shared_at(throwable, Arc::from(frames), captured_at);
    }

    /// [`Self::store_throwable_backtrace`] for frames already shared: a
    /// capture-memo hit hands over the memo's own `Arc` (nio21 wave 15 L4),
    /// so a repeated throw stores its trace with one refcount increment.
    pub(crate) fn store_throwable_backtrace_shared(
        &self,
        throwable: ObjectRef,
        frames: Arc<[BacktraceFrame]>,
    ) {
        self.store_throwable_backtrace_shared_at(
            throwable,
            frames,
            cratonvm_classloading::class_redefinition_count(),
        );
    }

    /// [`Self::store_throwable_backtrace_captured_at`] for shared frames.
    pub(crate) fn store_throwable_backtrace_shared_at(
        &self,
        throwable: ObjectRef,
        frames: Arc<[BacktraceFrame]>,
        captured_at: u64,
    ) {
        let address = throwable.as_ptr() as usize;
        // Interpreter round i3 wave 82, lane L2: with the throwable-owned
        // holder on, the registry row is authoritative only once the
        // throwable's `backtrace` names the registry (the self-reference
        // marker) rather than an older holder (`backtrace_ids`).
        if self
            .threads
            .throwable_layout_cache
            .backtrace_in_object_enabled()
        {
            self.retire_backtrace_holder(throwable);
        }
        // JIT round 20 (lane throw): fresh frames only. A fresh capture's `Arc`
        // is held here and at most by the memo made from it (2); a memo hit's
        // is also held by the row of the capture that made it (3+), whose
        // frames were counted then. Overcounting only asks for a collection
        // a little early.
        if Arc::strong_count(&frames) <= 2 {
            let cache = &self.threads.throwable_layout_cache;
            cache.note_retained_trace_frames(frames.len());
            // Past the budget: latch a collection request. Nothing collects
            // here (a capture runs inside a native or a JIT door with
            // unrooted references); `maybe_gc` takes the latch at the next
            // interpreted allocation, and the implicit-exception door asks
            // for it before it builds (`exceptions::collect_if_requested`).
            if cache.take_retained_trace_pressure() {
                self.mem
                    .gc_requested
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let record = ThrowableStackTrace {
            throwable,
            frames,
            captured_at,
        };
        // Appended, not indexed (`ThrowableStackShard`): the trace a refill
        // replaces is dropped when the shard is next flushed, compacted or
        // swept. (Round 19's opt-in that moved that drop out of this lock,
        // THROWS18-8, has nothing left to move and is retired.)
        self.threads.throwable_stacks[throwable_stack_shard(address)]
            .write()
            .push(address, record);
    }

    /// `read(trace)` of `throwable`'s retained trace: with
    /// `CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT=1`, decoded from the holder
    /// its `backtrace` names (or from `throwable` itself when it IS a holder:
    /// `StackTraceElement.of(backtrace, depth)` hands the holder to
    /// `initStackTraceElements`); otherwise, and for a throwable whose
    /// `backtrace` is the registry marker, from the registry.
    pub(in crate::vm::vm_init) fn with_throwable_trace<R>(
        &self,
        throwable: ObjectRef,
        read: impl FnOnce(&ThrowableStackTrace) -> R,
    ) -> Option<R> {
        if self
            .threads
            .throwable_layout_cache
            .backtrace_in_object_enabled()
        {
            if let Some(holder) = self.backtrace_holder_of(throwable) {
                let trace = self.decode_backtrace_holder(throwable, holder)?;
                return Some(read(&trace));
            }
        }
        self.with_registry_trace(throwable.as_ptr() as usize, read)
    }

    /// `read(trace)` of the throwable at `address`'s registry row, under its
    /// shard's lock. A shard with pending rows is flushed first under the
    /// write lock ([`ThrowableStackShard`]); otherwise the read lock serves.
    pub(in crate::vm::vm_init) fn with_registry_trace<R>(
        &self,
        address: usize,
        read: impl FnOnce(&ThrowableStackTrace) -> R,
    ) -> Option<R> {
        let shard = &self.threads.throwable_stacks[throwable_stack_shard(address)];
        {
            let traces = shard.read();
            if !traces.has_pending() {
                return traces.get(&address).map(read);
            }
        }
        let mut traces = shard.write();
        traces.flush();
        traces.get(&address).map(read)
    }

    /// [`Self::throwable_stack_trace_for`] WITHOUT line resolution, with the
    /// stamp the trace was stored under. For a writer that rewrites the trace
    /// and stores it back ([`Self::store_throwable_backtrace_captured_at`]);
    /// a reader wants `throwable_stack_trace_for`.
    pub(crate) fn throwable_stack_trace_unresolved_for(
        &self,
        throwable: ObjectRef,
    ) -> Option<(Vec<StackTraceEntry>, u64)> {
        let address = throwable.as_ptr() as usize;
        self.with_throwable_trace(throwable, |trace| {
            debug_assert_eq!(trace.throwable.as_ptr() as usize, address);
            (
                trace.frames.iter().map(BacktraceFrame::to_entry).collect(),
                trace.captured_at,
            )
        })
    }

    /// Return an owned, source-line-resolved snapshot.
    ///
    /// Throwable construction deliberately stores exact frame identities with
    /// unresolved lines. Copy under the registry lock, then resolve after
    /// releasing it so a reader gets the eager-capture result without making
    /// caught-and-discarded exceptions pay the line-table walk.
    pub fn throwable_stack_trace_for(&self, throwable: ObjectRef) -> Option<Vec<StackTraceEntry>> {
        let address = throwable.as_ptr() as usize;
        let (mut frames, captured_at, compiled_at) =
            self.with_throwable_trace(throwable, |trace| {
                // The record's handle and its key move together
                // (`remap_and_sweep_throwable_shards`).
                debug_assert_eq!(trace.throwable.as_ptr() as usize, address);
                let frames = trace
                    .frames
                    .iter()
                    .map(BacktraceFrame::to_entry)
                    .collect::<Vec<_>>();
                (frames, trace.captured_at, compiled_stamps(&trace.frames))
            })?;
        let cm = self.classes.class_manager.read();
        // A frame of a class redefined since the capture: the line of the
        // version it ran (interpreter round i1 wave 22, lane L3); a compiled
        // one's, the version it was compiled from (wave 38).
        crate::runtime::interpreter::obsolete_frames::resolve_lines_as_captured(
            &cm,
            captured_at,
            &mut frames,
            compiled_at.as_deref(),
        );
        crate::runtime::stackwalker::resolve_line_numbers_in_place(&cm.class_store, &mut frames);
        Some(frames)
    }

    /// [`Self::throwable_stack_trace_for`]'s length, from one shard probe:
    /// no copy, no line resolution.
    pub fn throwable_stack_trace_len_for(&self, throwable: ObjectRef) -> Option<usize> {
        // A holder's length is its array's: no decode.
        if self
            .threads
            .throwable_layout_cache
            .backtrace_in_object_enabled()
        {
            if let Some(holder) = self.backtrace_holder_of(throwable) {
                use crate::vm::realms::backtrace_ids::HOLDER_HEADER_WORDS;
                let len = self.mem.heap.array_length(holder);
                return Some(len.saturating_sub(HOLDER_HEADER_WORDS));
            }
        }
        let address = throwable.as_ptr() as usize;
        self.with_registry_trace(address, |trace| trace.frames.len())
    }

    /// Entry `index` (OUTERMOST-first) of [`Self::throwable_stack_trace_for`]'s
    /// answer: that one entry copied and its line resolved, not the whole
    /// trace. `resolve_line_numbers_in_place` decides each entry on its own, so
    /// the answer is the same entry the whole-trace read returns.
    pub fn throwable_stack_trace_entry_for(
        &self,
        throwable: ObjectRef,
        index: usize,
    ) -> Option<StackTraceEntry> {
        let (entry, captured_at, compiled_at) = self
            .with_throwable_trace(throwable, |trace| {
                trace.frames.get(index).map(|frame| {
                    (
                        frame.to_entry(),
                        trace.captured_at,
                        frame.compile_cp_stamp(),
                    )
                })
            })
            .flatten()?;
        let mut one = [entry];
        let stamps = [compiled_at];
        let cm = self.classes.class_manager.read();
        crate::runtime::interpreter::obsolete_frames::resolve_lines_as_captured(
            &cm,
            captured_at,
            &mut one,
            compiled_at.is_some().then_some(&stamps[..]),
        );
        crate::runtime::stackwalker::resolve_line_numbers_in_place(&cm.class_store, &mut one);
        drop(cm);
        let [entry] = one;
        Some(entry)
    }

    /// Interpreter round i3 wave 82, lane L2
    /// (`CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT=1`,
    /// `i73-L2-proposal-backtrace-owned-by-the-throwable-20261106.md`): retain
    /// a capture's frames in a `long[]` holder stored in `throwable`'s
    /// `backtrace` (`realms::backtrace_ids`), as HotSpot keeps its backtrace in
    /// the object; when the holder cannot name them (or the switch is off), in
    /// the registry, with `backtrace` pointed at it (the self-reference
    /// marker). The capture's own store: every other writer (the trap splice,
    /// a replaced fresh trace) keeps [`Self::store_throwable_backtrace`], which
    /// retires a holder the throwable had.
    ///
    /// With the switch on this store writes `backtrace` itself, so the fill
    /// that follows the capture (`lang_misc::capture_throwable_trace_body`,
    /// `finish_throwable_fill`) writes only `depth`
    /// (`NativeExceptionAccess::throwable_backtrace_in_object`).
    /// [`Self::retain_captured_backtrace`] for frames already shared (a
    /// capture-memo hit's `Arc`, nio21 wave 15 L4): with the throwable-owned
    /// holder off, one refcount increment into the registry as before; with
    /// it on, borrows the slice to encode the holder (or moves the `Arc` into
    /// the registry on fallback) without cloning the frames.
    pub(crate) fn retain_captured_backtrace_shared(
        &self,
        throwable: ObjectRef,
        frames: Arc<[BacktraceFrame]>,
    ) {
        if !self
            .threads
            .throwable_layout_cache
            .backtrace_in_object_enabled()
        {
            self.store_throwable_backtrace_shared(throwable, frames);
            return;
        }
        if let Some(captured_at) = self.try_install_backtrace_holder(throwable, &frames) {
            self.store_throwable_backtrace_shared_at(throwable, frames, captured_at);
            self.point_backtrace_at(throwable, None);
        }
    }

    pub(crate) fn retain_captured_backtrace(
        &self,
        throwable: ObjectRef,
        frames: Box<[BacktraceFrame]>,
    ) {
        if !self
            .threads
            .throwable_layout_cache
            .backtrace_in_object_enabled()
        {
            self.store_throwable_backtrace(throwable, frames);
            return;
        }
        if let Some(captured_at) = self.try_install_backtrace_holder(throwable, &frames) {
            self.store_throwable_backtrace_captured_at(throwable, frames, captured_at);
            self.point_backtrace_at(throwable, None);
        }
    }

    fn try_install_backtrace_holder(
        &self,
        throwable: ObjectRef,
        frames: &[BacktraceFrame],
    ) -> Option<u64> {
        let captured_at = cratonvm_classloading::class_redefinition_count();
        // A traced pool error (`exceptions::pooled_traced_oome`) is handed out
        // on a heap that just refused an allocation; its trace stays in the
        // registry, so the holder takes none of that heap's last bytes
        // (HotSpot preallocates each pooled error's backtrace with it;
        // `docs/feature-designs/real-jdk-mode.md`).
        let pooled = self.mem.preallocated_oome.read().is_traced(throwable);
        if !pooled && self.install_backtrace_holder(throwable, frames, captured_at) {
            return None;
        }
        Some(captured_at)
    }

    /// Encode `frames` into a new holder and store it in `throwable`'s
    /// `backtrace`. `false` when any step refuses: a frame without an id, the
    /// allocation, the throwable's slot. A holder allocated before a later
    /// refusal is garbage like any unreferenced array.
    #[inline(never)]
    pub(in crate::vm::vm_init) fn install_backtrace_holder(
        &self,
        throwable: ObjectRef,
        frames: &[BacktraceFrame],
        captured_at: u64,
    ) -> bool {
        let Some(words) = self
            .threads
            .throwable_layout_cache
            .frame_ids
            .encode(frames, captured_at)
        else {
            return false;
        };
        let heap = &self.mem.heap;
        // The native allocation path never collects (`try_alloc_array_full`),
        // so neither `throwable` nor the holder moves before the field store
        // below. A full heap refuses: the registry keeps this capture.
        let Some(holder) =
            heap.try_alloc_array_full(ClassId::new(0), ArrayElementType::Long, words.len())
        else {
            return false;
        };
        // As `NativeContextImpl::native_array_gc_request_after_alloc`.
        if crate::runtime::env_cache::disable_jit() && heap.needs_gc() {
            self.mem
                .native_array_gc_requested
                .store(true, Ordering::Relaxed);
        }
        let Some(data) = heap.array_data_ptr(holder) else {
            return false;
        };
        // SAFETY: `holder` is a `long[]` of `words.len()` elements allocated
        // just above, with no safepoint since; its payload starts at `data`.
        // A byte copy, so no alignment is assumed.
        unsafe {
            std::ptr::copy_nonoverlapping(
                words.as_ptr().cast::<u8>(),
                data,
                words.len() * std::mem::size_of::<u64>(),
            );
        }
        self.point_backtrace_at(throwable, Some(holder))
    }

    /// Store `holder` -- or, for `None`, the registry's self-reference marker
    /// -- in `throwable`'s `backtrace` (the slot `java/lang/Throwable`
    /// declares). `false`, nothing written, when the object or the slot cannot
    /// be confirmed.
    pub(in crate::vm::vm_init) fn point_backtrace_at(&self, throwable: ObjectRef, holder: Option<ObjectRef>) -> bool {
        let Some(slot) = crate::runtime::exceptions::throwable_backtrace_slot(self) else {
            return false;
        };
        let heap = &self.mem.heap;
        let (obj, validated) = heap.load_and_forward_checked(throwable);
        if !validated
            || heap.kind_of(obj) != crate::memory::heap::ObjectKind::Object
            || slot >= heap.get_header(obj).num_slots() as usize
        {
            return false;
        }
        heap.set_field_as(obj, slot, Value::Object(Some(holder.unwrap_or(obj))), b'L');
        true
    }

    /// `Object.clone()` of a throwable whose retained trace lives in the
    /// registry (page `i82-L2-a-cloned-throwable-does-not-keep-its-trace`).
    ///
    /// The field copy left the clone's `backtrace` naming the ORIGINAL (the
    /// registry's self-reference marker), so every reader of the clone read
    /// the original's row, and a later `fillInStackTrace()` on the original
    /// (which replaces that row) moved the clone's trace too. HotSpot's clone
    /// holds the original's backtrace object, which the original's refill
    /// replaces only in the original. So the clone gets a row of its own --
    /// the same shared frames under the same capture stamp -- and its own
    /// marker. A holder (`CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT=1`) is
    /// already the clone's own copy and is left alone, as is any other value.
    pub(crate) fn clone_throwable_backtrace(&self, original: ObjectRef, clone: ObjectRef) {
        let Some(slot) = crate::runtime::exceptions::throwable_backtrace_slot(self) else {
            return;
        };
        let heap = &self.mem.heap;
        let (clone, clone_ok) = heap.load_and_forward_checked(clone);
        let (original, original_ok) = heap.load_and_forward_checked(original);
        if !clone_ok || !original_ok || slot >= heap.get_header(clone).num_slots() as usize {
            return;
        }
        // One slot read for every cloned object; the class test only when the
        // slot names the original, which a non-throwable's slot almost never does.
        if heap.get_field_as(clone, slot, b'L') != Value::Object(Some(original)) {
            return;
        }
        {
            let cm = self.classes.class_manager.read();
            let Some(throwable) = cm.get_loaded_class_id("java/lang/Throwable") else {
                return;
            };
            if !cm.is_subclass_of(heap.class_id_of(clone), throwable) {
                return;
            }
        }
        self.give_clone_its_own_trace(original, clone, slot);
    }

    /// [`Self::clone_throwable_backtrace`] once `clone` is known to be a
    /// throwable whose `backtrace` (slot `slot`) names `original`: copy the
    /// original's registry row (shared frames, same stamp) to the clone and
    /// mark the clone with itself. Nothing when the original has no row.
    pub(in crate::vm::vm_init) fn give_clone_its_own_trace(&self, original: ObjectRef, clone: ObjectRef, slot: usize) {
        let heap = &self.mem.heap;
        let Some((frames, captured_at)) = self
            .with_registry_trace(original.as_ptr() as usize, |t| {
                (Arc::clone(&t.frames), t.captured_at)
            })
        else {
            return;
        };
        self.store_throwable_backtrace_shared_at(clone, frames, captured_at);
        heap.set_field_as(clone, slot, Value::Object(Some(clone)), b'L');
    }

    /// The registry is about to hold `throwable`'s trace: when its
    /// `backtrace` names a holder, point it at the registry (the
    /// self-reference marker) so the readers find the row. Any other value is
    /// left as its writer stored it (a cleared preallocated error's `null`
    /// stays `null`).
    pub(in crate::vm::vm_init) fn retire_backtrace_holder(&self, throwable: ObjectRef) {
        use crate::vm::realms::thread_realm::UNRESOLVED_THROWABLE_FIELD_INDEX;
        // A holder is only ever written through the resolved slot.
        let slot = self
            .threads
            .throwable_layout_cache
            .backtrace
            .load(Ordering::Relaxed);
        if slot == UNRESOLVED_THROWABLE_FIELD_INDEX {
            return;
        }
        let heap = &self.mem.heap;
        let (obj, validated) = heap.load_and_forward_checked(throwable);
        if !validated || self.backtrace_holder_of(obj).is_none() {
            return;
        }
        heap.set_field_as(obj, slot, Value::Object(Some(obj)), b'L');
    }

    /// The backtrace holder `throwable` names: `throwable` itself when it is
    /// one (`StackTraceElement.of(backtrace, depth)` passes the holder), or
    /// the holder in its `backtrace`. `None` for anything else: the registry
    /// marker, `null`, no such slot, a reference the heap does not know.
    pub(crate) fn backtrace_holder_of(&self, throwable: ObjectRef) -> Option<ObjectRef> {
        use crate::memory::heap::ObjectKind;
        use crate::vm::realms::thread_realm::UNRESOLVED_THROWABLE_FIELD_INDEX;
        let heap = &self.mem.heap;
        let (obj, validated) = heap.load_and_forward_checked(throwable);
        if !validated {
            return None;
        }
        match heap.kind_of(obj) {
            ObjectKind::Array => return self.is_backtrace_holder(obj).then_some(obj),
            ObjectKind::Object => {}
            _ => return None,
        }
        let slot = self
            .threads
            .throwable_layout_cache
            .backtrace
            .load(Ordering::Relaxed);
        if slot == UNRESOLVED_THROWABLE_FIELD_INDEX
            || slot >= heap.get_header(obj).num_slots() as usize
        {
            return None;
        }
        let Value::Object(Some(candidate)) = heap.get_field_as(obj, slot, b'L') else {
            return None;
        };
        if candidate == obj {
            return None;
        }
        let (candidate, validated) = heap.load_and_forward_checked(candidate);
        (validated
            && heap.kind_of(candidate) == ObjectKind::Array
            && self.is_backtrace_holder(candidate))
        .then_some(candidate)
    }

    /// Is `array` (a validated array) a backtrace holder: a `long[]` that
    /// starts with `backtrace_ids::HOLDER_MAGIC`?
    pub(in crate::vm::vm_init) fn is_backtrace_holder(&self, array: ObjectRef) -> bool {
        use crate::vm::realms::backtrace_ids::{is_holder_words, HOLDER_HEADER_WORDS};
        let heap = &self.mem.heap;
        if heap.array_element_type(array) != Some(ArrayElementType::Long) {
            return false;
        }
        let len = heap.array_length(array);
        if len < HOLDER_HEADER_WORDS {
            return false;
        }
        let Some(data) = heap.array_data_ptr(array) else {
            return false;
        };
        // SAFETY: a live `long[]` of at least two elements; word 0 is in
        // bounds. Unaligned read: the payload offset is the heap's.
        let first = unsafe { std::ptr::read_unaligned(data.cast::<u64>()) };
        is_holder_words(first, len)
    }

    /// `holder`'s trace as a registry record would hold it (`throwable` is the
    /// reader's handle, for its own checks). `None` when a word names no frame
    /// id this VM handed out.
    pub(in crate::vm::vm_init) fn decode_backtrace_holder(
        &self,
        throwable: ObjectRef,
        holder: ObjectRef,
    ) -> Option<ThrowableStackTrace> {
        use crate::vm::realms::backtrace_ids::{is_holder_words, HOLDER_HEADER_WORDS};
        let heap = &self.mem.heap;
        let len = heap.array_length(holder);
        let data = heap.array_data_ptr(holder)?;
        let mut words = Vec::with_capacity(len);
        for index in 0..len {
            let at = index * std::mem::size_of::<u64>();
            // SAFETY: `index` is below the holder's length; an unaligned
            // read at the element's byte offset.
            words.push(unsafe { std::ptr::read_unaligned(data.add(at).cast::<u64>()) });
        }
        if !words
            .first()
            .is_some_and(|&first| is_holder_words(first, len))
        {
            return None;
        }
        let frames = self
            .threads
            .throwable_layout_cache
            .frame_ids
            .decode(&words[HOLDER_HEADER_WORDS..])?;
        Some(ThrowableStackTrace {
            throwable,
            frames: frames.into(),
            captured_at: words[1],
        })
    }

    /// Forward live registry handles after a relocating collection and discard
    /// entries whose Throwable was collected. The registry deliberately does
    /// not keep its key object alive; the liveness verdict for an entry that
    /// did not move is `memory::addr_keyed::survived_in_place`.
    ///
    /// See [`remap_and_sweep_throwable_shards`] for the two-phase shape and
    /// why it replaced a one-pass loop (gc-common w2-g, 2026-09-23): the old
    /// loop re-inserted a moved entry into a HIGHER shard it had not drained
    /// yet, so that shard's pass looked the entry up in the same pointer map a
    /// second time; under a sliding compactor's chained map (`A -> B`,
    /// `B -> C`) the trace of the Throwable at `A` ended up keyed at `C`.
    ///
    /// No two shard locks are ever held together any more, so the old
    /// "SINGLE-THREADED INVARIANT" on lock nesting is gone; the function is
    /// still only called under stop-the-world (`update_all_roots`).
    ///
    /// `survived` is the caller's survival verdict: `gc::update_all_roots`'
    /// once-per-pause `addr_keyed::InPlaceVerdict` (gc-common w18-a), the
    /// same answer as `survived_in_place` without one young free-list copy
    /// and sort per dead row. The per-address form this replaced had no other
    /// caller.
    pub(crate) fn remap_and_sweep_throwable_stack_traces_with(
        &self,
        pointer_map: &cratonvm_types::PointerMap,
        survived: &dyn Fn(usize) -> bool,
    ) {
        remap_and_sweep_throwable_shards(&self.threads.throwable_stacks, pointer_map, survived);
        self.threads
            .throwable_layout_cache
            .reset_retained_trace_frames();
    }

    /// Sweep-only pass over the backtrace table: drop the traces whose
    /// Throwable did not survive, move nothing. For a reclamation that is not
    /// followed by `update_all_roots` (see
    /// `memory::addr_keyed::sweep_address_keyed_tables`). Safe outside a pause:
    /// each shard is filtered in place under its own write lock, and a
    /// concurrent `store_throwable_stack_trace` for a new object allocated on a
    /// just-freed address either lands before the filter (the address is a
    /// live object, the row is kept -- and it is the new row) or after it.
    ///
    /// Production caller since gc-common w3-g:
    /// `addr_keyed::sweep_address_keyed_tables`.
    pub(crate) fn sweep_throwable_stack_traces(&self) {
        let heap = &self.mem.heap;
        self.sweep_throwable_stack_traces_with(&|addr| {
            crate::memory::addr_keyed::survived_in_place(heap, addr)
        });
    }

    /// [`Self::sweep_throwable_stack_traces`] with the caller's verdict
    /// (gci i22/g1rk: `addr_keyed::sweep_address_keyed_tables_with`, G1's
    /// mark-exact sweep after a trusted remark cleanup). Nothing moves.
    pub(crate) fn sweep_throwable_stack_traces_with(&self, survived: &dyn Fn(usize) -> bool) {
        remap_and_sweep_throwable_shards(
            &self.threads.throwable_stacks,
            &cratonvm_types::PointerMap::default(),
            survived,
        );
        self.threads
            .throwable_layout_cache
            .reset_retained_trace_frames();
    }
}

/// Wire this VM's `ConcurrentGcState` owed-cycle transitions to its
/// `GcBarrier` poll page using `Weak<SharedVm>` so there is no reference cycle
/// and no process-global state.
pub(in crate::vm::vm_init) fn wire_owed_poll_sync(shared: &Arc<SharedVm>) {
    let weak = Arc::downgrade(shared);
    shared
        .mem
        .concurrent_gc_state
        .set_poll_sync_hook(Box::new(move || {
            if let Some(vm) = weak.upgrade() {
                vm.mem
                    .gc_barrier
                    .sync_owed_poll_page(&|| vm.mem.concurrent_gc_state.inline_start_requested());
            }
        }));
    shared
        .mem
        .gc_barrier
        .sync_owed_poll_page(&|| shared.mem.concurrent_gc_state.inline_start_requested());
}
