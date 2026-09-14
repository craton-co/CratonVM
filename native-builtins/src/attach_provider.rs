// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `sun/tools/attach/AttachProviderImpl` — the two natives the **Windows**
//! attach provider needs before `VirtualMachine.list()` can answer at all.
//!
//! # Why this file exists, and why it is Windows-only
//!
//! `com.sun.tools.attach.VirtualMachine.list()` is the entry point every
//! JVM-enumerating tool takes (`jps`/`jcmd`-shaped code, Spring Boot devtools,
//! self-attaching JMX agents). On Linux `sun.tools.attach.AttachProviderImpl`
//! declares no natives on the `listVirtualMachines()` path — it delegates
//! straight to `HotSpotAttachProvider.listVirtualMachines()`, which is ordinary
//! bytecode — so the call has always worked on this VM without anything here.
//!
//! The **Windows** class of the same name is a different class with a different
//! shape (JDK 25, `javap -p --module jdk.attach sun.tools.attach.AttachProviderImpl`):
//!
//! ```text
//! private static native java.lang.String tempPath();
//! private static native long   volumeFlags(java.lang.String);
//! private static native int    enumProcesses(int[], int);
//! private static native boolean isLibraryLoadedByProcess(java.lang.String, int);
//! ```
//!
//! and `listVirtualMachines()` gates on the first two:
//!
//! ```text
//! 0: invokestatic  isTempPathSecure:()Z
//! 3: ifeq          11
//! 6: invokespecial HotSpotAttachProvider.listVirtualMachines   <-- the Linux path
//! 11: invokevirtual listJavaProcesses                          <-- the other two
//! ```
//!
//! `isTempPathSecure()` calls `tempPath()`, requires a `X:\`-shaped answer, and
//! asks `volumeFlags()` for that root's `FILE_PERSISTENT_ACLS` bit. With no
//! bridge for `tempPath()` the whole call died before any of that:
//!
//! ```text
//! java.lang.UnsatisfiedLinkError: sun/tools/attach/AttachProviderImpl.tempPath()Ljava/lang/String;
//!     at sun.tools.attach.AttachProviderImpl.isTempPathSecure(AttachProviderImpl.java:80)
//!     at sun.tools.attach.AttachProviderImpl.listVirtualMachines(AttachProviderImpl.java:64)
//!     at com.sun.tools.attach.VirtualMachine.list(VirtualMachine.java:146)
//! ```
//!
//! measured on JDK 25 in **both** `--real-jdk` and `--jdk-only`, by
//! `apps/probes/JdkOnlyPlatformProbe.java`'s `agent` section
//! (`attach=throw-UnsatisfiedLinkError` where HotSpot says `attach=list-ok`).
//! It is the whole reason the strict-corpus gate had no `25-windows` key: the
//! Linux baseline carries no `agent` row because on Linux there is no such
//! native to miss.
//!
//! # Why these two and not all four
//!
//! Answering the first two truthfully routes the call onto
//! `HotSpotAttachProvider.listVirtualMachines()` — the same bytecode the Linux
//! leg already runs green — so the platforms converge on ONE code path instead
//! of this VM growing a second, Windows-only enumeration of its own.
//! `enumProcesses` / `isLibraryLoadedByProcess` are reached only when the temp
//! path is NOT ACL-protected (a FAT/exFAT `%TEMP%`, which NTFS is not), and a
//! fabricated answer for either would be a list of processes this VM invented.
//! They stay unregistered, so that path still fails loudly and names itself —
//! a narrower gap than the one this file closes, and an honest one.
//!
//! # No new dependency
//!
//! `GetVolumeInformationW` is declared here rather than pulled in with a
//! `windows-sys` dependency: one `extern "system"` block against `kernel32`,
//! which the MSVC and GNU targets both link by default, keeps this crate's
//! dependency graph (and the supply-chain gate that reads it) unchanged.

#![cfg(windows)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

