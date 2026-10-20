// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The platform default of `java.io.tmpdir` — the value HotSpot's
//! `java_props_md.c` puts in `sprops.tmp_dir` before any `-D` is applied.
//!
//! # Why this is not `std::env::temp_dir()`
//!
//! On Unix, `std::env::temp_dir()` answers `$TMPDIR` when it is set. HotSpot
//! never reads `$TMPDIR`: Linux answers `P_tmpdir` (`/tmp`) unconditionally,
//! and macOS answers `confstr(_CS_DARWIN_USER_TEMP_DIR)`, falling back to
//! `P_tmpdir`. The only override HotSpot honours is `-Djava.io.tmpdir=...`,
//! which the property table applies on top of this value.
//!
//! The difference is observable, not cosmetic. MEASURED 2026-09-24 on the
//! Azure host, JDK 25.0.4: with `TMPDIR=/nonexistent/x`, HotSpot reports
//! `java.io.tmpdir=/tmp` and a `RecordingStream` delivers its event, while
//! CratonVM reported `/nonexistent/x` and `new RecordingStream()` threw
//! `IllegalStateException: Can't create Flight Recorder` out of
//! `jdk.jfr.internal.Repository.setBasePath`; `TMPDIR=` (empty) made CratonVM
//! report an empty `java.io.tmpdir` where HotSpot reports `/tmp`. See
//! `docs/internal/fixed-suite-bugs/netty/jfr-events-repository-basepath-npe-FIXED-20260924.md`.
//!
//! Windows keeps `std::env::temp_dir()`: HotSpot calls `GetTempPathW`, and the
//! `GetTempPath2W` std uses answers the same for every non-SYSTEM process.

/// `P_tmpdir` from `<stdio.h>` on glibc, musl and Darwin.
#[cfg(unix)]
const P_TMPDIR: &str = "/tmp";

/// The platform default of `java.io.tmpdir`. Never empty.
pub fn java_io_tmpdir() -> String {
    platform_tmpdir()
}

#[cfg(windows)]
fn platform_tmpdir() -> String {
    std::env::temp_dir().to_string_lossy().into_owned()
}

#[cfg(target_os = "macos")]
fn platform_tmpdir() -> String {
    use std::ffi::{c_char, c_int, CStr};
    // Declared here rather than taken from `libc`, for the same reason
    // `os_encoding` declares its five kernel32 entry points: this is the whole
    // surface. `confstr` is in libSystem on every macOS release.
    extern "C" {
        fn confstr(name: c_int, buf: *mut c_char, len: usize) -> usize;
    }
    /// `<unistd.h>`: `#define _CS_DARWIN_USER_TEMP_DIR 65537`.
    const _CS_DARWIN_USER_TEMP_DIR: c_int = 65537;
    let mut buf = [0 as c_char; 1024];
    // SAFETY: `buf` is writable for `buf.len()` bytes; `confstr` writes at most
    // that many, NUL-terminated, and returns the size it needed.
    let needed = unsafe { confstr(_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr(), buf.len()) };
    if needed > 0 && needed <= buf.len() {
        // SAFETY: `confstr` succeeded, so `buf` holds a NUL-terminated string.
        let dir = unsafe { CStr::from_ptr(buf.as_ptr()) };
        let dir = dir.to_string_lossy();
        if !dir.is_empty() {
            return dir.into_owned();
        }
    }
    P_TMPDIR.to_owned()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_tmpdir() -> String {
    P_TMPDIR.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_io_tmpdir_is_never_empty() {
        assert!(!java_io_tmpdir().is_empty());
    }

    /// HotSpot on Linux answers `P_tmpdir` whatever `$TMPDIR` says.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_answers_p_tmpdir_not_the_environment() {
        assert_eq!(java_io_tmpdir(), "/tmp");
    }
}
