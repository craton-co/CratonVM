// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Phase 5 fast boot, WP5 + WP6: the launcher's `restore_or_boot`
//! (`docs/jdk-only/boot-image-snapshot-20261028.md`).
//!
//! `CRATONVM_BOOT_IMAGE` (a scalar; default `off`):
//!
//! * `off` -- every run cold boots; nothing here does anything.
//! * `capture` -- cold boot, and write an image at the snapshot point P1
//!   (replacing any image with the same key). The run then goes on to `main`
//!   exactly as a cold run does.
//! * `auto` -- restore from the image whose key matches this run, else cold
//!   boot and capture (so the next identical run restores).
//!
//! A `,verbose` token (`auto,verbose`) prints one `[cratonvm] boot image:` line
//! per decision on stderr (the default is silent: a run's output must not
//! depend on whether it restored). `CRATONVM_BOOT_IMAGE_DIR` overrides the
//! cache directory (`boot_image::cache_dir`).
//!
//! # The two points
//!
//! * **P0** -- in `run()` right before `Vm::try_new`. Everything before it
//!   (argv, configuration) runs in every mode. A capture run snapshots the
//!   binary's writable segment, `main-vm`'s TLS block, its fds, signal state,
//!   limits and environment, then arms the capture arena. A restoring run maps
//!   the image instead and rebinds the process state ([`Restored`]).
//! * **P1** -- after `boot_publish_jdk_booted` (stage b): the JDK is booted,
//!   the main class is not yet resolved. A capture run writes the image here
//!   ([`Capture::at_p1`]); a restored run arrives here without having booted
//!   ([`Restored::at_p1`]). Both then start the deferred threads (WP3).
//!
//! # What makes a run eligible
//!
//! Linux x86_64 (the launcher is `-no-pie` there), a real-JDK boot, and none
//! of the launcher features that run code or start threads between P0 and P1
//! (agents, watchdogs, samplers, `-version`, `-XX:+PrintFlagsFinal`, the
//! jdk-only trace sinks): the caller decides and passes the reason.
//!
//! # Address-space layout
//!
//! The image holds pointers into the C libraries (the binary's GOT, function
//! pointers in `.data`). The key pins the shared-library layout, so under ASLR
//! no image would ever match: [`reexec_without_aslr`], first thing in `main`,
//! re-executes the launcher once with `ADDR_NO_RANDOMIZE` when the knob is on,
//! then clears the flag again so the VM's own child processes get ASLR back.

use cratonvm_types::boot_image::process_state::{
    self, EnvChange, FdRecord, LimitRecord, SignalState, StackBounds, ThreadTls, TlsRun,
};
use cratonvm_types::boot_image::statics::{self, ClassList, Resolution, StaticClass, SymTab};
use cratonvm_types::boot_image::{self, BootKey, KeyInputs, Refusal, RwSnapshot, SegmentSpec};
use cratonvm_types::image_layout::{self, Backing, ImageMode};
use cratonvm_vm::runtime::launcher_signals::LauncherSignals;
use cratonvm_vm::vm::SharedVm;
use cratonvm_vm::{JvmThread, Vm};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The knob's name (a `flag_groups::SCALARS` entry).
pub const ENV_KNOB: &str = "CRATONVM_BOOT_IMAGE";
/// Cache directory override (a `flag_groups::SCALARS` entry).
pub const ENV_DIR: &str = "CRATONVM_BOOT_IMAGE_DIR";

/// WP7's checked classification of every static/thread-local a capture's boot
/// changes (see `cratonvm_types::boot_image::statics`). Lives in the launcher
/// so adding an owner relinks only this crate.
pub const STATIC_CLASSES: &str = include_str!("boot_image_statics.txt");

/// Restore/capture is implemented for the `-no-pie` Linux x86_64 launcher.
pub const SUPPORTED: bool = cfg!(all(target_os = "linux", target_arch = "x86_64"));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Knob {
    Off,
    Capture,
    Auto,
}

