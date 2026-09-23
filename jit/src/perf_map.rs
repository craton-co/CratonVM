// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `perf-<pid>.map`: the plain-text symbol map `perf`, `samply` and other
//! profilers read to name JIT frames (`CRATONVM_JIT_PERF_MAP`, default-OFF).
//!
//! One line per published region, `<hex start> <hex size> <name>\n`, written
//! to `/tmp/perf-<pid>.map` on Unix and `%TEMP%\perf-<pid>.map` on Windows.
//! The file is truncated when the sink first opens (a stale map left by an
//! earlier process with a recycled pid would otherwise name its code), then
//! appended to for the life of the process.
//!
//! Lines are never withdrawn. When an address is freed and reused, the later
//! line covering it wins in perf's lookup, so the map stays correct for
//! samples taken after the reuse.
//!
//! Every line is flushed as soon as it is written, because the process can die
//! at any moment and a buffered tail would be lost exactly when it matters.
//! Any I/O error disables the sink for the rest of the process, with a single
//! stderr line; the sink never panics.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

type Writer = std::io::BufWriter<std::fs::File>;

/// Set once the sink has given up; never cleared.
static FAILED: AtomicBool = AtomicBool::new(false);

/// The open map, or `None` when opening or a write failed.
static SINK: OnceLock<Mutex<Option<Writer>>> = OnceLock::new();

/// `CRATONVM_JIT_PERF_MAP`, read once.
fn requested() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        crate::code_events::flag_is_on(cratonvm_types::flags::runtime_var("CRATONVM_JIT_PERF_MAP"))
    })
}

/// Whether publication events should be written to the map.
pub fn enabled() -> bool {
    requested() && !FAILED.load(Ordering::Relaxed)
}

/// Where the map for process `pid` lives.
///
/// `CRATONVM_JIT_PERF_MAP_DIR` overrides the directory. The default stays
/// `/tmp` on Unix because `perf` builds that path itself and will not look
/// anywhere else; see [`open_map`] for why this is an explicit opt-out rather
/// than an `$XDG_RUNTIME_DIR` default.
pub fn map_path(pid: u32) -> std::path::PathBuf {
    let name = format!("perf-{pid}.map");
    if let Some(dir) =
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_PERF_MAP_DIR").filter(|d| !d.is_empty())
    {
        return std::path::PathBuf::from(dir).join(name);
    }
    #[cfg(windows)]
    {
        std::env::temp_dir().join(name)
    }
    #[cfg(not(windows))]
    {
        std::path::PathBuf::from("/tmp").join(name)
    }
}

/// Write one map line. Line breaks in `name` are dropped so one region can
/// never become two lines; spaces are kept, since perf reads the rest of the
/// line as the symbol.
pub fn write_line<W: Write>(
    out: &mut W,
    start: usize,
    len: usize,
    name: &str,
) -> std::io::Result<()> {
    write!(out, "{start:x} {len:x} ")?;
    for piece in name.split(['\n', '\r']) {
        out.write_all(piece.as_bytes())?;
    }
    out.write_all(b"\n")
}

fn fail(what: std::fmt::Arguments<'_>) {
    if !FAILED.swap(true, Ordering::SeqCst) {
        // `eprintln!` panics when stderr is gone; this path must not.
        let _ = writeln!(std::io::stderr(), "[cratonvm] perf map disabled: {what}");
    }
}

