//! Process and thread state a boot image cannot carry in its memory (Phase 5
//! WP6): kernel and C-library state the capture run changed between P0 and P1.
//! The capture run records it at P1 (into its arena, next to the image root);
//! the restored run re-applies it right after the image is mapped.
//!
//! Each piece is its own type with a `take`/`apply` (or `diff`/`apply`) pair:
//!
//! * [`ThreadTls`] -- the calling thread's static TLS block of the main
//!   executable (every `thread_local!` of the launcher binary and of the C code
//!   linked into it). A restored `main-vm` is a fresh OS thread, so its TLS
//!   holds the values the restore run's own prologue left there; the bytes the
//!   capture run's boot wrote P0..P1 are copied back (minus the excluded
//!   symbols, see `statics`).
//! * [`StackBounds`] -- the calling thread's stack, so a key can pin it.
//! * [`FdRecord`] -- descriptors opened P0..P1 and still open at P1: reopened
//!   at the same number from the same path, flags and offset.
//! * [`SignalState`] -- dispositions of every signal (process-wide), and the
//!   calling thread's signal mask and alternate stack.
//! * [`LimitRecord`] -- resource limits the boot raised (`RLIMIT_NOFILE`).
//! * [`EnvChange`] -- environment variables the boot set or removed (the C
//!   library's `environ` is not imaged).
//!
//! No statics (see the module docs of [`super`]). Linux x86_64/aarch64 only;
//! elsewhere every `take` answers `None`/empty and every `apply` is a no-op.

use std::ffi::OsString;
use std::ops::Range;
use std::path::PathBuf;

/// `[addr, addr + len)` of the calling thread's static TLS block for the main
/// executable, or `None` (no `PT_TLS`, or unsupported platform).
pub fn main_object_tls_block() -> Option<(usize, usize)> {
    sys::main_object_tls_block()
}

/// The bytes of the calling thread's main-executable TLS block at one point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadTls {
    pub addr: usize,
    pub bytes: Vec<u8>,
}

/// One run of changed TLS bytes: `(offset into the block, bytes)`.
pub type TlsRun = (usize, Vec<u8>);

impl ThreadTls {
    /// Copy the calling thread's block now.
    pub fn take() -> Option<ThreadTls> {
        let (addr, len) = main_object_tls_block()?;
        let mut bytes = vec![0u8; len];
        // SAFETY: the calling thread's own, live TLS block.
        bytes.copy_from_slice(unsafe { std::slice::from_raw_parts(addr as *const u8, len) });
        Some(ThreadTls { addr, bytes })
    }

    /// The runs that differ between `self` (P0) and the calling thread's block
    /// now (P1), skipping `excludes` (offsets into the block). Runs closer
    /// than 8 bytes are merged.
    pub fn diff(&self, excludes: &[Range<usize>]) -> Vec<TlsRun> {
        let Some(now) = ThreadTls::take() else {
            return Vec::new();
        };
        if now.addr != self.addr || now.bytes.len() != self.bytes.len() {
            return Vec::new();
        }
        let excluded = |i: usize| excludes.iter().any(|r| r.contains(&i));
        let mut runs: Vec<TlsRun> = Vec::new();
        let mut i = 0;
        let n = now.bytes.len();
        while i < n {
            if now.bytes[i] == self.bytes[i] || excluded(i) {
                i += 1;
                continue;
            }
            let start = i;
            let mut last = i;
            while i < n && i - last < 8 {
                if now.bytes[i] != self.bytes[i] && !excluded(i) {
                    last = i;
                }
                i += 1;
            }
            runs.push((start, now.bytes[start..=last].to_vec()));
        }
        runs
    }

    /// Byte offsets (into the block) that differ between `self` and now,
    /// before exclusions: what the static classifier resolves to symbols.
    pub fn changed_ranges(&self) -> Vec<Range<usize>> {
        self.diff(&[]).into_iter().map(|(o, b)| o..o + b.len()).collect()
    }
}

