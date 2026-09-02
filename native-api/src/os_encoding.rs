// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! What the HOST says its text encoding is — the source of HotSpot's
//! `native.encoding`, `stdout.encoding`, `stderr.encoding` and
//! `stdin.encoding`.
//!
//! # Why this module exists
//!
//! CratonVM used to pin all of those to `"UTF-8"` with the comment *"JDK 18+
//! pinned to UTF-8 for stdout/stderr/file/native"*. That is true of
//! **`file.encoding` only** (JEP 400). The others still follow the platform,
//! and the gap was the open residual of
//! `bug-printstream-charset-answers-the-abstract-base-20260825.md` §5 —
//! *"CratonVM answers UTF-8 where HotSpot answers the console encoding
//! (`Cp1251` on this host, from `stdout.encoding`)"* — which
//! `stdout-encoding-differs-from-hotspot-on-windows-20260901.md` then measured
//! at ten divergent rows in a 273-assertion differential and whose §9
//! recommended exactly this: *"A, with C as its kill switch"* — follow the
//! host, keep an opt-out.
//!
//! MEASURED against Temurin 25.0.3+9, `-XshowSettings:properties`:
//!
//! ```text
//!                        Linux LANG=C.UTF-8   Linux LANG=C      Windows (ACP 1251)
//!   file.encoding        UTF-8                UTF-8             UTF-8
//!   native.encoding      UTF-8                ANSI_X3.4-1968    Cp1251
//!   stdout.encoding      UTF-8                ANSI_X3.4-1968    Cp1251   (redirected)
//!   stdin.encoding       UTF-8                ANSI_X3.4-1968    cp866    (a console)
//! ```
//!
//! Two facts fall out of that table and drive the whole implementation:
//!
//! * **On Unix all of them are the locale's codeset.** There is no separate
//!   "console" encoding to ask for, and redirecting the stream does not change
//!   the answer.
//! * **On Windows they are not one value.** `native.encoding` comes from the
//!   ANSI code page (`GetACP`); a std stream that is *attached to a console*
//!   reports that console's code page instead, which is why `stdin.encoding`
//!   reads `cp866` above while `stdout.encoding`, redirected into a pipe,
//!   falls back to the ACP name.
//!
//! The observable consequence is not the property string, it is the bytes:
//! under `LANG=C`, `System.out.print("Ж")` writes `?` on HotSpot (US-ASCII
//! cannot map it) and used to write the two UTF-8 bytes `D0 96` here.
//!
//! # The two kill switches
//!
//! Both restore the pre-2026-09-01 constant exactly, in one binary, so the
//! change can be A/B'd on any host:
//!
//! * `CRATONVM_NATIVE_ENCODING=<name>` — pins [`native_encoding`].
//! * `CRATONVM_STDOUT_ENCODING=<name>` — pins all three [`stream_encoding`]
//!   answers, and with them the `Charset` `install_charset` stamps on
//!   `System.out` / `System.err`, because that stamp is read from the
//!   `stdout.encoding` / `stderr.encoding` PROPERTIES rather than from here.
//!
//! # What is NOT claimed
//!
//! The Unix path reports the codeset **verbatim** as `nl_langinfo(CODESET)`
//! gives it. HotSpot has a small table that rewrites a few platform spellings
//! (notably on AIX/Solaris); the two rows this host can produce (`UTF-8` and
//! `ANSI_X3.4-1968`) are passed through by HotSpot too, and a row that cannot
//! be measured is not worth guessing at.
//!
//! `sun.jnu.encoding` is deliberately NOT derived from here. It decides how
//! FILE NAMES are encoded, so moving it changes class loading rather than
//! printing — a different blast radius, and the staging in
//! `stdout-encoding-differs-from-hotspot-on-windows-20260901.md` §9 keeps it
//! out of this step on purpose.

use std::sync::OnceLock;

/// A kill-switch value, trimmed, or `None` when the variable is unset/empty.
fn pinned(var: &str) -> Option<String> {
    cratonvm_types::flags::runtime_var(var)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The platform's native encoding: HotSpot's `native.encoding`.
#[must_use]
pub fn native_encoding() -> &'static str {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE
        .get_or_init(|| pinned("CRATONVM_NATIVE_ENCODING").unwrap_or_else(detect_native_encoding))
        .as_str()
}

/// Which of the three standard streams an encoding is being asked for. Only
/// Windows distinguishes them; on Unix all three answer [`native_encoding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdStream {
    /// `System.in` — `stdin.encoding`.
    In,
    /// `System.out` — `stdout.encoding`.
    Out,
    /// `System.err` — `stderr.encoding`.
    Err,
}

