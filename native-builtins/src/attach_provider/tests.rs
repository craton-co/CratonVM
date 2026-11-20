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