/// The knob and the verbose bit. `Off` on an unsupported target, whatever the
/// variable says.
pub fn knob() -> (Knob, bool) {
    let Ok(v) = std::env::var(ENV_KNOB) else {
        return (Knob::Off, false);
    };
    let mut k = Knob::Off;
    let mut verbose = false;
    for t in v.split(',').map(str::trim) {
        match t {
            "capture" => k = Knob::Capture,
            "auto" | "on" => k = Knob::Auto,
            "verbose" => verbose = true,
            _ => {}
        }
    }
    if !SUPPORTED {
        k = Knob::Off;
    }
    (k, verbose)
}

fn note(verbose: bool, msg: &str) {
    tracing::info!("boot image: {msg}");
    if verbose {
        eprintln!("[cratonvm] boot image: {msg}");
    }
}

/// First thing in `main`: with the knob on and ASLR active, re-execute this
/// binary with `ADDR_NO_RANDOMIZE` (once: the flag survives `execve`). Either
/// way, the flag is then cleared for this process's future children (the
/// current layout was fixed at `execve`). Silent and best effort: on failure
/// the run goes on with ASLR, and no image will match it.
pub fn reexec_without_aslr() {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        if knob().0 == Knob::Off {
            return;
        }
        const ADDR_NO_RANDOMIZE: libc::c_ulong = 0x0040000;
        // SAFETY: personality(0xffffffff) only queries.
        let cur = unsafe { libc::personality(0xffff_ffff) };
        if cur < 0 {
            return;
        }
        let cur = cur as libc::c_ulong;
        if cur & ADDR_NO_RANDOMIZE == 0 {
            // SAFETY: setting a personality bit for the coming execve.
            if unsafe { libc::personality(cur | ADDR_NO_RANDOMIZE) } < 0 {
                return;
            }
            use std::os::unix::ffi::OsStrExt;
            let args: Vec<std::ffi::CString> = std::env::args_os()
                .filter_map(|a| std::ffi::CString::new(a.as_bytes()).ok())
                .collect();
            let mut argv: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
            argv.push(std::ptr::null());
            // The binary's real path, not `/proc/self/exe`: the kernel names
            // the process (`comm`) after the executed file's base name.
            let exe = std::env::current_exe()
                .ok()
                .and_then(|p| std::ffi::CString::new(p.as_os_str().as_bytes()).ok())
                .unwrap_or_else(|| c"/proc/self/exe".to_owned());
            // SAFETY: NUL-terminated argv of valid C strings; on success this
            // does not return.
            unsafe { libc::execv(exe.as_ptr(), argv.as_ptr()) };
            // execv failed: put the personality back and run as we are.
            // SAFETY: as above.
            unsafe { libc::personality(cur) };
            return;
        }
        // SAFETY: clearing the bit only affects later execve calls.
        unsafe { libc::personality(cur & !ADDR_NO_RANDOMIZE) };
    }
}

/// What the launcher passes at P0.
pub struct P0Inputs<'a> {
    pub java_home: Option<&'a Path>,
    pub classpath: &'a [String],
    /// The whole expanded launcher argv. Part of the key: the main class, its
    /// arguments and `sun.java.command` are baked into the booted JDK (system
    /// properties, `VM.savedProps`), so an image is per command line.
    pub argv: &'a [String],
    /// `Err(reason)` when the run uses a launcher feature the image cannot
    /// carry.
    pub eligible: Result<(), &'static str>,
}

/// The decision at P0.
pub enum AtP0 {
    /// No image involved: the cold path, unchanged.
    Cold,
    /// Capturing: snapshot taken, arena armed.
    Capture(Box<Capture>),
    /// Restored: the image is mapped and the process state rebound.
    Restored(Restored),
}

impl AtP0 {
    pub fn is_restored(&self) -> bool {
        matches!(self, AtP0::Restored(_))
    }
    pub fn is_capture(&self) -> bool {
        matches!(self, AtP0::Capture(_))
    }
}

/// The image root: what the capture run leaves in its arena for the restored
/// run. Every pointer and `Vec` here is arena memory, valid after restore.
#[repr(C)]
struct ImageRoot {
    magic: u64,
    /// `Arc::into_raw` of one extra strong handle: the restored run's `vm.shared`.
    shared: *const SharedVm,
    /// The capture's `vm.main_thread` box; the restored run takes ownership.
    main_thread: *mut JvmThread,
    /// `Arc::into_raw` of one extra handle on the launcher-signals slot the
    /// deferred `LauncherSignals` start fills.
    launcher_signals: *const parking_lot::Mutex<Option<Option<LauncherSignals>>>,
    /// Length of the arena segment (for `adopt_restored`).
    arena_len: usize,
    /// WP6 rebinds, recorded at P1.
    tls_addr: usize,
    tls_runs: Vec<TlsRun>,
    fds: Vec<FdRecord>,
    signals: Option<SignalState>,
    limits: Vec<LimitRecord>,
    env: Vec<EnvChange>,
}