/// HotSpot's `stdin.encoding` / `stdout.encoding` / `stderr.encoding` for
/// `stream`.
#[must_use]
pub fn stream_encoding(stream: StdStream) -> &'static str {
    static IN: OnceLock<String> = OnceLock::new();
    static OUT: OnceLock<String> = OnceLock::new();
    static ERR: OnceLock<String> = OnceLock::new();
    let cell = match stream {
        StdStream::In => &IN,
        StdStream::Out => &OUT,
        StdStream::Err => &ERR,
    };
    cell.get_or_init(|| {
        pinned("CRATONVM_STDOUT_ENCODING").unwrap_or_else(|| detect_stream_encoding(stream))
    })
    .as_str()
}

// ---------------------------------------------------------------------------
// Unix
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn detect_native_encoding() -> String {
    // The process starts in the "C" locale no matter what the environment says,
    // so `nl_langinfo(CODESET)` answers `ANSI_X3.4-1968` for EVERY locale until
    // `setlocale` has been called. HotSpot's launcher calls
    // `setlocale(LC_ALL, "")` for exactly this reason.
    //
    // We narrow that to `LC_CTYPE`, which is the only category `CODESET` reads,
    // and we put it back afterwards: `LC_NUMERIC` in particular decides whether
    // C-side `printf("%f")` writes `1.5` or `1,5`, and CratonVM links C code it
    // does not own. Leaving the process locale where we found it costs nothing
    // here and cannot surprise them.
    //
    // Asking libc rather than parsing the locale NAME is what makes this exact.
    // A name-parse cannot tell a locale that is installed from one that is only
    // requested: on this host `LANG=en_US.ISO-8859-1` makes HotSpot answer
    // `ANSI_X3.4-1968`, because `setlocale` failed and the process stayed in
    // `C`, where a name-parse answers `ISO-8859-1`.
    //
    // SAFETY: `setlocale`/`nl_langinfo` are called on one thread during
    // bootstrap, behind the `OnceLock` in `native_encoding()`. The `char*`
    // `nl_langinfo` returns is only valid until the next `setlocale`, so it is
    // copied before the restore.
    unsafe {
        let empty = c"";
        let previous = libc::setlocale(libc::LC_CTYPE, empty.as_ptr());
        let codeset = libc::nl_langinfo(libc::CODESET);
        let name = if codeset.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr(codeset)
                .to_string_lossy()
                .into_owned()
        };
        // `previous` points into libc's own storage and stays valid across the
        // `setlocale` above; a null means the first call failed and there is
        // nothing to restore.
        if !previous.is_null() {
            libc::setlocale(libc::LC_CTYPE, previous);
        }
        if name.is_empty() {
            "UTF-8".to_string()
        } else {
            name
        }
    }
}

#[cfg(unix)]
fn detect_stream_encoding(_stream: StdStream) -> String {
    // Unix has no console code page distinct from the locale, and HotSpot
    // answers the same string whether the stream is a tty or a pipe.
    native_encoding().to_string()
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    // Declared here rather than pulled from `windows-sys`: these five are the
    // whole surface, they are stable since NT 3.1, and `native-api` is a
    // dependency of every other crate — adding a platform SDK crate to it to
    // read two code pages would be paid for by the entire build.
    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetACP() -> u32;
        pub fn GetConsoleCP() -> u32;
        pub fn GetConsoleOutputCP() -> u32;
        pub fn GetStdHandle(n_std_handle: u32) -> isize;
        pub fn GetConsoleMode(h: isize, mode: *mut u32) -> i32;
    }
    pub const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    pub const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    pub const STD_ERROR_HANDLE: u32 = -12i32 as u32;
}

/// The charset name HotSpot uses for a Windows ANSI/OEM code page.
///
/// The `Cp<n>` spelling is the general rule and is what a 1251 host reports
/// (MEASURED). The rows above it are the code pages whose JDK charset is NOT
/// named `Cp<n>`; everything else — including every code page this table has
/// never seen — falls through to the rule.
#[cfg(windows)]
#[must_use]
fn windows_acp_name(cp: u32) -> String {
    match cp {
        0 | 65001 => "UTF-8".to_string(),
        874 => "MS874".to_string(),
        932 => "MS932".to_string(),
        936 => "GBK".to_string(),
        949 => "MS949".to_string(),
        950 => "MS950".to_string(),
        1361 => "x-Johab".to_string(),
        20932 => "EUC_JP".to_string(),
        other => format!("Cp{other}"),
    }
}

