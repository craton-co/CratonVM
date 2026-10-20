// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! zr z7/rs: lazy commit of mimalloc's arenas (`CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT`).
//!
//! **Opt-in since zr z10/mt** (`=1`, or `force`; unset and `0` keep mimalloc's
//! eager default). The z10 host rows, 5 interleaved rounds, CPU seconds
//! median: class loading 0.95 lazy vs 0.79 eager (+20 %), boot 0.53 vs 0.51;
//! `MIMALLOC_ALLOW_THP=0` on the eager arm cost the same, so the CPU is the
//! huge pages' (about 35k extra 4 KiB first-touch faults), and so is the
//! ~90 MB RSS saving. A default flip must cost at most 1.05x CPU. The RSS side
//! is now taken by `sparse_block_alloc` for the big zeroed blocks, without
//! giving up the arena's huge pages.
//!
//! # Why
//!
//! mimalloc v3's default `arena_eager_commit=2` commits every arena eagerly on
//! an overcommit OS (Linux). For the first arena that is a 1 GiB reservation
//! (`arena_reserve`) mapped read-write in one call. Because the reservation is
//! committed, `allow_large` stays true down to `unix_mmap`, which then
//! `madvise(MADV_HUGEPAGE)`s the whole GiB (`c_src/mimalloc/v3/src/prim/unix/
//! prim.c:413-421`, reached through `arena.c:355` `mi_arena_reserve` →
//! `os.c:299/342`: only `commit=false` clears `allow_large`). So on a host whose
//! THP mode is `madvise` -- the common distro default, where nothing else in the
//! process gets huge pages -- every 2 MiB run of the allocator's arena that the
//! VM touches faults a whole huge page in.
//!
//! Measured on the Linux host (zr z7b, `Zr7RsBoot hello`, THP `madvise`): the
//! `[anon:mimalloc]` arena held 141 MB of RSS, all of it `AnonHugePages`, out of
//! a 208.8 MB Hello world (HotSpot: 43.7 MB). `MIMALLOC_ARENA_EAGER_COMMIT=0`
//! took `/usr/bin/time %M` from 233 MB to 155 MB. The GC's own heap is not
//! involved: `reservation.rs::platform_reserve` advises only its own mapping,
//! and only under `CRATONVM_HEAP_THP`.
//!
//! # How
//!
//! An `.init_array.00100` entry runs before mimalloc's own process constructor
//! and after std's argv hook (`.init_array.00099`), i.e. before any Rust
//! allocation and before
//! mimalloc reads its options from the environment. It calls
//! `mi_option_set_default(arena_eager_commit, 0)`: a DEFAULT, so an explicit
//! `MIMALLOC_ARENA_EAGER_COMMIT` (or the legacy `MIMALLOC_EAGER_REGION_COMMIT`)
//! still wins when mimalloc initialises the option.
//!
//! (zs s1/fp: mimalloc's constructor is `constructor(101)` only when the C
//! side is built by clang; under gcc -- the `cc` crate's default on Linux --
//! it is a plain `constructor` (`c_src/mimalloc/v3/src/prim/prim.c:33-38`),
//! which runs after every prioritised `.init_array` entry. Either way it runs
//! after this hook; only the "101" was clang-specific.)
//!
//! zr z9/rv8: the option reads non-zero after the default only when mimalloc
//! marked it `INITIALIZED` (`options.c`: `mi_option_set_default` writes any
//! option that is not, including one an earlier read left `DEFAULTED`), and
//! only an explicit setting does that: an environment variable, which
//! mimalloc matches CASE-INSENSITIVELY (`prim/unix/prim.c` `_mi_prim_getenv`)
//! and under the legacy name too, or an `mi_option_set` call. So that case is
//! the user's explicit choice under a spelling the hook's own probe missed
//! (`mimalloc_eager_region_commit=2`), not a late hook, and it is left alone
//! (`env`). The z8 hook forced it to 0 with `mi_option_set`; set the switch to
//! `force` for that behaviour.
//!
//! The hook runs before `main`, so it reads its switch with libc `getenv`
//! (nothing here may allocate: an allocation would reserve the first arena
//! with the old setting). The switch is environment-only for that reason.
//!
//! Only the launcher installs mimalloc (`#[global_allocator]` in `main.rs`);
//! `libcratonvm` and `cratonvm-embed` use their host's allocator and are not
//! affected.

use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};

/// mimalloc's `mi_option_arena_eager_commit` (index 4 in both the v2 and v3
/// `mi_option_e` enums: show_errors, show_stats, verbose, eager_commit /
/// deprecated_eager_commit, arena_eager_commit).
#[cfg(all(feature = "mimalloc", not(windows)))]
const MI_OPTION_ARENA_EAGER_COMMIT: std::os::raw::c_int = 4;