/// Write `runs` into the calling thread's main-executable TLS block, which
/// must be at `expected_addr` (the capture's; the key pins it). `false`, with
/// nothing written, when the block is elsewhere or too small.
///
/// # Safety
/// Overwrites live thread-locals of the calling thread: nothing may hold a
/// borrow of one of them, and every byte written must be valid for its type
/// (the capture run's values of the same binary).
pub unsafe fn apply_tls_runs(expected_addr: usize, runs: &[TlsRun]) -> bool {
    let Some((addr, len)) = main_object_tls_block() else {
        return false;
    };
    if addr != expected_addr || runs.iter().any(|(o, b)| o + b.len() > len) {
        return false;
    }
    for (o, b) in runs {
        // SAFETY: inside the calling thread's TLS block (checked above).
        unsafe { std::ptr::copy_nonoverlapping(b.as_ptr(), (addr + o) as *mut u8, b.len()) };
    }
    true
}

/// The calling thread's stack `[lo, hi)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct StackBounds {
    pub lo: usize,
    pub hi: usize,
}

impl StackBounds {
    pub fn current() -> Option<StackBounds> {
        sys::stack_bounds()
    }
}

/// A descriptor the boot opened and kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FdRecord {
    pub fd: i32,
    pub path: PathBuf,
    /// `open` flags as `/proc/self/fdinfo` reports them (minus creation flags).
    pub flags: i32,
    /// File offset at P1.
    pub pos: u64,
    pub cloexec: bool,
}

/// The descriptors open now (sorted). Empty where unsupported.
pub fn open_fds() -> Vec<i32> {
    sys::open_fds()
}

/// Every descriptor open now that was not in `p0`: a record each, or an error
/// naming the first one that cannot be reopened from a path (a pipe, socket,
/// memfd, deleted file, directory stream ...).
pub fn record_new_fds(p0: &[i32]) -> Result<Vec<FdRecord>, String> {
    sys::record_new_fds(p0)
}

/// Reopen each record at its number. `Err` names the first that failed;
/// descriptors reopened before it stay open (the caller is past the point of
/// no return and reports the failure).
pub fn reopen_fds(records: &[FdRecord]) -> Result<(), String> {
    sys::reopen_fds(records)
}

/// Process signal dispositions plus the calling thread's mask and altstack.
#[derive(Clone)]
pub struct SignalState {
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    actions: Vec<(i32, libc::sigaction)>,
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    mask: libc::sigset_t,
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    altstack: libc::stack_t,
}

impl std::fmt::Debug for SignalState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SignalState { .. }")
    }
}

impl SignalState {
    pub fn take() -> Option<SignalState> {
        sys::signal_take()
    }

    /// Signals whose disposition differs between `self` and `other`.
    pub fn differing(&self, other: &SignalState) -> Vec<i32> {
        sys::signal_differing(self, other)
    }

    /// Re-install every disposition of `self` that differs from the current
    /// one, then the mask and (when it was set) the alternate stack.
    pub fn apply(&self) -> Result<usize, String> {
        sys::signal_apply(self)
    }
}

/// A resource limit at P1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitRecord {
    pub resource: i32,
    pub cur: u64,
    pub max: u64,
}

/// The limits the boot is known to change, now.
pub fn limits() -> Vec<LimitRecord> {
    sys::limits()
}

/// Apply each limit that differs from the current one (best effort; the
/// count applied).
pub fn apply_limits(limits: &[LimitRecord]) -> usize {
    sys::apply_limits(limits)
}

/// An environment variable the boot set (`Some`) or removed (`None`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvChange {
    pub name: OsString,
    pub value: Option<OsString>,
}

/// The process environment now, sorted by name.
pub fn env_now() -> Vec<(OsString, OsString)> {
    let mut v: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    v.sort();
    v
}