/// `sun/tools/attach/AttachProviderImpl` — the Windows class. The Linux class
/// of the same name declares neither of these methods, which is why every
/// registration below is `#![cfg(windows)]`: a `Bridge` naming a method the
/// image does not have is exactly what the bridge ratchet exists to stop.
const FQN: &str = "sun/tools/attach/AttachProviderImpl";

/// `FILE_PERSISTENT_ACLS`, the bit `isTempPathSecure()` tests. Named here only
/// so the constant this file DOES NOT interpret is documented: the flags word
/// is handed back whole and the JDK's own bytecode does the masking.
const FILE_PERSISTENT_ACLS: u32 = 0x0000_0008;

extern "system" {
    /// `kernel32!GetVolumeInformationW`. Only the file-system flags out-param
    /// is wanted; every buffer is passed null, which the API documents as
    /// "not interested in this one".
    fn GetVolumeInformationW(
        lp_root_path_name: *const u16,
        lp_volume_name_buffer: *mut u16,
        n_volume_name_size: u32,
        lp_volume_serial_number: *mut u32,
        lp_maximum_component_length: *mut u32,
        lp_file_system_flags: *mut u32,
        lp_file_system_name_buffer: *mut u16,
        n_file_system_name_size: u32,
    ) -> i32;
}

/// `private static native String tempPath()`.
///
/// The JDK's C body is `GetTempPath`, and `std::env::temp_dir()` on Windows is
/// that same call (`GetTempPath2W`, falling back to `GetTempPathW`) — so this
/// is the platform's answer rather than a re-derivation from `java.io.tmpdir`,
/// which a program can set to anything and which would make the security
/// verdict below depend on a system property.
///
/// The caller wants a `X:\`-shaped string: it checks `length() >= 3`,
/// `charAt(1) == ':'` and `charAt(2) == '\\'` before using `substring(0, 3)`
/// as a volume root. A path that does not have that shape (a UNC `\\server\share`
/// temp directory) makes `isTempPathSecure()` answer false, which is the
/// conservative direction and the JDK's own.
fn temp_path(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let dir = std::env::temp_dir();
    let mut s = dir.to_string_lossy().into_owned();
    // `GetTempPath` guarantees a trailing separator; `env::temp_dir()` strips
    // it. Nothing in `isTempPathSecure()` reads past index 2, but a caller that
    // treats the result as a directory prefix would silently concatenate.
    if !s.ends_with('\\') && !s.ends_with('/') {
        s.push('\\');
    }
    let obj = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(obj))))
}

/// `private static native long volumeFlags(String path)`.
///
/// Returns `GetVolumeInformationW`'s `lpFileSystemFlags` word, widened to the
/// `long` the declaration promises. The JDK masks it itself, so nothing here
/// interprets the value — see [`FILE_PERSISTENT_ACLS`].
///
/// **A failure answers 0, not an exception.** That is the JDK's own contract
/// for this native (its C body returns 0 when `GetVolumeInformation` fails) and
/// it is also the safe direction: 0 has no `FILE_PERSISTENT_ACLS` bit, so an
/// unreadable volume reads as "not secure" rather than as "secure".
fn volume_flags(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(p))) = args.first() else {
        return Ok(Some(Value::Long(0)));
    };
    let Some(path) = ctx.read_string(*p) else {
        return Ok(Some(Value::Long(0)));
    };
    let mut wide: Vec<u16> = path.encode_utf16().collect();
    wide.push(0);
    let mut flags: u32 = 0;
    // SAFETY: `wide` is NUL-terminated and outlives the call; every buffer
    // out-param is null with a zero length, which `GetVolumeInformationW`
    // documents as "do not report this field"; `flags` is a live `u32`.
    let ok = unsafe {
        GetVolumeInformationW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags,
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return Ok(Some(Value::Long(0)));
    }
    Ok(Some(Value::Long(i64::from(flags))))
}