/// The charset name HotSpot uses for a Windows CONSOLE code page.
///
/// This is deliberately a different rule from [`windows_acp_name`]: the JDK's
/// `getConsoleEncoding()` spells a console code page `ms<n>` in the
/// `874..=950` band and `cp<n>` everywhere else — which is why a Cyrillic
/// console reports the lower-case `cp866` for `stdin.encoding` while the ANSI
/// code page of the same machine reports `Cp1251`.
#[cfg(windows)]
#[must_use]
fn windows_console_name(cp: u32) -> String {
    if (874..=950).contains(&cp) {
        format!("ms{cp}")
    } else {
        format!("cp{cp}")
    }
}

#[cfg(windows)]
fn detect_native_encoding() -> String {
    // SAFETY: `GetACP` takes no arguments and cannot fail.
    windows_acp_name(unsafe { win::GetACP() })
}

#[cfg(windows)]
fn detect_stream_encoding(stream: StdStream) -> String {
    // SAFETY: all three are argument-less code-page queries.
    let (handle_id, cp) = unsafe {
        match stream {
            StdStream::In => (win::STD_INPUT_HANDLE, win::GetConsoleCP()),
            StdStream::Out => (win::STD_OUTPUT_HANDLE, win::GetConsoleOutputCP()),
            StdStream::Err => (win::STD_ERROR_HANDLE, win::GetConsoleOutputCP()),
        }
    };
    if cp != 0 && windows_stream_is_console(handle_id) {
        return windows_console_name(cp);
    }
    // Redirected into a file or a pipe: HotSpot falls back to the ACP name.
    // MEASURED — `java -XshowSettings:properties | grep encoding` on a 1251
    // host reports `stdout.encoding = Cp1251` (ACP) and, in the same run,
    // `stdin.encoding = cp866` (the console still attached to stdin).
    native_encoding().to_string()
}

/// Whether a std handle is attached to a console. `GetConsoleMode` succeeding
/// is the same test the JDK's own console detection uses; it fails for a file,
/// a pipe and a closed handle alike.
#[cfg(windows)]
fn windows_stream_is_console(handle_id: u32) -> bool {
    // SAFETY: `GetStdHandle` returns a borrowed handle that must not be closed,
    // and `GetConsoleMode` only writes through the out-pointer on success.
    unsafe {
        let h = win::GetStdHandle(handle_id);
        if h == 0 || h == -1 {
            return false;
        }
        let mut mode: u32 = 0;
        win::GetConsoleMode(h, &mut mode) != 0
    }
}

// ---------------------------------------------------------------------------
// Anything else
// ---------------------------------------------------------------------------

#[cfg(not(any(unix, windows)))]
fn detect_native_encoding() -> String {
    "UTF-8".to_string()
}

#[cfg(not(any(unix, windows)))]
fn detect_stream_encoding(_stream: StdStream) -> String {
    "UTF-8".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_encoding_is_stable_and_non_empty() {
        let a = native_encoding();
        assert!(!a.is_empty());
        assert_eq!(a, native_encoding(), "cached value must not drift");
    }

    #[cfg(unix)]
    #[test]
    fn unix_streams_all_answer_the_locale_codeset() {
        // The measured HotSpot behaviour: on Unix the three stream encodings
        // and the native encoding are ONE value, tty or not.
        for s in [StdStream::In, StdStream::Out, StdStream::Err] {
            assert_eq!(stream_encoding(s), native_encoding());
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_codeset_is_read_after_setlocale() {
        // Not a tautology: the test host runs under `LANG=C.UTF-8`, and without
        // the `setlocale(LC_CTYPE, "")` in `detect_native_encoding` the process
        // is still in the "C" locale, where `nl_langinfo(CODESET)` answers
        // `ANSI_X3.4-1968`. So this asserts the setlocale call is there.
        //
        // Skipped when the environment really does ask for a C/POSIX locale,
        // where `ANSI_X3.4-1968` is the RIGHT answer, and when either kill
        // switch is pinning the answer.
        if pinned("CRATONVM_NATIVE_ENCODING").is_some() {
            return;
        }
        let asks_for_c = ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
            .is_none_or(|v| v == "C" || v == "POSIX");
        if asks_for_c {
            return;
        }
        assert_ne!(
            native_encoding(),
            "ANSI_X3.4-1968",
            "codeset was read before setlocale()"
        );
    }

    #[cfg(windows)]
    #[test]
    fn acp_spelling_is_uppercase_cp_and_console_spelling_is_lowercase() {
        // The one row that is MEASURED, plus the band rule around it.
        assert_eq!(windows_acp_name(1251), "Cp1251");
        assert_eq!(windows_console_name(866), "cp866");
        assert_eq!(windows_console_name(932), "ms932");
        assert_eq!(windows_console_name(950), "ms950");
        assert_eq!(windows_console_name(1251), "cp1251");
        assert_eq!(windows_acp_name(65001), "UTF-8");
        assert_eq!(windows_acp_name(936), "GBK");
    }
}