/// What the hook did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum LazyCommitOutcome {
    /// The hook never ran (Windows, no mimalloc, or a platform without the
    /// constructor section).
    NotRun = 0,
    /// The lazy default was installed before mimalloc initialised its options.
    Default = 1,
    /// `MIMALLOC_ARENA_EAGER_COMMIT` / `MIMALLOC_EAGER_REGION_COMMIT` was set:
    /// the hook left mimalloc's own reading alone.
    EnvWins = 2,
    /// The option was explicitly initialised under a spelling the probe
    /// missed, and `CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT=force` asked for it to
    /// be overridden anyway: forced with `mi_option_set`.
    Late = 3,
    /// The switch is unset or off (`0`): mimalloc's default (2) kept. The
    /// default since zr z10/mt.
    Off = 4,
}

static OUTCOME: AtomicU8 = AtomicU8::new(LazyCommitOutcome::NotRun as u8);
static VALUE_AFTER: AtomicI64 = AtomicI64::new(-1);

/// Does a switch value mean "off"? `0`, `off`, `false`, `no` (any case).
pub(crate) fn switch_is_off(value: Option<&[u8]>) -> bool {
    match value {
        None => false,
        Some(v) => {
            let v = v.trim_ascii();
            [&b"0"[..], b"off", b"false", b"no"]
                .iter()
                .any(|w| v.eq_ignore_ascii_case(w))
        }
    }
}

/// Does the switch ask for lazy commit? Set, not blank, and not an off word
/// (zr z10/mt: opt-in).
pub(crate) fn switch_is_on(value: Option<&[u8]>) -> bool {
    value.is_some_and(|v| !v.trim_ascii().is_empty()) && !switch_is_off(value)
}

/// Does the switch say `force` (any case)? The z8 behaviour: override an
/// explicit mimalloc setting the environment probe did not see.
pub(crate) fn switch_is_force(value: Option<&[u8]>) -> bool {
    value.is_some_and(|v| v.trim_ascii().eq_ignore_ascii_case(b"force"))
}

/// The decision, without the FFI: `requested` is the CratonVM switch read on
/// ([`switch_is_on`]; unset is off since zr z10/mt), `force` the switch read
/// `force`, `env_set` whether an explicit mimalloc variable is present under one of the spellings the hook probes, and
/// `after_default` the option's value once the default was installed (mimalloc
/// initialises it from the environment on that read). A non-zero
/// `after_default` means mimalloc found an explicit setting (see the module
/// docs), which wins unless `force`.
pub(crate) fn decide(
    requested: bool,
    force: bool,
    env_set: bool,
    after_default: i64,
) -> LazyCommitOutcome {
    if !requested {
        LazyCommitOutcome::Off
    } else if env_set {
        LazyCommitOutcome::EnvWins
    } else if after_default == 0 {
        LazyCommitOutcome::Default
    } else if force {
        LazyCommitOutcome::Late
    } else {
        LazyCommitOutcome::EnvWins
    }
}

/// The `[cratonvm] mimalloc:` line of the `CRATONVM_GC_STATS` summary: the
/// hook's outcome and the option's value as the hook left it.
pub(crate) fn summary_line() -> String {
    let outcome = match OUTCOME.load(Ordering::Relaxed) {
        1 => "default",
        2 => "env",
        3 => "late",
        4 => "off",
        _ => "not-run",
    };
    format!(
        "[cratonvm] mimalloc: lazy_arena_commit={outcome} arena_eager_commit={}",
        VALUE_AFTER.load(Ordering::Relaxed)
    )
}

#[cfg(all(feature = "mimalloc", not(windows)))]
mod hook {
    use super::*;
    use std::os::raw::{c_char, c_int, c_long};

    extern "C" {
        fn mi_option_set_default(option: c_int, value: c_long);
        fn mi_option_set(option: c_int, value: c_long);
        fn mi_option_get(option: c_int) -> c_long;
        fn getenv(name: *const c_char) -> *mut c_char;
    }