const ROOT_MAGIC: u64 = u64::from_le_bytes(*b"CVMROOT1");

fn image_dir() -> Option<PathBuf> {
    let over = std::env::var_os(ENV_DIR).filter(|v| !v.is_empty()).map(PathBuf::from);
    boot_image::cache_dir(over.as_deref())
}

/// The key of this run. Beyond `BootKey::gather`: `main-vm`'s stack and TLS
/// block (a restored `main-vm` is a fresh thread; its TLS must be where the
/// capture's was for the copied thread-locals to be right).
fn key_for(inp: &P0Inputs<'_>) -> BootKey {
    let cp: Vec<PathBuf> = inp.classpath.iter().map(PathBuf::from).collect();
    let opts = inp.argv.join("\u{0}");
    let mut key = BootKey::gather(&KeyInputs {
        java_home: inp.java_home,
        classpath: &cp,
        vm_options: &opts,
    });
    let st = StackBounds::current().unwrap_or_default();
    let mut v = (st.lo as u64).to_le_bytes().to_vec();
    v.extend_from_slice(&(st.hi as u64).to_le_bytes());
    key.push("main_vm_stack", v);
    let (ta, tl) = process_state::main_object_tls_block().unwrap_or((0, 0));
    let mut v = (ta as u64).to_le_bytes().to_vec();
    v.extend_from_slice(&(tl as u64).to_le_bytes());
    key.push("main_vm_tls", v);
    key
}

/// P0. Decides cold / capture / restore; on restore, maps the image and
/// rebinds the process state before returning.
pub fn at_p0(inp: &P0Inputs<'_>) -> AtP0 {
    let (k, verbose) = knob();
    if k == Knob::Off {
        return AtP0::Cold;
    }
    if let Err(why) = inp.eligible {
        note(verbose, &format!("not used: {why}"));
        return AtP0::Cold;
    }
    let Some(dir) = image_dir() else {
        note(verbose, "not used: no cache directory");
        return AtP0::Cold;
    };
    let key = key_for(inp);
    let path = boot_image::image_path(&dir, &key);
    if k == Knob::Auto {
        match boot_image::try_map(&path, &key) {
            Ok(m) => match Restored::adopt(m.root, verbose) {
                Ok(r) => {
                    note(
                        verbose,
                        &format!(
                            "restored {} ({} segments, {} KiB mapped, {} diff bytes)",
                            path.display(),
                            m.segments,
                            m.mapped_bytes >> 10,
                            m.diff_bytes
                        ),
                    );
                    return AtP0::Restored(r);
                }
                Err(e) => {
                    // Past the point of no return: the data diff is applied.
                    eprintln!("[cratonvm] boot image: restore failed after mapping: {e}");
                    std::process::exit(1);
                }
            },
            Err(Refusal::NotFound) => note(verbose, "miss: no image; capturing"),
            Err(r) => note(verbose, &format!("refused ({r}); cold boot and capture")),
        }
    }
    match Capture::begin(key, path, verbose) {
        Ok(c) => AtP0::Capture(c),
        Err(e) => {
            note(verbose, &format!("capture not started: {e}"));
            AtP0::Cold
        }
    }
}

/// A capture in progress (P0..P1). Built before the arena is armed, so its
/// own buffers are ordinary heap, never imaged.
pub struct Capture {
    key: BootKey,
    path: PathBuf,
    verbose: bool,
    rw: Option<RwSnapshot>,
    tls: Option<ThreadTls>,
    fds0: Vec<i32>,
    signals0: Option<SignalState>,
    limits0: Vec<LimitRecord>,
    env0: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    /// `/proc/self/maps` at P0, for the dangling-pointer gate at P1.
    maps0: String,
}