/// Open the map, refusing to follow a symlink at the destination.
///
/// # Why `O_NOFOLLOW`
///
/// The path is PREDICTABLE (`/tmp/perf-<pid>.map`) and the directory is
/// world-writable. Opening it `create(true).truncate(true)` through a
/// pre-planted symlink truncates whatever the link points at, with this
/// process's privileges. Severity is low — the sink is opt-in
/// (`CRATONVM_JIT_PERF_MAP`) and HotSpot's own perf map has the same shape —
/// but a JVM is often the only process in a container that writes a
/// predictable `/tmp` path, and the fix is one flag.
///
/// `O_NOFOLLOW` fails with `ELOOP` when the final component is a symlink,
/// which reaches [`fail`] and disables the sink for the process with one
/// stderr line. Refusing to profile is the right answer to "someone has
/// arranged for me to truncate a file I did not name".
///
/// # Why NOT `$XDG_RUNTIME_DIR`
///
/// Moving the default out of `/tmp` would be the obvious hardening and it
/// would break the tool. `perf` looks for this file at a path it builds
/// itself — `/tmp/perf-<pid>.map`, hard-coded — so a map written anywhere else
/// names nothing. `CRATONVM_JIT_PERF_MAP_DIR` is the escape for an operator
/// whose profiler can be pointed elsewhere (`samply`, a container with a
/// private `/tmp`); it is deliberately explicit rather than inferred from the
/// environment, because silently writing a map `perf` cannot find is a worse
/// failure than writing one it can.
fn open_map(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(o_nofollow());
    }
    options.open(path)
}

/// `O_NOFOLLOW` without a `libc` dependency in this crate.
///
/// The value is fixed by each platform's ABI and has never changed; it is
/// pinned here per target rather than linked, because `cratonvm-jit` does not
/// depend on `libc` and adding that edge for one constant is the wrong trade.
/// An unrecognised Unix gets `0`, which is the pre-existing behaviour rather
/// than a wrong flag — fail-open on the flag, never on the path.
///
/// Shared with [`crate::jitdump`], whose sink writes an equally predictable
/// name into the same world-writable directory.
#[cfg(unix)]
pub(crate) const fn o_nofollow() -> i32 {
    #[cfg(target_os = "linux")]
    {
        0o400000
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        0x0100
    }
    #[cfg(any(target_os = "freebsd", target_os = "netbsd", target_os = "openbsd"))]
    {
        0x0100
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    {
        0
    }
}

fn open() -> Option<Writer> {
    let path = map_path(std::process::id());
    match open_map(&path) {
        Ok(file) => Some(std::io::BufWriter::new(file)),
        Err(e) => {
            fail(format_args!("cannot open {}: {e}", path.display()));
            None
        }
    }
}

/// Append `[start, start+len) name`. No-op unless enabled.
pub fn record(start: usize, len: usize, name: &str) {
    if !enabled() {
        return;
    }
    let lock = SINK.get_or_init(|| Mutex::new(open()));
    let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(writer) = guard.as_mut() else {
        return;
    };
    let result = write_line(writer, start, len, name).and_then(|()| writer.flush());
    if let Err(e) = result {
        *guard = None;
        fail(format_args!(
            "write to {} failed: {e}",
            map_path(std::process::id()).display()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(start: usize, len: usize, name: &str) -> String {
        let mut out = Vec::new();
        write_line(&mut out, start, len, name).expect("Vec writes cannot fail");
        String::from_utf8(out).expect("utf-8")
    }

    #[test]
    fn a_line_is_lowercase_hex_start_size_then_the_name() {
        assert_eq!(
            line(0x7f00_dead_b000, 0x1a0, "java/lang/String.hashCode()I [c1]"),
            "7f00deadb000 1a0 java/lang/String.hashCode()I [c1]\n"
        );
    }

    #[test]
    fn spaces_survive_and_line_breaks_do_not() {
        assert_eq!(line(0x10, 0x20, "a b\nc\r\nd"), "10 20 a bcd\n");
    }

    #[test]
    fn exactly_one_newline_per_record() {
        let text = line(1, 2, "x\n\n\ny");
        assert_eq!(text.matches('\n').count(), 1);
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn the_map_is_named_after_the_pid() {
        let path = map_path(4242);
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("perf-4242.map")
        );
        #[cfg(not(windows))]
        assert_eq!(path, std::path::PathBuf::from("/tmp/perf-4242.map"));
    }
}