/// What changed between `p0` and now.
pub fn env_changes(p0: &[(OsString, OsString)]) -> Vec<EnvChange> {
    let now = env_now();
    let mut out = Vec::new();
    for (n, v) in &now {
        match p0.iter().find(|(pn, _)| pn == n) {
            Some((_, pv)) if pv == v => {}
            _ => out.push(EnvChange {
                name: n.clone(),
                value: Some(v.clone()),
            }),
        }
    }
    for (pn, _) in p0 {
        if !now.iter().any(|(n, _)| n == pn) {
            out.push(EnvChange {
                name: pn.clone(),
                value: None,
            });
        }
    }
    out
}

/// Re-apply `changes`.
///
/// # Safety
/// Modifies the process environment: no other thread may read or write it
/// concurrently (the restore path runs it while only the launcher thread,
/// blocked in `join`, and `main-vm` exist).
pub unsafe fn apply_env_changes(changes: &[EnvChange]) {
    for c in changes {
        match &c.value {
            // SAFETY: the caller guarantees exclusive access to the environment.
            Some(v) => unsafe { std::env::set_var(&c.name, v) },
            None => unsafe { std::env::remove_var(&c.name) },
        }
    }
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod sys {
    use super::{FdRecord, LimitRecord, SignalState, StackBounds};
    use libc::{c_int, c_void, dl_phdr_info, size_t, PT_TLS};
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" fn first_tls(info: *mut dl_phdr_info, _size: size_t, data: *mut c_void) -> c_int {
        // SAFETY: the loader passes a valid record; `data` is our out slot.
        let info = unsafe { &*info };
        let out = unsafe { &mut *(data as *mut Option<(usize, usize)>) };
        let phdrs = unsafe { std::slice::from_raw_parts(info.dlpi_phdr, info.dlpi_phnum as usize) };
        if let Some(p) = phdrs.iter().find(|p| p.p_type == PT_TLS) {
            let addr = info.dlpi_tls_data as usize;
            if addr != 0 {
                *out = Some((addr, p.p_memsz as usize));
            }
        }
        1 // the first object is the main executable
    }

    pub fn main_object_tls_block() -> Option<(usize, usize)> {
        let mut out: Option<(usize, usize)> = None;
        // SAFETY: the callback only writes `out`.
        unsafe { libc::dl_iterate_phdr(Some(first_tls), &mut out as *mut _ as *mut c_void) };
        out
    }

    pub fn stack_bounds() -> Option<StackBounds> {
        // SAFETY: plain pthread attribute queries on the calling thread.
        unsafe {
            let mut attr: libc::pthread_attr_t = std::mem::zeroed();
            if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
                return None;
            }
            let mut addr: *mut c_void = std::ptr::null_mut();
            let mut size: size_t = 0;
            let rc = libc::pthread_attr_getstack(&attr, &mut addr, &mut size);
            libc::pthread_attr_destroy(&mut attr);
            (rc == 0).then(|| StackBounds {
                lo: addr as usize,
                hi: addr as usize + size,
            })
        }
    }

    pub fn open_fds() -> Vec<i32> {
        let mut v: Vec<i32> = std::fs::read_dir("/proc/self/fd")
            .map(|d| {
                d.flatten()
                    .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse().ok()))
                    .collect()
            })
            .unwrap_or_default();
        // The directory stream's own descriptor is listed while it is open.
        v.retain(|&fd| std::fs::read_link(format!("/proc/self/fd/{fd}")).is_ok());
        v.sort_unstable();
        v
    }

    fn fdinfo(fd: i32) -> Option<(u64, i32)> {
        let s = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).ok()?;
        let mut pos = None;
        let mut flags = None;
        for line in s.lines() {
            if let Some(v) = line.strip_prefix("pos:") {
                pos = v.trim().parse().ok();
            } else if let Some(v) = line.strip_prefix("flags:") {
                flags = i32::from_str_radix(v.trim(), 8).ok();
            }
        }
        Some((pos?, flags?))
    }

    pub fn record_new_fds(p0: &[i32]) -> Result<Vec<FdRecord>, String> {
        let mut out = Vec::new();
        for fd in open_fds() {
            if p0.contains(&fd) {
                continue;
            }
            let target = std::fs::read_link(format!("/proc/self/fd/{fd}"))
                .map_err(|e| format!("fd {fd}: readlink: {e}"))?;
            let md = std::fs::metadata(&target)
                .map_err(|_| format!("fd {fd} -> {} is not a reopenable path", target.display()))?;
            if !md.is_file() {
                return Err(format!(
                    "fd {fd} -> {} is not a regular file",
                    target.display()
                ));
            }
            let (pos, flags) = fdinfo(fd).ok_or_else(|| format!("fd {fd}: no fdinfo"))?;
            out.push(FdRecord {
                fd,
                path: target,
                flags: flags & !(libc::O_CREAT | libc::O_EXCL | libc::O_TRUNC | libc::O_CLOEXEC),
                pos,
                cloexec: flags & libc::O_CLOEXEC != 0,
            });
        }
        Ok(out)
    }

    pub fn reopen_fds(records: &[FdRecord]) -> Result<(), String> {
        for r in records {
            let mut c = r.path.as_os_str().as_bytes().to_vec();
            c.push(0);
            // SAFETY: NUL-terminated path; plain syscalls on descriptors.
            unsafe {
                let flags = r.flags | if r.cloexec { libc::O_CLOEXEC } else { 0 };
                let fd = libc::open(c.as_ptr() as *const libc::c_char, flags);
                if fd < 0 {
                    return Err(format!(
                        "reopen fd {} ({}): {}",
                        r.fd,
                        r.path.display(),
                        std::io::Error::last_os_error()
                    ));
                }
                if fd != r.fd {
                    let rc = if r.cloexec {
                        libc::dup3(fd, r.fd, libc::O_CLOEXEC)
                    } else {
                        libc::dup2(fd, r.fd)
                    };
                    libc::close(fd);
                    if rc < 0 {
                        return Err(format!("dup fd {}: {}", r.fd, std::io::Error::last_os_error()));
                    }
                }
                if r.pos != 0 && libc::lseek(r.fd, r.pos as libc::off_t, libc::SEEK_SET) < 0 {
                    return Err(format!("seek fd {}: {}", r.fd, std::io::Error::last_os_error()));
                }
            }
        }
        Ok(())
    }

    /// Signals 1..=64 minus SIGKILL/SIGSTOP (which cannot be queried usefully).
    fn signals() -> impl Iterator<Item = i32> {
        (1..=64).filter(|&s| s != libc::SIGKILL && s != libc::SIGSTOP && !(32..=33).contains(&s))
    }

    pub fn signal_take() -> Option<SignalState> {
        // SAFETY: query-only sigaction / sigprocmask / sigaltstack calls.
        unsafe {
            let mut actions = Vec::new();
            for s in signals() {
                let mut a: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(s, std::ptr::null(), &mut a) == 0 {
                    actions.push((s, a));
                }
            }
            let mut mask: libc::sigset_t = std::mem::zeroed();
            libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut mask);
            let mut altstack: libc::stack_t = std::mem::zeroed();
            libc::sigaltstack(std::ptr::null(), &mut altstack);
            Some(SignalState {
                actions,
                mask,
                altstack,
            })
        }
    }

    fn same_action(a: &libc::sigaction, b: &libc::sigaction) -> bool {
        // SAFETY: sigset_t is plain bytes.
        let mask_eq = unsafe {
            libc::memcmp(
                &a.sa_mask as *const _ as *const c_void,
                &b.sa_mask as *const _ as *const c_void,
                std::mem::size_of::<libc::sigset_t>(),
            ) == 0
        };
        a.sa_sigaction == b.sa_sigaction && a.sa_flags == b.sa_flags && mask_eq
    }

    pub fn signal_differing(a: &SignalState, b: &SignalState) -> Vec<i32> {
        a.actions
            .iter()
            .filter(|(s, x)| {
                b.actions
                    .iter()
                    .find(|(t, _)| t == s)
                    .is_none_or(|(_, y)| !same_action(x, y))
            })
            .map(|(s, _)| *s)
            .collect()
    }

    pub fn signal_apply(st: &SignalState) -> Result<usize, String> {
        let now = signal_take().ok_or("signal state unreadable")?;
        let mut n = 0;
        // SAFETY: installing dispositions recorded from this same binary (the
        // handlers are at the same addresses: the key pins binary and layout).
        unsafe {
            for s in signal_differing(st, &now) {
                let (_, a) = st.actions.iter().find(|(t, _)| *t == s).expect("listed");
                if libc::sigaction(s, a, std::ptr::null_mut()) != 0 {
                    return Err(format!(
                        "sigaction({s}): {}",
                        std::io::Error::last_os_error()
                    ));
                }
                n += 1;
            }
            libc::pthread_sigmask(libc::SIG_SETMASK, &st.mask, std::ptr::null_mut());
            if st.altstack.ss_flags & libc::SS_DISABLE == 0 && !st.altstack.ss_sp.is_null() {
                let mut ss = st.altstack;
                ss.ss_flags = 0;
                if libc::sigaltstack(&ss, std::ptr::null_mut()) != 0 {
                    return Err(format!("sigaltstack: {}", std::io::Error::last_os_error()));
                }
            }
        }
        Ok(n)
    }

    const LIMITS: [libc::__rlimit_resource_t; 1] = [libc::RLIMIT_NOFILE];

    pub fn limits() -> Vec<LimitRecord> {
        LIMITS
            .iter()
            .filter_map(|&r| {
                let mut l: libc::rlimit = unsafe { std::mem::zeroed() };
                // SAFETY: query only.
                (unsafe { libc::getrlimit(r, &mut l) } == 0).then(|| LimitRecord {
                    resource: r as i32,
                    cur: l.rlim_cur,
                    max: l.rlim_max,
                })
            })
            .collect()
    }

    pub fn apply_limits(limits: &[LimitRecord]) -> usize {
        let now = self::limits();
        let mut n = 0;
        for l in limits {
            if now.contains(l) {
                continue;
            }
            let v = libc::rlimit {
                rlim_cur: l.cur,
                rlim_max: l.max,
            };
            // SAFETY: setrlimit on our own process.
            if unsafe { libc::setrlimit(l.resource as libc::__rlimit_resource_t, &v) } == 0 {
                n += 1;
            }
        }
        n
    }
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
mod sys {
    use super::{FdRecord, LimitRecord, SignalState, StackBounds};
    pub fn main_object_tls_block() -> Option<(usize, usize)> {
        None
    }
    pub fn stack_bounds() -> Option<StackBounds> {
        None
    }
    pub fn open_fds() -> Vec<i32> {
        Vec::new()
    }
    pub fn record_new_fds(_p0: &[i32]) -> Result<Vec<FdRecord>, String> {
        Err("unsupported platform".into())
    }
    pub fn reopen_fds(_r: &[FdRecord]) -> Result<(), String> {
        Err("unsupported platform".into())
    }
    pub fn signal_take() -> Option<SignalState> {
        None
    }
    pub fn signal_differing(_a: &SignalState, _b: &SignalState) -> Vec<i32> {
        Vec::new()
    }
    pub fn signal_apply(_s: &SignalState) -> Result<usize, String> {
        Err("unsupported platform".into())
    }
    pub fn limits() -> Vec<LimitRecord> {
        Vec::new()
    }
    pub fn apply_limits(_l: &[LimitRecord]) -> usize {
        0
    }
}