impl Capture {
    fn begin(key: BootKey, path: PathBuf, verbose: bool) -> Result<Box<Capture>, String> {
        if image_layout::mode() != ImageMode::Off {
            return Err("image mode already set".into());
        }
        image_layout::set_mode(ImageMode::Capture);
        let tls = ThreadTls::take();
        let fds0 = process_state::open_fds();
        let signals0 = SignalState::take();
        let limits0 = process_state::limits();
        let env0 = process_state::env_now();
        let maps0 = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
        let mut cap = Box::new(Capture {
            key,
            path,
            verbose,
            rw: None,
            tls,
            fds0,
            signals0,
            limits0,
            env0,
            maps0,
        });
        // The writable-segment snapshot last: anything allocated after it (in
        // the allocator whose statics live there) would show in the diff.
        cap.rw = Some(RwSnapshot::take().map_err(|e| e.to_string())?);
        // Last: everything above is ordinary heap.
        if let Err(e) = crate::image_arena::arm() {
            // Capture mode with no arena would spoil every mapping; the mode
            // cannot go back to Off, so move on to Restored (later mappings
            // anywhere, deferred starts run at once).
            image_layout::spoil("capture arena could not be armed");
            image_layout::set_mode(ImageMode::Restored);
            return Err(e.to_string());
        }
        Ok(cap)
    }

    /// P1 of a capture run: write the image, then leave capture mode and start
    /// the deferred threads, so the run goes on exactly as a cold one.
    pub fn at_p1(self: Box<Self>, vm: &Vm, launcher_signals: &DeferredLauncherSignals) {
        let verbose = self.verbose;
        let result = self.write(vm, launcher_signals);
        // An armed arena would keep serving the rest of the run; a failed
        // write may have left it armed.
        let _ = crate::image_arena::disarm();
        image_layout::set_mode(ImageMode::Restored);
        match result {
            Ok(msg) => note(verbose, &msg),
            Err(e) => note(verbose, &format!("capture refused: {e}")),
        }
        cratonvm_vm::runtime::deferred_starts::run_deferred_starts(&vm.shared);
    }