/// Wave coordinator entry point; wired from `lib.rs`.
pub fn register_attach_provider(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    // `Bridge`, not `Intrinsic` and not `SyntheticStub`: each of these stands
    // in for a real `ACC_NATIVE` method of the Windows runtime image and
    // answers what the platform answers, so `--jdk-only` keeps both.
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(FQN, "tempPath", "()Ljava/lang/String;", temp_path);
    r.register(FQN, "volumeFlags", "(Ljava/lang/String;)J", volume_flags);
    r.set_category(__prev_cat);
    let _ = FILE_PERSISTENT_ACLS;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `tempPath()` must come back in the shape `isTempPathSecure()` parses,
    /// because every other reading of it is a silent "not secure".
    ///
    /// The JDK checks `length() >= 3 && charAt(1) == ':' && charAt(2) == '\\'`
    /// and then uses `substring(0, 3)` as the volume root. A run whose `%TEMP%`
    /// is a UNC path legitimately fails that test, so this asserts the shape
    /// only when the drive-letter form is what the OS actually returned —
    /// asserting it unconditionally would be a test about the host.
    #[test]
    fn temp_path_is_shaped_for_the_caller_that_parses_it() {
        let dir = std::env::temp_dir();
        let mut s = dir.to_string_lossy().into_owned();
        if !s.ends_with('\\') && !s.ends_with('/') {
            s.push('\\');
        }
        let b: Vec<char> = s.chars().collect();
        assert!(
            b.len() >= 3,
            "temp_dir() answered {s:?}, which is shorter than the three characters \
             `isTempPathSecure()` reads before it does anything else"
        );
        if b[1] == ':' {
            assert_eq!(
                b[2], '\\',
                "temp_dir() answered {s:?}: a drive-letter path whose third character is not a \
                 backslash makes isTempPathSecure() answer false without ever calling \
                 volumeFlags, so VirtualMachine.list() silently takes the unregistered \
                 listJavaProcesses path instead"
            );
        }
        assert!(
            s.ends_with('\\') || s.ends_with('/'),
            "GetTempPath guarantees a trailing separator and this native promises the same; \
             {s:?} has none"
        );
    }

    /// The volume the temp path lives on must report flags, and on any NTFS
    /// volume it must report `FILE_PERSISTENT_ACLS` — which is the bit that
    /// decides whether `listVirtualMachines()` takes the shared bytecode path
    /// or the one whose two natives are deliberately unregistered.
    ///
    /// Scored as "flags are readable", not "the bit is set": a CI runner is
    /// free to hand us a temp directory on a volume without persistent ACLs,
    /// and this test must report on the call rather than on the runner's disk.
    #[test]
    fn volume_flags_reads_the_temp_volume() {
        let dir = std::env::temp_dir();
        let s = dir.to_string_lossy().into_owned();
        let b: Vec<char> = s.chars().collect();
        if b.len() < 3 || b[1] != ':' {
            // Not the drive-letter shape; the JDK would not call volumeFlags
            // at all, so there is nothing here to measure.
            return;
        }
        let root: String = b[..3].iter().collect();
        let mut wide: Vec<u16> = root.encode_utf16().collect();
        wide.push(0);
        let mut flags: u32 = 0;
        let ok = unsafe {
            GetVolumeInformationW(
                wide.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut flags,
                std::ptr::null_mut(),
                0,
            )
        };
        assert_ne!(
            ok, 0,
            "GetVolumeInformationW failed for {root:?}. The native answers 0 on failure, which \
             reads as `not secure` and routes VirtualMachine.list() onto the two natives this \
             file deliberately does not register."
        );
        assert_ne!(
            flags, 0,
            "GetVolumeInformationW succeeded for {root:?} and reported a flags word of zero. \
             That is not a plausible file system and would mean the out-param was never \
             written -- the argument order in the extern block is the thing to check."
        );
    }
}