    /// `getenv` as bytes, without allocating.
    fn env(name: &'static [u8]) -> Option<&'static [u8]> {
        // SAFETY: `name` is NUL-terminated (every caller passes a `b"..\0"`
        // literal); before `main` no thread can be mutating the environment,
        // and the returned pointer is the process's own environment string.
        let p = unsafe { getenv(name.as_ptr() as *const c_char) };
        if p.is_null() {
            None
        } else {
            // SAFETY: `getenv` returned a NUL-terminated string that lives as
            // long as the environment is not modified.
            Some(unsafe { std::ffi::CStr::from_ptr(p) }.to_bytes())
        }
    }

    extern "C" fn lazy_arena_commit() {
        // `concat!` keeps the switch's name a plain string literal, which is
        // what the flag-declaration guard's reader scan looks for.
        let switch = concat!("CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT", "\0");
        let raw = env(switch.as_bytes());
        let requested = switch_is_on(raw);
        let force = switch_is_force(raw);
        let env_set = env(b"MIMALLOC_ARENA_EAGER_COMMIT\0").is_some()
            || env(b"mimalloc_arena_eager_commit\0").is_some()
            || env(b"MIMALLOC_EAGER_REGION_COMMIT\0").is_some();
        let after = if !requested {
            // SAFETY: plain option read; mimalloc's options table is static.
            unsafe { mi_option_get(MI_OPTION_ARENA_EAGER_COMMIT) }
        } else {
            // SAFETY: as above; `set_default` only writes an uninitialised
            // option, and the read initialises it from the environment.
            unsafe {
                mi_option_set_default(MI_OPTION_ARENA_EAGER_COMMIT, 0);
                mi_option_get(MI_OPTION_ARENA_EAGER_COMMIT)
            }
        };
        let outcome = decide(requested, force, env_set, after as i64);
        let mut value = after as i64;
        if outcome == LazyCommitOutcome::Late {
            // SAFETY: as above.
            unsafe {
                mi_option_set(MI_OPTION_ARENA_EAGER_COMMIT, 0);
                value = mi_option_get(MI_OPTION_ARENA_EAGER_COMMIT) as i64;
            }
        }
        VALUE_AFTER.store(value, Ordering::Relaxed);
        OUTCOME.store(outcome as u8, Ordering::Relaxed);
    }

    /// Priority 100: after std's `.init_array.00099` argv hook, before
    /// mimalloc's process constructor (101 under clang, unprioritised under
    /// gcc). macOS has no priorities in `__mod_init_func`; there the `Late`
    /// fallback covers an earlier init.
    #[used]
    #[cfg_attr(target_vendor = "apple", link_section = "__DATA,__mod_init_func")]
    #[cfg_attr(not(target_vendor = "apple"), link_section = ".init_array.00100")]
    static LAZY_ARENA_COMMIT: extern "C" fn() = lazy_arena_commit;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zr7rs_the_switch_reads_off_words_only() {
        assert!(!switch_is_off(None));
        for v in [&b"0"[..], b"off", b"OFF", b" false ", b"No"] {
            assert!(switch_is_off(Some(v)), "{v:?}");
        }
        for v in [&b"1"[..], b"on", b"", b"2"] {
            assert!(!switch_is_off(Some(v)), "{v:?}");
        }
    }

    #[test]
    fn zr7rs_an_explicit_mimalloc_variable_wins_and_off_keeps_eager() {
        assert_eq!(decide(false, false, false, 2), LazyCommitOutcome::Off);
        assert_eq!(decide(false, false, true, 0), LazyCommitOutcome::Off);
        assert_eq!(decide(true, false, true, 2), LazyCommitOutcome::EnvWins);
        assert_eq!(decide(true, false, true, 0), LazyCommitOutcome::EnvWins);
        assert_eq!(decide(true, false, false, 0), LazyCommitOutcome::Default);
    }

    /// zr z10/mt: opt-in. Unset, blank and the off words leave mimalloc's
    /// eager default; `1`, `on` and `force` ask for lazy commit.
    #[test]
    fn zr10mt_lazy_commit_is_opt_in() {
        assert!(!switch_is_on(None));
        for v in [&b""[..], b"  ", b"0", b"off", b"FALSE", b"no"] {
            assert!(!switch_is_on(Some(v)), "{v:?}");
        }
        for v in [&b"1"[..], b"on", b"true", b"force", b" Force "] {
            assert!(switch_is_on(Some(v)), "{v:?}");
        }
        // Unset reaches `decide` as not requested: eager, whatever the option.
        assert_eq!(
            decide(switch_is_on(None), false, false, 2),
            LazyCommitOutcome::Off
        );
        assert_eq!(
            decide(switch_is_on(Some(b"1")), false, false, 0),
            LazyCommitOutcome::Default
        );
    }

    /// zr z9/rv8: a non-zero option after the default is an explicit setting
    /// mimalloc found under a spelling the probe missed (case-insensitive
    /// match, legacy name), e.g. `mimalloc_eager_region_commit=2`: it wins.
    /// The z8 hook forced it to 0 (`Late`); `force` keeps that for an A/B.
    #[test]
    fn zr9rv8_an_explicit_setting_the_probe_missed_is_not_overridden() {
        assert_eq!(decide(true, false, false, 2), LazyCommitOutcome::EnvWins);
        assert_eq!(decide(true, false, false, 1), LazyCommitOutcome::EnvWins);
        assert_eq!(decide(true, true, false, 2), LazyCommitOutcome::Late);
        assert!(switch_is_force(Some(b" Force ")));
        assert!(!switch_is_force(Some(b"1")));
        assert!(!switch_is_force(None));
        assert!(!switch_is_off(Some(b"force")), "`force` keeps the hook on");
    }

    /// In this very test binary the hook ran before `main` (Linux / macOS
    /// with mimalloc): its outcome is recorded and, when it installed the
    /// lazy default, the option reads 0. Unset (the default since zr z10/mt)
    /// records `Off` and leaves mimalloc's value.
    #[cfg(all(feature = "mimalloc", not(windows)))]
    #[test]
    fn zr7rs_the_hook_ran_before_main() {
        let o = OUTCOME.load(Ordering::Relaxed);
        assert_ne!(o, LazyCommitOutcome::NotRun as u8, "{}", summary_line());
        if o == LazyCommitOutcome::Default as u8 {
            assert_eq!(VALUE_AFTER.load(Ordering::Relaxed), 0);
        }
    }
}