    fn write(self: Box<Self>, vm: &Vm, launcher_signals: &DeferredLauncherSignals) -> Result<String, String> {
        let me = cratonvm_types::image_layout::current_os_tid().unwrap_or(0);
        match image_layout::threads_blocking_capture(&[me]) {
            Some(t) if t.is_empty() => {}
            Some(t) => {
                let names: Vec<String> = t.iter().map(|t| format!("{}({})", t.name, t.tid)).collect();
                return Err(format!("threads alive at P1: {}", names.join(", ")));
            }
            None => return Err("no thread census".into()),
        }
        if let Some(why) = image_layout::spoiled() {
            return Err(format!("spoiled: {why}"));
        }
        match crate::image_arena::census() {
            Some(c) if c.heap.inner_allocs_while_armed == 0 => {}
            Some(c) => {
                return Err(format!(
                    "{} allocations missed the arena",
                    c.heap.inner_allocs_while_armed
                ))
            }
            None => return Err("arena not armed".into()),
        }

        // WP7: classify every changed static and thread-local. Everything
        // below that allocates and must survive into the image (the root and
        // its vectors, the data diff) is allocated while the arena is still
        // armed; transient work frees back into it.
        let arena_statics: Vec<Range<usize>> = crate::image_arena::own_statics().to_vec();
        let rw = self.rw.as_ref().ok_or("no writable-segment snapshot")?;
        let data_changed: Vec<Range<u64>> = rw
            .changed()
            .into_iter()
            .filter(|r| !arena_statics.iter().any(|a| a.start <= r.start && r.end <= a.end))
            .map(|r| r.start as u64..r.end as u64)
            .collect();
        let tls_changed: Vec<Range<u64>> = self
            .tls
            .as_ref()
            .map(|t| t.changed_ranges().into_iter().map(|r| r.start as u64..r.end as u64).collect())
            .unwrap_or_default();
        let (data_res, tls_res) = classify(&data_changed, &tls_changed)?;
        let unclassified: Vec<String> = data_res
            .iter()
            .map(|r| (r, ""))
            .chain(tls_res.iter().map(|r| (r, "tls:")))
            .filter(|(r, _)| r.class != Some(StaticClass::Keep) && r.class != Some(StaticClass::Exclude))
            .map(|(r, t)| {
                format!(
                    "{t}{} ({} bytes, {})",
                    r.owner,
                    r.bytes,
                    if r.class.is_none() { "unclassified" } else { "refuse" }
                )
            })
            .collect();
        if verbose_census() {
            dump_resolutions(&data_res, &tls_res);
        }
        if !unclassified.is_empty() {
            return Err(format!(
                "{} static(s) not restore-safe: {}",
                unclassified.len(),
                unclassified.join("; ")
            ));
        }
        let mut excludes = arena_statics;
        excludes.extend(
            data_res
                .iter()
                .filter(|r| r.class == Some(StaticClass::Exclude))
                .map(|r| r.range.start as usize..r.range.end as usize),
        );
        let tls_excl: Vec<Range<usize>> = tls_res
            .iter()
            .filter(|r| r.class == Some(StaticClass::Exclude))
            .map(|r| r.range.start as usize..r.range.end as usize)
            .collect();

        let fds = process_state::record_new_fds(&self.fds0)?;
        let signals = SignalState::take();
        if let (Some(a), Some(b)) = (&signals, &self.signals0) {
            let d = a.differing(b);
            if !d.is_empty() {
                tracing::debug!(?d, "boot image: signal dispositions changed P0..P1");
            }
        }
        let limits = process_state::limits();
        let limits: Vec<LimitRecord> = limits.into_iter().filter(|l| !self.limits0.contains(l)).collect();
        let env = process_state::env_changes(&self.env0);
        let (tls_addr, tls_runs) = match &self.tls {
            Some(t) => (t.addr, t.diff(&tls_excl)),
            None => (0, Vec::new()),
        };
        let root = Box::into_raw(Box::new(ImageRoot {
            magic: ROOT_MAGIC,
            shared: Arc::into_raw(Arc::clone(&vm.shared)),
            main_thread: &*vm.main_thread as *const JvmThread as *mut JvmThread,
            launcher_signals: Arc::into_raw(Arc::clone(launcher_signals)),
            arena_len: 0,
            tls_addr,
            tls_runs,
            fds,
            signals,
            limits,
            env,
        }));
        let diff = rw.diff(&excludes);

        // The arena stops changing here.
        crate::image_arena::disarm().map_err(|e| e.to_string())?;
        let (arena_base, arena_len) = crate::image_arena::captured_extent().ok_or("no arena extent")?;
        // SAFETY: the root is a live arena block nothing else references yet.
        unsafe { (*root).arena_len = arena_len };
        let mut segments = boot_image::segments_at_p1().map_err(|e| e.to_string())?;
        segments.push(SegmentSpec {
            kind: boot_image::kind::ARENA,
            addr: arena_base,
            len: arena_len,
            prot: boot_image::prot::READ | boot_image::prot::WRITE,
            backing: Backing::Anonymous { hugetlb: false },
        });
        segments.sort_by_key(|s| s.addr);
        dangling_gate(&self.maps0, &segments, self.verbose)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let spec = boot_image::ImageSpec {
            key: &self.key,
            root: root as usize,
            segments: &segments,
            diff: &diff,
        };
        // SAFETY: P1 -- no other thread exists (checked above), the arena is
        // disarmed, every recorded mapping is live.
        let rep = unsafe { boot_image::write_image(&self.path, &spec) }.map_err(|e| e.to_string())?;
        Ok(format!(
            "captured {} ({} KiB file, {} segments, {} diff runs / {} bytes, {} statics classified, {} tls runs)",
            rep.path.display(),
            rep.file_len >> 10,
            segments.len(),
            rep.diff_runs,
            rep.diff_bytes,
            data_res.len() + tls_res.len(),
            // SAFETY: the root is live.
            unsafe { (*root).tls_runs.len() }
        ))
    }
}

/// One `/proc/self/maps` line: `[start, end)`, perms, label.
struct Vma {
    start: usize,
    end: usize,
    perms: String,
    label: String,
}

fn parse_maps(text: &str) -> Vec<Vma> {
    text.lines()
        .filter_map(|l| {
            let mut c = l.split_whitespace();
            let (a, b) = c.next()?.split_once('-')?;
            let perms = c.next()?.to_string();
            let label = c.nth(3).unwrap_or("").to_string();
            Some(Vma {
                start: usize::from_str_radix(a, 16).ok()?,
                end: usize::from_str_radix(b, 16).ok()?,
                perms,
                label,
            })
        })
        .collect()
}

/// Page-rounded `[lo, hi)` of the binary's `.data`..`.bss` (`_end`): fixed
/// addresses (`-no-pie`), carried by the data diff.
fn binary_rw_span() -> (usize, usize) {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        extern "C" {
            static __data_start: u8;
            static _end: u8;
        }
        // SAFETY: linker-defined symbols; only their addresses are taken.
        let (s, e) = unsafe {
            (std::ptr::addr_of!(__data_start) as usize, std::ptr::addr_of!(_end) as usize)
        };
        (s & !0xFFF, (e + 0xFFF) & !0xFFF)
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        (0, 0)
    }
}

/// WP7's pointer gate: every word of imaged anonymous memory that points into
/// a writable mapping the image does NOT carry would dangle in the restored
/// process (a C-library heap block, a mapping made outside the broker, a
/// pre-P0 allocator block). Refuses the capture on one into a mapping made
/// P0..P1, or into the allocator/brk heaps; the calling thread's stack and
/// the process stack are not targets (stack bounds are legitimately recorded
/// values, and the key pins `main-vm`'s stack address).
fn dangling_gate(maps0: &str, segments: &[SegmentSpec], verbose: bool) -> Result<(), String> {
    let now = std::fs::read_to_string("/proc/self/maps").map_err(|e| e.to_string())?;
    let before = parse_maps(maps0);
    let stack = StackBounds::current().unwrap_or_default();
    // The binary's own writable segment is at a fixed address and diffed.
    let (bin_lo, bin_hi) = binary_rw_span();
    let mut targets: Vec<(usize, usize, String, bool)> = Vec::new();
    for v in parse_maps(&now) {
        // The capture arena's reserve beyond its used part holds no live
        // block (the bump allocator never handed it out, or it was freed),
        // so only garbage can point there.
        let arena = crate::image_arena::ARENA_BASE;
        if v.perms.as_bytes().get(1) != Some(&b'w')
            || (v.start >= arena && v.end <= arena + crate::image_arena::ARENA_RESERVE)
            || (v.start >= stack.lo && v.end <= stack.hi)
            || (v.start >= bin_lo && v.end <= bin_hi)
            || matches!(v.label.as_str(), "[stack]" | "[vvar]" | "[vdso]")
        {
            continue;
        }
        let new = !before.iter().any(|b| b.start == v.start && b.end == v.end);
        let name = format!("{:x}-{:x} {} {}", v.start, v.end, v.perms, v.label);
        // What the image does not carry of this mapping: the VMA minus the
        // recorded segments (an arena or broker reservation is imaged only
        // up to its used part).
        let mut pieces = vec![(v.start, v.end)];
        for s in segments {
            let (a, b) = (s.addr, s.addr + s.len);
            pieces = pieces
                .into_iter()
                .flat_map(|(x, y)| {
                    if b <= x || y <= a {
                        vec![(x, y)]
                    } else {
                        [(x, a.max(x)), (b.min(y), y)].into_iter().filter(|p| p.0 < p.1).collect()
                    }
                })
                .collect();
        }
        for (x, y) in pieces {
            targets.push((x, y, name.clone(), new));
        }
    }
    targets.sort_by_key(|t| t.0);
    let mut hits: Vec<(usize, usize)> = vec![(0, 0); targets.len()]; // (count, first source addr)
    for s in segments {
        if !matches!(s.backing, Backing::Anonymous { .. }) || s.prot & boot_image::prot::READ == 0 {
            continue;
        }
        // SAFETY: a live, readable, recorded mapping (checked at P1 by
        // `segments_at_p1`); no other thread runs.
        let words = unsafe { std::slice::from_raw_parts(s.addr as *const usize, s.len / 8) };
        for (i, &w) in words.iter().enumerate() {
            if w < 0x10000 || w > 0x7fff_ffff_ffff {
                continue;
            }
            let k = targets.partition_point(|t| t.0 <= w);
            if k > 0 && w < targets[k - 1].1 {
                let h = &mut hits[k - 1];
                if h.0 == 0 {
                    h.1 = s.addr + i * 8;
                }
                h.0 += 1;
                if h.0 <= 6 && verbose_census() {
                    // SAFETY: `w` lies in a live readable-or-not mapping;
                    // only print when it is readable.
                    let t = &targets[k - 1];
                    let readable = t.2.split_whitespace().nth(1).is_some_and(|p| p.starts_with('r'));
                    let peek = if readable && w + 32 <= t.1 {
                        let b = unsafe { std::slice::from_raw_parts(w as *const u8, 32) };
                        let hex: String = b[..16].iter().map(|x| format!("{x:02x}")).collect();
                        let asc: String =
                            b.iter().map(|&x| if (32..127).contains(&x) { x as char } else { '.' }).collect();
                        format!("{hex} |{asc}|")
                    } else {
                        String::new()
                    };
                    let ctx: String = (1..=3)
                        .filter_map(|d| words.get(i.wrapping_sub(d)).map(|v| format!("{v:#x}")))
                        .collect::<Vec<_>>()
                        .join(",");
                    eprintln!(
                        "[cratonvm] boot image census ptr-sample {} src={:#x} val={w:#x} prev=[{ctx}] {peek}",
                        t.2,
                        s.addr + i * 8
                    );
                }
            }
        }
    }
    let mut bad = Vec::new();
    for (t, h) in targets.iter().zip(&hits) {
        if h.0 == 0 {
            continue;
        }
        let refuse = t.3 || t.2.contains("[heap]") || t.2.contains("mimalloc");
        let line = format!(
            "{} word(s) into {} ({}), first at {:#x}",
            h.0,
            t.2,
            if t.3 { "made P0..P1" } else { "pre-P0" },
            h.1
        );
        if verbose_census() {
            eprintln!("[cratonvm] boot image census pointer {line}");
        }
        if refuse {
            bad.push(line);
        }
    }
    if !bad.is_empty() && unsafe_ignore_pointers() {
        // Diagnosis only: lets the restore path be exercised and measured
        // while the gate still finds hazards. Never a supported mode.
        note(verbose, &format!("UNSAFE: ignoring {} dangling-pointer target(s)", bad.len()));
        return Ok(());
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(format!("imaged memory points into non-imaged mappings: {}", bad.join("; ")))
    }
}

/// The launcher's signal-handler slot (the `LauncherSignals` deferred start).
pub type DeferredLauncherSignals = cratonvm_vm::runtime::deferred_starts::DeferredSlot<Option<LauncherSignals>>;

/// `unsafe-ignore-pointers` token: write the image although the dangling-pointer
/// gate refuses (diagnosis and measurement of the restore path only).
fn unsafe_ignore_pointers() -> bool {
    std::env::var(ENV_KNOB).is_ok_and(|v| v.split(',').any(|t| t.trim() == "unsafe-ignore-pointers"))
}

fn verbose_census() -> bool {
    std::env::var(ENV_KNOB).is_ok_and(|v| v.split(',').any(|t| t.trim() == "census"))
}

fn dump_resolutions(data: &[Resolution], tls: &[Resolution]) {
    for (tag, list) in [("data", data), ("tls", tls)] {
        for r in list {
            let class = match r.class {
                Some(StaticClass::Keep) => "keep",
                Some(StaticClass::Exclude) => "exclude",
                Some(StaticClass::Refuse) => "refuse",
                None => "UNCLASSIFIED",
            };
            eprintln!("[cratonvm] boot image census {tag} {class} {} {} {}", r.bytes, r.range.end - r.range.start, r.owner);
        }
    }
}

/// Resolve the changed bytes to owners and classify them with the builtin
/// list (symbols read from this binary's `.symtab`).
fn classify(
    data: &[Range<u64>],
    tls: &[Range<u64>],
) -> Result<(Vec<Resolution>, Vec<Resolution>), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    // Keep only symbols that can own a changed byte (the table has ~10^5
    // entries; this runs with the arena armed). A symbol is kept when it
    // overlaps, or starts before and may cover, a changed range.
    let hit = |a: u64, s: u64, list: &[Range<u64>]| {
        let end = a + s.max(1);
        list.iter().any(|r| r.start < end && a < r.end)
    };
    let tab = SymTab::read_filtered(&exe, &|a, s, t| hit(a, s, if t { tls } else { data }))
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    let list = ClassList::parse(STATIC_CLASSES).map_err(|e| format!("boot_image_statics.txt: {e}"))?;
    Ok((
        statics::resolve(data, &tab, &list, false),
        statics::resolve(tls, &tab, &list, true),
    ))
}

/// A restored run, between P0 and P1.
pub struct Restored {
    root: *mut ImageRoot,
    verbose: bool,
}

impl Restored {
    /// Right after `try_map`: leave Off, adopt the arena, and rebind the
    /// process state the image could not carry. Every rebind is here or in
    /// [`Restored::at_p1`], each naming the state it restores.
    fn adopt(root: usize, verbose: bool) -> Result<Restored, String> {
        let root = root as *mut ImageRoot;
        image_layout::set_mode(ImageMode::Restored);
        // SAFETY: `try_map` mapped the image; the root is the capture's.
        let r = unsafe { &*root };
        if r.magic != ROOT_MAGIC {
            return Err("image root has a bad magic".into());
        }
        // (1) The capture arena: frees of imaged blocks become no-ops before
        // anything can free one (mimalloc never saw them).
        crate::image_arena::adopt_restored(crate::image_arena::ARENA_BASE, r.arena_len)
            .map_err(|e| e.to_string())?;
        // (2) main-vm's thread-locals the capture boot set (this thread is a
        // fresh one; the key pinned its TLS block address).
        // SAFETY: we are main-vm; the runs are the capture's values for this
        // binary, minus the excluded owners.
        if !unsafe { process_state::apply_tls_runs(r.tls_addr, &r.tls_runs) } {
            return Err("main-vm TLS block moved".into());
        }
        // (3) Descriptors the boot opened and kept (lib/modules, jars, ...).
        process_state::reopen_fds(&r.fds)?;
        // (4) Signal dispositions installed during boot, main-vm's mask and
        // alternate stack.
        if let Some(s) = &r.signals {
            s.apply()?;
        }
        // (5) Resource limits the boot raised (jdk-only RLIMIT_NOFILE).
        process_state::apply_limits(&r.limits);
        // (6) Environment variables the boot set or removed.
        // SAFETY: only this thread and the launcher thread (blocked in join)
        // exist.
        unsafe { process_state::apply_env_changes(&r.env) };
        Ok(Restored { root, verbose })
    }

    /// The VM the capture booted, and the launcher-signals slot.
    pub fn take_vm(&mut self) -> (Vm, DeferredLauncherSignals) {
        // SAFETY: the root's pointers are the extra handles the capture made
        // for exactly this; taken once (root fields nulled).
        unsafe {
            let r = &mut *self.root;
            assert!(!r.shared.is_null(), "boot image root taken twice");
            let shared = Arc::from_raw(r.shared);
            let main_thread = Box::from_raw(r.main_thread);
            let signals = Arc::from_raw(r.launcher_signals);
            r.shared = std::ptr::null();
            (Vm { shared, main_thread }, signals)
        }
    }

    /// P1 of a restored run: the per-launch rebinds that need the VM, then
    /// the deferred thread starts.
    pub fn at_p1(self, vm: &Vm) {
        // (7) The VM's start instant (uptime, management `getStartTime`,
        // perf-data origin): this process's start, not the capture's.
        vm.shared.debug.diagnostic_counters.rebase_start_instant(std::time::Instant::now());
        let n = cratonvm_vm::runtime::deferred_starts::run_deferred_starts(&vm.shared);
        note(self.verbose, &format!("restored run at P1; {n} deferred thread starts"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_static_class_list_parses_and_excludes_the_allocator() {
        let l = ClassList::parse(STATIC_CLASSES).expect("boot_image_statics.txt parses");
        assert_eq!(l.classify("heap_main"), Some(StaticClass::Exclude));
        assert_eq!(l.classify("_mi_heap_default"), Some(StaticClass::Exclude));
        assert_eq!(l.classify("a::b::X::__RUST_STD_INTERNAL_VAL"), Some(StaticClass::Keep));
        assert_eq!(l.classify("some::new::STATIC"), None);
    }
}
