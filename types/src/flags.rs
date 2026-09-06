// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! One typed, process-lifetime configuration for the `CRATONVM_*` flags.
//!
//! # Why this lives in `cratonvm-types`
//!
//! `vm/src/runtime/env_cache.rs` already caches ~100 flags behind per-flag
//! `OnceLock`s and is the right idea, but it lives in `cratonvm-vm`. `types`,
//! `gc`, `jit` and `classloading` all sit *below* `vm` in the dependency graph
//! and cannot reach it, so each of them grew its own private copy of the same
//! parse — and the copies have drifted at least once in production (see the
//! three-way `CRATONVM_LOADER_AWARE_RESOLUTION` split documented in
//! `env_cache::loader_aware_resolution`).
//!
//! `cratonvm-types` is the one crate every other crate already depends on, and
//! it is where the previous cross-crate concept — `LockLevel` in
//! [`crate::lock_order`] — was put for exactly this reason. So the config goes
//! here.
//!
//! The alternative — a per-crate once-cell that `vm` populates at startup —
//! was rejected because it cannot preserve current behaviour. Today every flag
//! self-initialises on *first read*, from wherever that read happens, and a
//! number of those reads run before `vm` initialises anything:
//! `types::field_layout`'s compact-reference decision, `native_io`'s certified
//! deployment profile, `classloading`'s class-path setup. A config that is only
//! valid after `vm` installs it would silently serve defaults to all of them,
//! which is precisely the silent behaviour change this refactor exists to avoid.
//!
//! # Latching semantics
//!
//! [`flags()`] returns a `&'static VmFlags` initialised on first use from the
//! process environment. This preserves the existing convention — "setting the
//! variable after the first read has no effect" — but *widens* it: the first
//! read of **any** flag now latches **all** of them, where previously each flag
//! latched independently.
//!
//! [`install`] lets a launcher build the config explicitly — environment plus
//! `-XX:` command-line flags — and publish it before anything reads a flag.
//!
//! # Overriding a flag in a test
//!
//! Use [`with_thread_overrides`] (or [`override_thread`] when a guard suits the
//! call site better). A pure-parse test that needs no global state at all
//! should still prefer [`VmFlags::from_source`] with a [`MapSource`].
//!
//! **Do not `set_var` a declared flag.** Because the snapshot latches on first
//! read, `set_var` after any other test in the same binary has touched
//! [`flags()`] mutates `environ` and nothing else — the test then passes only
//! when it wins the race to initialise the snapshot, and quietly measures the
//! developer's ambient environment when it loses. That is an order-dependent
//! test that looks like a flake; the diagnosis is written up in
//! `libcratonvm-no-jdk-test-order-dependent-fixed-20260730.md`, and
//! `types/tests/flag_env_mutation_guard.rs` fails the build if a new one
//! appears. (`set_var` is also `unsafe` under Rust 2024, so the override hooks
//! are what keeps this tree edition-ready.)
//!
//! # Truth tables
//!
//! There is no single answer in this tree to "what does `CRATONVM_FOO=false`
//! mean": the [`parse`] module carries five *different* boolean parsers because
//! five different ones are in use today. `flag-census.md` §10 has
//! the full matrix. Unifying them is a behaviour change and is deliberately not
//! part of this refactor; naming each parser at each field is what makes the
//! divergence visible enough to retire later, flag by flag, with benchmarks.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};

/// FxHash-backed aliases for the two collections on the flag *read* path.
///
/// PERF (2026-08-18, commons-math `BigDecimalBench` profile): `runtime_var_os`
/// consults `declared_flag_names()` on EVERY call, and with the default
/// `RandomState` that is a SipHash of the key plus a `memcmp`. On a
/// `BigDecimal` benchmark that showed up as `hash_one::<&str>` 1.70% +
/// `sip::Hasher::write` 1.41% of the whole process — for looking up string
/// constants in a set that never changes after startup.
///
/// This is the same trade this crate already made for `StringPool` (see the
/// `rustc-hash` dependency note in Cargo.toml): FxHash is ~3-5x faster than
/// SipHash on the short ASCII keys these hold, and neither collection is
/// exposed to untrusted input — the flag-name set is built from a compile-time
/// inventory, and `MapSource` from the process environment — so the HashDoS
/// resistance SipHash buys is not load-bearing here.
type FxHashSetStr = rustc_hash::FxHashSet<&'static str>;
type FxHashMapStr = rustc_hash::FxHashMap<String, OsString>;
/// `name -> Some(value)` for an override that sets, `name -> None` for one
/// that unsets. See [`VmFlags::undeclared_edit`].
type FxHashMapStrOpt = rustc_hash::FxHashMap<String, Option<OsString>>;
use std::ffi::{OsStr, OsString};
use std::sync::atomic::{AtomicPtr, AtomicU8, AtomicUsize, Ordering};
use std::sync::OnceLock;

// ───────────────────────────────────────────────────────────────────────────
// Sources
// ───────────────────────────────────────────────────────────────────────────

/// Where flag values come from.
///
/// Production uses [`EnvSource`]; tests use [`MapSource`] so they never mutate
/// process-global state.
pub trait FlagSource {
    /// Raw value for `name`, exactly as `std::env::var_os` would return it.
    fn get(&self, name: &str) -> Option<OsString>;
}

/// The process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvSource;

impl FlagSource for EnvSource {
    #[inline]
    fn get(&self, name: &str) -> Option<OsString> {
        std::env::var_os(name)
    }
}

/// An explicit map, for tests and for launchers that layer `-XX:` flags over
/// the environment.
#[derive(Debug, Clone, Default)]
pub struct MapSource(FxHashMapStr);

impl MapSource {
    /// Build from `(name, value)` pairs.
    pub fn new<'a, I>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        Self(
            pairs
                .into_iter()
                .map(|(k, v)| (k.to_string(), OsString::from(v)))
                .collect(),
        )
    }

    /// A source with nothing set — the "no environment at all" baseline.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Snapshot of the process environment.
    ///
    /// Taken with **one** walk of `environ` (`std::env::vars_os`) rather than
    /// one `getenv` per field. That matters at this size: `getenv` takes the
    /// environ lock and linearly scans, and [`VmFlags`] now has enough fields
    /// that a per-field probe would be a measurable fixed startup cost — and
    /// would grow with every crate still to be migrated.
    ///
    /// Semantics match `var_os` exactly: `getenv` returns the **first** match
    /// for a duplicated name, so the first entry wins here too. Names that are
    /// not valid UTF-8 are dropped, which is not observable — every lookup is
    /// by `&str`, so such a name could never be matched anyway.
    pub fn from_process_env() -> Self {
        let mut map: FxHashMapStr = FxHashMapStr::default();
        for (name, value) in std::env::vars_os() {
            if let Ok(name) = name.into_string() {
                map.entry(name).or_insert(value);
            }
        }
        Self(map)
    }

    fn declared_snapshot(src: &dyn FlagSource) -> Self {
        let mut map = FxHashMapStr::default();
        for &name in declared_flag_names() {
            if let Some(value) = src.get(name) {
                map.insert(name.to_string(), value);
            }
        }
        Self(map)
    }

    /// Set one entry, builder-style.
    pub fn with(mut self, name: &str, value: &str) -> Self {
        self.0.insert(name.to_string(), OsString::from(value));
        self
    }

    /// Clear one entry, builder-style — the mirror of [`with`](Self::with).
    ///
    /// "Absent" and "set to the empty string" are different for several flags
    /// (`cached_is_set!` in `vm/src/runtime/env_cache.rs` treats the empty
    /// string as *set*), so a source layered over the real environment needs a
    /// way to say "as if this had never been exported", not just "empty".
    pub fn without(mut self, name: &str) -> Self {
        self.0.remove(name);
        self
    }
}

impl FlagSource for MapSource {
    fn get(&self, name: &str) -> Option<OsString> {
        self.0.get(name).cloned()
    }
}

/// The process environment, overlaid with an explicit map that wins.
///
/// This is how a launcher applies `-XX:` flags without calling `set_var`.
#[derive(Debug, Clone, Default)]
pub struct OverlaySource(MapSource);

impl OverlaySource {
    /// Overlay `pairs` on top of the process environment.
    pub fn new(overrides: MapSource) -> Self {
        Self(overrides)
    }
}

impl FlagSource for OverlaySource {
    fn get(&self, name: &str) -> Option<OsString> {
        self.0.get(name).or_else(|| std::env::var_os(name))
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Parsers
// ───────────────────────────────────────────────────────────────────────────

/// The boolean and value parsers in use across the tree.
///
/// Each function reproduces one existing call-site idiom **byte for byte**.
/// Every field in [`VmFlags`] names the parser it was built with, so a reviewer
/// can check the migration against the pre-refactor source without re-deriving
/// the semantics.
pub mod parse {
    use super::FlagSource;
    use std::ffi::OsString;

    /// `std::env::var_os(NAME).is_some()` — presence is truth.
    ///
    /// The overwhelming majority idiom (~600 sites). Note that `NAME=0` and
    /// `NAME=` (empty) are both **enabled** under this parser.
    #[inline]
    pub fn present(src: &dyn FlagSource, name: &str) -> bool {
        src.get(name).is_some()
    }

    /// `std::env::var(NAME).is_ok()` — presence is truth, but invalid UTF-8
    /// reads as absent. Kept distinct from [`present`] so `var` call sites keep
    /// their exact semantics.
    #[inline]
    pub fn present_utf8(src: &dyn FlagSource, name: &str) -> bool {
        utf8(src, name).is_some()
    }

    /// The value as a `String`, matching `std::env::var(NAME).ok()`.
    #[inline]
    pub fn utf8(src: &dyn FlagSource, name: &str) -> Option<String> {
        src.get(name).and_then(|v| v.into_string().ok())
    }

    /// `match var(NAME) { Ok(v) => !v.is_empty() && v != "0", Err(_) => false }`
    ///
    /// Used by `env_cache::disable_jit` and friends: unset, empty and `"0"` are
    /// all **off**; anything else is on.
    #[inline]
    pub fn non_empty_non_zero(src: &dyn FlagSource, name: &str) -> bool {
        match utf8(src, name) {
            Some(v) => !v.is_empty() && v != "0",
            None => false,
        }
    }

    /// `!(empty | "0" | "false" | "off" | "no")`, case-insensitive, trimmed.
    ///
    /// Used by `native_io::env_flag_enabled` for the confinement and strict
    /// defence-in-depth deployment profiles.
    #[inline]
    pub fn truthy_word(src: &dyn FlagSource, name: &str) -> bool {
        match utf8(src, name) {
            Some(v) => {
                let v = v.trim().to_ascii_lowercase();
                !(v.is_empty() || v == "0" || v == "false" || v == "off" || v == "no")
            }
            None => false,
        }
    }

    /// `"1" | "true" | "yes" | "on"`, case-insensitive, trimmed; everything
    /// else — including `"2"` — is false.
    ///
    /// Used by `lock_order::compute_enforced`.
    #[inline]
    pub fn affirmative_word(src: &dyn FlagSource, name: &str) -> bool {
        match utf8(src, name) {
            Some(v) => {
                let v = v.trim();
                v.eq_ignore_ascii_case("1")
                    || v.eq_ignore_ascii_case("true")
                    || v.eq_ignore_ascii_case("yes")
                    || v.eq_ignore_ascii_case("on")
            }
            None => false,
        }
    }

    /// `"1" | "true"` (case-insensitive on `true` only), default `false`.
    ///
    /// Used by `g1::parallel_evac_enabled`.
    #[inline]
    pub fn one_or_true(src: &dyn FlagSource, name: &str) -> bool {
        utf8(src, name)
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    }

    /// Default-ON opt-out: `var(NAME).map(|v| v != "0").unwrap_or(true)`.
    ///
    /// Used by `gen_heap`'s old-gen sweep gate.
    #[inline]
    pub fn on_unless_zero(src: &dyn FlagSource, name: &str) -> bool {
        utf8(src, name).map(|v| v != "0").unwrap_or(true)
    }

    /// Trimmed decimal `usize`, `None` if unset or unparseable.
    #[inline]
    pub fn usize_opt(src: &dyn FlagSource, name: &str) -> Option<usize> {
        utf8(src, name).and_then(|v| v.trim().parse::<usize>().ok())
    }

    /// Trimmed decimal `usize` clamped to `>= 1`, `None` if unset/unparseable.
    #[inline]
    pub fn usize_min1(src: &dyn FlagSource, name: &str) -> Option<usize> {
        usize_opt(src, name).map(|v| v.max(1))
    }

    /// Trimmed decimal `usize`, kept only when `> 0`. Lifted from
    /// `native_builtins::net_phase_e`'s `CRATONVM_HTTP_MAX_BODY`.
    #[inline]
    pub fn usize_positive(src: &dyn FlagSource, name: &str) -> Option<usize> {
        usize_opt(src, name).filter(|&n| n > 0)
    }

    /// Decimal `u64` with **no** trimming, matching the `s.parse::<u64>()`
    /// call sites that do not trim.
    #[inline]
    pub fn u64_opt_untrimmed(src: &dyn FlagSource, name: &str) -> Option<u64> {
        utf8(src, name).and_then(|v| v.parse::<u64>().ok())
    }

    /// Decimal `i64` with **no** trimming.
    #[inline]
    pub fn i64_opt_untrimmed(src: &dyn FlagSource, name: &str) -> Option<i64> {
        utf8(src, name).and_then(|v| v.parse::<i64>().ok())
    }

    /// Hexadecimal address, with an optional `0x` / `0X` prefix; `0` when
    /// unset or unparseable.
    #[inline]
    pub fn hex_addr_or_zero(src: &dyn FlagSource, name: &str) -> usize {
        utf8(src, name)
            .and_then(|s| {
                let s = s.trim();
                let s = s
                    .strip_prefix("0x")
                    .or_else(|| s.strip_prefix("0X"))
                    .unwrap_or(s);
                usize::from_str_radix(s, 16).ok()
            })
            .unwrap_or(0)
    }

    /// `match var(NAME) { Ok(v) => !v.is_empty() && v != "0", Err(_) => true }`
    ///
    /// The default-**ON** twin of [`non_empty_non_zero`]. Used by
    /// `classloading::class_manager::loader_aware_resolution`.
    #[inline]
    pub fn non_empty_non_zero_default_true(src: &dyn FlagSource, name: &str) -> bool {
        match utf8(src, name) {
            Some(v) => !v.is_empty() && v != "0",
            None => true,
        }
    }

    /// `!matches!(value.as_deref(), Ok("0") | Ok("false") | Ok("no"))` — default
    /// ON, disabled only by those three exact lowercase spellings. Used by
    /// `classloading`'s boot module registry.
    #[inline]
    pub fn on_unless_off_word(src: &dyn FlagSource, name: &str) -> bool {
        !matches!(
            utf8(src, name).as_deref(),
            Some("0") | Some("false") | Some("no")
        )
    }

    /// Trimmed `!empty && != "0" && !~ "false"` — default OFF. Used by
    /// `native_io::nio_selector` and `native_io::socket_channel`.
    #[inline]
    pub fn non_empty_non_zero_non_false(src: &dyn FlagSource, name: &str) -> bool {
        utf8(src, name)
            .map(|v| {
                let t = v.trim();
                !t.is_empty() && t != "0" && !t.eq_ignore_ascii_case("false")
            })
            .unwrap_or(false)
    }

    /// `var(NAME).as_deref() == Ok("1")` — only the exact string `1` counts.
    #[inline]
    pub fn exactly_one(src: &dyn FlagSource, name: &str) -> bool {
        utf8(src, name).as_deref() == Some("1")
    }

    /// `matches!(var(NAME).as_deref(), Ok("1") | Ok("true") | Ok("yes"))` —
    /// exact, lowercase-only, untrimmed; `"on"` is **false** here, unlike
    /// [`affirmative_word`]. Truth table 8 (see the module docs and
    /// `flag-census.md` §10). Lifted from
    /// `native_builtins::service_loader`'s `CRATONVM_DIAG_SERVICELOADER`.
    #[inline]
    pub fn one_true_yes_exact(src: &dyn FlagSource, name: &str) -> bool {
        matches!(
            utf8(src, name).as_deref(),
            Some("1") | Some("true") | Some("yes")
        )
    }

    /// `match var(NAME) { Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
    /// Err(_) => true }` — default **ON**, off only for an untrimmed `0` or
    /// `false` in any case. The empty string is **on** here, which is what
    /// separates it from [`non_empty_non_zero_non_false`]. Truth table 9.
    /// Lifted from `native_builtins::lang_system`'s
    /// `CRATONVM_INHERIT_THREAD_CCL`.
    #[inline]
    pub fn on_unless_zero_or_false(src: &dyn FlagSource, name: &str) -> bool {
        match utf8(src, name) {
            Some(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            None => true,
        }
    }

    /// Default **ON**, off only for the five exact spellings `0`, `false`,
    /// `FALSE`, `off`, `OFF`. Mixed case such as `False` leaves it **on**,
    /// which is what separates it from [`on_unless_off_word`]. Truth table 10.
    /// Lifted from `native_builtins::jboss_msc`'s `CRATONVM_MSC_REAL_START`.
    #[inline]
    pub fn on_unless_off_word_cased(src: &dyn FlagSource, name: &str) -> bool {
        !matches!(
            utf8(src, name).as_deref(),
            Some("0") | Some("false") | Some("FALSE") | Some("off") | Some("OFF")
        )
    }

    /// The default-**ON** twin of [`truthy_word`], and not only in the default:
    /// the empty string is **on** here and **off** there. Trimmed and
    /// lowercased; off for `0` / `false` / `off` / `no`. Truth table 11.
    /// Lifted from `native_builtins::reflect_annotations::real_proxy_enabled`.
    #[inline]
    pub fn truthy_word_default_true(src: &dyn FlagSource, name: &str) -> bool {
        match utf8(src, name) {
            Some(v) => {
                let v = v.trim().to_ascii_lowercase();
                !(v == "0" || v == "false" || v == "off" || v == "no")
            }
            None => true,
        }
    }

    /// A non-empty value, `None` when unset or empty.
    #[inline]
    pub fn non_empty_string(src: &dyn FlagSource, name: &str) -> Option<String> {
        utf8(src, name).filter(|s| !s.is_empty())
    }

    /// Trimmed decimal `usize` with no trimming of the *input* — matches the
    /// `s.parse::<usize>()` call sites that do not trim.
    #[inline]
    pub fn usize_opt_untrimmed(src: &dyn FlagSource, name: &str) -> Option<usize> {
        utf8(src, name).and_then(|v| v.parse::<usize>().ok())
    }

    /// Trimmed decimal `i32`, kept only when `> 0`.
    #[inline]
    pub fn i32_positive(src: &dyn FlagSource, name: &str) -> Option<i32> {
        utf8(src, name)
            .and_then(|v| v.trim().parse::<i32>().ok())
            .filter(|&n| n > 0)
    }

    /// Trimmed decimal `u64`, kept only when `> 0`.
    #[inline]
    pub fn u64_positive(src: &dyn FlagSource, name: &str) -> Option<u64> {
        utf8(src, name)
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&n| n > 0)
    }

    /// `Some(true)` for `1`/`true`/`yes`/`on`, `Some(false)` for
    /// `0`/`false`/`no`/`off`, `None` for unset, non-UTF-8, **or an
    /// unrecognised spelling**. Trimmed and lower-cased first. Truth table 12.
    ///
    /// The first parser here whose `None` does not mean "off". Three callers
    /// need that distinction because their default is neither: `ir_verify`'s
    /// `CRATONVM_JIT_VERIFY_IR` and `thread_state`'s
    /// `CRATONVM_STRESS_THREAD_STATES` resolve *unset* to
    /// `cfg!(debug_assertions)` while treating an explicit `0` as off even in a
    /// debug build. Folding those two into one `bool` here would silently arm
    /// the verifier in release builds, so the tri-state is the whole point.
    ///
    /// Lifted from the byte-identical private `env_flag` helpers in
    /// `jit/src/ir_verify.rs` and `jit/src/metrics.rs`.
    #[inline]
    pub fn tristate_word(src: &dyn FlagSource, name: &str) -> Option<bool> {
        let raw = src.get(name)?;
        let value = raw.to_str()?.trim().to_ascii_lowercase();
        match value.as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        }
    }

    /// `Some(false)` for exactly `0` / `false` / `off`, `Some(true)` for any
    /// other value, `None` when unset or not UTF-8. Untrimmed and
    /// case-sensitive. Truth table 13.
    ///
    /// The tri-state twin of [`on_unless_off_word`]: same "off" words, but
    /// "unset" is reported instead of folded into a default, and an
    /// unrecognised spelling reads as **on** (where [`tristate_word`] would
    /// report `None`). Lifted from `vm::threading::thread_state`.
    #[inline]
    pub fn tristate_off_word(src: &dyn FlagSource, name: &str) -> Option<bool> {
        utf8(src, name).map(|v| !matches!(v.as_str(), "0" | "false" | "off"))
    }

    /// The raw OS value, `None` when unset **or empty**.
    ///
    /// The OS-native sibling of [`non_empty_string`], for values that are paths
    /// and must not be forced through UTF-8. Lifted from
    /// `jit::metrics::json_sink`, which reads an exported-but-empty
    /// `CRATONVM_JIT_METRICS_OUT` as "no sink" rather than as a file named `""`.
    #[inline]
    pub fn os_non_empty(src: &dyn FlagSource, name: &str) -> Option<OsString> {
        src.get(name).filter(|v| !v.is_empty())
    }
}

// ───────────────────────────────────────────────────────────────────────────
// GC flags
// ───────────────────────────────────────────────────────────────────────────

/// Blocked-access debug mode (`CRATONVM_DBG_BLOCKED_ACCESS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlockedAccessMode {
    /// Not set, empty, or `0`.
    #[default]
    Off,
    /// Exactly `warn`.
    Warn,
    /// Any other non-empty, non-`0` value.
    Panic,
}

/// Compiled-in default for the moving (compacting) young generation.
///
/// # Default-on contract
///
/// `ARCHITECTURE.md` advertises a "generational semi-space collector (Cheney
/// moving young gen)". The JIT, root gathering, and collector all now read
/// [`GcFlags::moving_young`], so this single default switches them together.
/// Keep this `true`: `CRATONVM_NO_MOVING_YOUNG` is the supported compatibility
/// opt-out and the regression tests below pin both sides of that contract.
///
/// # Flipping this value silently rewires unrelated fixes — check these first
///
/// "Switches them together" is the feature and also the hazard. Several fixes
/// that have nothing to do with compaction are keyed on this flag, so a flip
/// changes whether they engage **without touching a line of their code and
/// without failing a test**. Three casualties are on record, in both
/// directions:
///
/// * **on ⇒ a fix stopped engaging.** `gc_quiescence::young_marker_follows_
///   side_tables` guarded the young class-mirror deferral behind
///   `!moving_young_enabled()`, but the collector term it mirrors for an
///   explicit `System.gc()` (`divert_non_moving`'s `explicit_full_gc`) carries
///   no such condition. Flipping this to `true` in `67de5400a` made the
///   predicate unconditionally `false`, re-opening
///   `TestDefaultInstanceManager.testClassUnloading` for a third time, one day
///   after it was fixed and verified. Class unloading appears in no lane of the
///   flip's own evidence sweep.
/// * **off ⇒ a fix stopped engaging.** The mirror image, found 2026-07-31: the
///   full-GPR safepoint spill, the scratch flush and shadow publication — three
///   ROOT-VISIBILITY mechanisms, none of them moving-specific — were live only
///   because this default is on, so `CRATONVM_NO_MOVING_YOUNG=1` withdrew all
///   three at once and the opt-out lane faulted on a zeroed heap slot within
///   seconds. See `jit-no-moving-young-opt-out-unpublishes-roots-CLOSED-20260803.md`.
/// * **on, but nominally.** For two days after the flip the constant was `true`
///   while every cycle still diverted to the non-moving sweep
///   (`cycles=0 coverage_fallbacks=66`). Anything keyed on the FLAG changed
///   behaviour immediately; anything keyed on the actual collector decision did
///   not. Those are not the same question — see
///   `default-moving-young-enabled-20260730.md`.
///
/// Before changing this value, re-read every site — the sweep is
/// `rg 'moving_young_enabled\(\)' gc/ vm/ jit/` (8 sites as of 2026-08-01) —
/// and for each ask **which** question it means: "will this cycle relocate?"
/// (correctly flag-keyed) or "will some marker follow the loader-scoped side
/// tables / is this oop published?" (a different question that merely
/// correlated). Then run a class-unloading lane, not just the throughput and
/// differential lanes.
///
/// See `moving-young-precise-roots.md`.
pub const DEFAULT_MOVING_YOUNG: bool = true;

/// Whether the JIT publishes a complete, mechanically-enumerable **relocation
/// contract** for its compiled frames — exact frame base, bounded spill band,
/// and every live oop published as a rewritable root at every GC-capable
/// safepoint, for *every* compiled entry kind (single-pass, optimizing IR, OSR,
/// and frames entered by a direct JIT→JIT call, which push no entry guard).
///
/// # Why this exists, and why it is `false`
///
/// It is `false` because that contract does not hold today, and the collector
/// already refuses to trust the JIT because of it. The guarantee comes from the
/// **per-frame coverage proof**, not from any blanket rule: for each live
/// compiled frame `conservative_roots` looks up the `OopMapEntry` matching that
/// frame's recorded safepoint id and requires `moving_young_coverage_complete`.
/// Finding **no** matching map is a refusal too — the check reports "not
/// covered" rather than "nothing to object to". `memory::roots::collect_roots`
/// runs that proof on the path of every collection and `gen_heap` diverts on
/// its verdict, so:
///
/// > A frame that publishes no complete safepoint map can never be live during
/// > a relocating young collection.
///
/// The optimizing IR backend publishes **no `oop_maps` at all** — `ir_lower.rs`
/// emits none, and an empty vector means "no precise coverage" — so an IR frame
/// always fails that proof and always forces the non-moving sweep. A frame
/// entered by a direct JIT→JIT call pushes no entry guard and is likewise not
/// reachable from the chain the proof walks.
///
/// (An earlier version of this note rested instead on a *process-wide* veto
/// that fired whenever compiled code merely existed. That blanket has since
/// become the opt-in `CRATONVM_MOVING_YOUNG_NO_JIT`, leaving the per-cycle
/// proof as the default authority. The invariant the gates below rely on is
/// unaffected: it never needed the blanket, only the per-frame proof, which is
/// strictly narrower and still fails closed for exactly these frame kinds.)
///
/// That fact is what this constant names. Several JIT admission gates
/// were written to protect the moving collector from frames it cannot rewrite —
/// the optimizing IR tier, and direct JIT→JIT calls. Keyed on
/// `moving_young` alone they fire whenever the *flag* is on, which under
/// [`DEFAULT_MOVING_YOUNG`] is always — even though the state they guard against
/// is unreachable. The cost is not theoretical: with the IR gate closed, every
/// compile falls through to the single-pass backend and the optimizing tier
/// contributes nothing (see
/// `jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md`).
///
/// So the gates read this constant *in addition to* `moving_young`, and the
/// runtime veto reads it too. One flip re-arms all of them together, which is
/// the point: a future change that gives the JIT the real contract must not be
/// able to lift the veto while leaving a gate disarmed, or vice versa.
///
/// # What flipping this to `true` requires
///
/// Not just IR safepoint maps. Every obligation the verifier in
/// `conservative_roots` can report must hold in production traffic:
/// unregistered JIT frames on the stack, unavailable exact frame bases,
/// unbounded spill bands, live oops outside the published map, and the
/// cross-thread handshake (`CROSS_THREAD_JIT_PEER`). The blanket veto was
/// introduced precisely because all of those were observed failing.
///
/// This constant does **not** gate the map-publication machinery itself.
/// `x64::shadow_stack_maps_enabled` and `collect_live_oop_homes` stay keyed on
/// `moving_young`, so the single-pass backend keeps emitting and testing the
/// protocol that a future flip depends on.
pub const JIT_PUBLISHES_RELOCATION_CONTRACT: bool = false;

/// Flags read by `cratonvm-gc` (and, for the shared ones, by `jit` and `vm`).
///
/// Unless a field says otherwise it was built with [`parse::present`], i.e. it
/// is `true` whenever the variable is set to anything at all.
#[derive(Debug, Clone, Default)]
pub struct GcFlags {
    // ── Semantics-changing ────────────────────────────────────────────────
    /// The moving (compacting, Cheney) young generation.
    ///
    /// Shaped as an **opt-OUT** (`CRATONVM_NO_MOVING_YOUNG`) over
    /// [`DEFAULT_MOVING_YOUNG`]; `CRATONVM_MOVING_YOUNG` is retained as a
    /// backwards-compatible opt-IN that becomes a no-op once the default
    /// flips. Not [`parse::present`] — see [`GcFlags::from_source`].
    ///
    /// The shared flag is also consumed by JIT code generation, so moving
    /// collection and rewritable root maps cannot be enabled independently.
    ///
    /// The former companion flag `CRATONVM_ALLOW_MOVING_YOUNG` is **gone**. It
    /// was a second opt-in that had to be set *in addition* to this one before
    /// `gen_heap` would run a moving cycle under a live JIT frame — i.e. in
    /// the only situation the feature exists for — which is why the feature
    /// was repeatedly "implemented" and repeatedly still a gap. A flag whose
    /// sole job is to permit correct behaviour is not a safety mechanism; the
    /// per-cycle coverage proof is.
    pub moving_young: bool,
    /// `CRATONVM_MOVING_YOUNG_FALLBACKS` — **verbosity only.**
    ///
    /// Coverage-fallback reporting is now unconditional and at `warn` level
    /// (`gc::gc_quiescence::record_moving_young_coverage_fallback`), because a
    /// silent slide back to the non-moving sweep is exactly how the young
    /// generation stopped being a copying collector unnoticed. This flag now
    /// only asks for *every* occurrence instead of the rate-limited subset.
    pub moving_young_fallbacks: bool,
    /// `CRATONVM_NO_GC_PROMOTION_GUARD` — opt **out** of the promotion-OOM
    /// guard. Consumers want `!no_gc_promotion_guard`.
    pub no_gc_promotion_guard: bool,
    /// `CRATONVM_PROMOTION_OOM_GUARD_BROAD` — widen the promotion-OOM guard.
    pub promotion_oom_guard_broad: bool,
    /// `CRATONVM_NO_SELECTIVE_PROMOTE` — opt **out** of selective promotion,
    /// which is on by default. (The `CRATONVM_SELECTIVE_PROMOTE` spelling
    /// found in 13 documents has never been read; see the census §3.)
    pub no_selective_promote: bool,
    /// `CRATONVM_SP_NO_COALESCE` — opt **out** of selective-promotion region
    /// coalescing.
    pub sp_no_coalesce: bool,
    /// `CRATONVM_NO_DEFRAG_PROMOTE` — opt **out** of the non-moving young
    /// sweep's defragmentation escalation (promote every unpinned survivor,
    /// ignoring `PROMOTION_AGE`, once the young arena's free list can no longer
    /// serve a modest contiguous request). On by default; consumers want
    /// `!no_defrag_promote`.
    pub no_defrag_promote: bool,
    /// `CRATONVM_CARD_TABLE_ONLY` — restrict old→young discovery to the card
    /// table.
    pub card_table_only: bool,
    /// `CRATONVM_GC_FULL_RSET_SCAN` -- restore the whole-old-generation
    /// old->young walk on every young collection.
    ///
    /// **Default OFF since 2026-09-02** (gc-genpause F2); it was the DEFAULT
    /// behaviour before that, reached by the absence of
    /// [`Self::card_table_only`].
    ///
    /// What it turns back on is `scan_all_old_to_young` -- a full
    /// `OldGen::walk_objects()` (which materialises a `Vec` of every tenured
    /// object) scanning every reference slot of every one of them, AFTER the
    /// dirty-card scan has already answered the same question. It made young
    /// pause time grow permanently with old-generation size: measured at
    /// 41-61 ms of a ~229 ms steady-state pause on a heap whose old generation
    /// held no old->young edges at all.
    ///
    /// It was a standing insurance premium against a missed write barrier, and
    /// what replaces it is [`Self::verify_rset`] -- the same insurance as a
    /// verifier you can run, rather than a tax every collection pays. This
    /// flag is the revert lever: set it if a premature-reclamation defect is
    /// suspected and you want the old belt-and-braces seeding back.
    pub full_rset_scan: bool,
    /// `CRATONVM_GC_VERIFY_RSET` -- after each young collection's old->young
    /// seeding, walk the whole old generation and report every edge the card
    /// table did NOT deliver, as `[rset-verify] edges=N missing=M`.
    ///
    /// The generational twin of G1's `CRATONVM_G1_DBG_RSET`, and for the same
    /// reason: a remembered set that is trusted needs a way to be checked, and
    /// a checker whose output cannot distinguish "nothing missing" from "never
    /// looked" is worthless -- so it prints the edge count it verified beside
    /// the misses, and a run that seeded no edges says so.
    ///
    /// Costs a full old-gen walk per young collection, which is exactly the
    /// cost [`Self::full_rset_scan`] used to pay unconditionally. That is the
    /// trade: pay it while you are auditing, not forever.
    pub verify_rset: bool,
    /// `CRATONVM_GC_SYNC_YOUNG_WIPE` — zero the evacuated young semi-space
    /// INSIDE the pause, as every moving cycle did before 2026-09-02, instead
    /// of on a helper thread after it. The revert lever for the off-pause
    /// wipe; the first thing to set if a conservative root ever names the
    /// inactive semi-space.
    pub gc_sync_young_wipe: bool,
    /// `CRATONVM_GC_JIT_REF_STORE_GATES` — let the GENERATIONAL collector
    /// publish the compiled-reference-store barrier plan, so a compiled
    /// reference store gates its barriers inline instead of always calling
    /// `jit_putfield_object`. Default **ON** opt-out
    /// ([`parse::on_unless_zero`]).
    ///
    /// `=0` withholds the plan and every compiled reference store takes the
    /// full helper path exactly as it did before 2026-09-02 — the A/B lever,
    /// and the first thing to set if a compiled store is ever suspected of
    /// missing a card or an SATB entry.
    pub gc_jit_ref_store_gates: bool,
    /// `CRATONVM_OLD_SWEEP_JIT` — default **ON** opt-out for the old-gen
    /// non-moving sweep. [`parse::on_unless_zero`].
    pub old_sweep_jit: bool,
    /// `CRATONVM_G1_PARALLEL_EVAC` — parallel STW evacuation. Default **ON**
    /// opt-out since 2026-08-13 ([`parse::on_unless_zero`]); set `=0` to force
    /// the single-threaded evacuator.
    ///
    /// It was an opt-IN while G1-9 was open — a live-object corruption whose
    /// root cause turned out to be a compact-layout scan divergence in the
    /// parallel evacuator's own object walk, not a race. With that fixed and
    /// covered by a unit regression, the flag's default is the throughput
    /// decision it was always meant to be. `=0` remains the bisection lever for
    /// any suspected parallel-evacuation regression, and a cycle record still
    /// names which evacuator ran.
    pub g1_parallel_evac: bool,
    /// `CRATONVM_G1_PARALLEL_EVAC_SCREEN` — apply the serial evacuator's header
    /// screens on the PARALLEL arm too: the root-is-an-object-start guard, the
    /// per-holder element clamp, and the per-candidate plausibility screen.
    /// Default **ON** ([`parse::on_unless_zero`]); `=0` restores the
    /// pre-2026-09-05 unscreened walks.
    ///
    /// The screens are not new — `scan_and_evacuate_refs`,
    /// `scan_source_region_for_cset_refs` and the two serial drivers' root
    /// loops have had them since 2026-08-26/2026-09-02. They simply never
    /// crossed to the parallel arm, which is the DEFAULT one, so a corrupt or
    /// interior candidate that the serial path refuses was followed, copied and
    /// written through. `=0` is the same-binary A/B for that claim.
    ///
    /// It also stands down `SharedEvac::evacuate`'s last-ditch alignment
    /// refusal, deliberately: an off word that left one guard armed would be a
    /// HALF A/B — the abort it exists to reproduce would not come back, and the
    /// arm would read as evidence that the screens were not the fix.
    pub g1_parallel_evac_screen: bool,
    /// `CRATONVM_G1_EVAC_REF_IMPLAUSIBLE_REFUSE` — a reference-slot candidate
    /// whose legacy header is IMPLAUSIBLE (a class id in the band no loader
    /// mints, class 0 carrying thousands of fields, or — since 2026-09-06 —
    /// `class_id`/`shape` recombining to a pointer into this arena) is REFUSED,
    /// not merely counted. **OPT-IN** ([`parse::non_empty_non_zero`]).
    ///
    /// The ROOT supply route has refused on this screen since 2026-09-02
    /// (`addr_is_followable_object`, whose doc records the eight-kilobyte
    /// object that gating on the tag screen alone produced), and the
    /// reference-slot route runs the same check and discards its answer — so
    /// the parity argument for turning this on is strong.
    ///
    /// It is nonetheless OFF by default, and the reason is measurement rather
    /// than doubt about the parity. On `TestHostConfigAutomaticDeployment-
    /// XmlExternalWarXml` under G1 it refuses 14-16 candidates per run and the
    /// run still SIGSEGVs, so it does not fix what it was reached for; and a
    /// refusal that is WRONG drops a live reference, which produces the
    /// premature-free corruption this whole investigation is about. Defaulting
    /// ON would be trading an understood failure for an unmeasured one. Turn it
    /// on to reproduce the measurement, not to harden a production run.
    pub g1_evac_ref_implausible_refuse: bool,
    /// `CRATONVM_G1_PARALLEL_EVAC_IN_JIT` — let the parallel evacuator run for
    /// pauses taken while a thread is inside compiled code. Default **ON**
    /// ([`parse::on_unless_zero`]); `=0` restores the serial fallback.
    ///
    /// Until F-01 the young driver fell back to the serial evacuator whenever
    /// `gc_quiescence::is_active()`, on the stated ground that "only the serial
    /// path implements conservative-JIT-root region pinning". That ground was
    /// stale: `young_collection_parallel` computes the identical exclusion via
    /// `pinned_region_set_including_non_object_roots`, and its own comment says
    /// it does so deliberately "even if that gate is ever loosened". Pinning is
    /// a collection-set FILTER applied before evacuation begins; nothing in it
    /// requires the evacuation loop to be single-threaded.
    ///
    /// The gate mattered because on a JIT-warm application it is true for
    /// nearly every pause — the audit's own instrumentation recorded 330,263 of
    /// 330,264 pauses with a live compiled frame — so in production G1 copied
    /// on one thread and the persistent worker pool never ran.
    ///
    /// `=0` is the bisection lever, and the FIRST thing to try for any G1
    /// crash or corruption seen only with the JIT warm: under it, JIT-warm
    /// pauses take exactly the evacuator every G1 result before this flag was
    /// produced under. Defect G1-11 (an access violation under
    /// `-XX:+UseG1GC -Xmx32m` with the JIT warm) is open at the time of
    /// writing and lives in this path; reproduce it in both arms before
    /// attributing a change in its frequency to anything else.
    pub g1_parallel_evac_in_jit: bool,
    /// `CRATONVM_G1_EAGER_HUMONGOUS` — reclaim provably-dead humongous spans
    /// during evacuation pauses instead of waiting for a concurrent-mark
    /// cleanup. Default **ON** ([`parse::on_unless_zero`]); set `=0` to restore
    /// cleanup-only reclaim.
    ///
    /// Humongous spans are never evacuated — a young or mixed pause leaves them
    /// where they are — so before this the ONLY thing that ever freed one was
    /// `G1Collector::cleanup`, at the end of a whole concurrent mark cycle. A
    /// program whose humongous garbage is short-lived (the `new byte[4 MiB]`
    /// per request shape) therefore held every dead buffer until IHOP happened
    /// to fire, which on a heap sized for the live set may be never.
    ///
    /// `=0` is the bisection lever: eager reclaim is the only path that frees
    /// memory outside the collection set, so it is the first thing to rule out
    /// if a pause is suspected of losing a live humongous object.
    pub g1_eager_humongous: bool,
    /// `CRATONVM_G1_YOUNG_PAUSE_TARGET` — let `max_gc_pause_ms` bound the
    /// YOUNG generation, not just the old half of a mixed collection set.
    /// **Opt-in** ([`parse::present`]); see the measurement below for why it is
    /// not a default.
    ///
    /// G1's whole proposition is a configurable pause goal, and until this flag
    /// the goal reached exactly one decision: how many OLD regions a mixed
    /// collection set may take. The young half was unbounded — the only thing
    /// that ever asked for a young collection was `needs_gc`, which fires when
    /// the FREE pool falls below ~25% of the heap. So Eden grows to roughly
    /// three quarters of `-Xmx` before the first pause, and young pause time
    /// scales with the heap SIZE rather than with the pause goal: raising
    /// `-Xmx` makes every pause longer, which is the opposite of what a
    /// pause-target collector is for.
    ///
    /// With it on, the collector also collects once the young region count
    /// reaches an adaptive target, shrunk after any pause that overruns
    /// `max_gc_pause_ms` and grown back while pauses stay under half of it. Two
    /// rules keep it from acting on anything but evidence:
    ///
    /// * the target starts at its ceiling AND a target at the ceiling is not a
    ///   trigger, so the cap does nothing at all until a productive pause has
    ///   been measured to overrun the goal. (Starting at the ceiling alone was
    ///   not enough and this doc claimed it was: the ceiling is 60% of the
    ///   region count while the free-pool trigger waits for 75%, so on a heap
    ///   with headroom the ceiling itself fired. Measured at `-Xmx2048m`: a
    ///   210 ms pause manufactured in a run whose other arms took none.)
    /// * an unproductive pause (nothing copied, nothing freed — e.g. everything
    ///   pinned) resets the target to the ceiling, so the cap can never turn
    ///   into a storm of pauses that cannot help.
    ///
    /// # Measured, 2026-08-18 — why this is opt-IN
    ///
    /// `probes/G1ChurnPauseProbe 96 900` under `-Xmx2048m` (96 MiB retained,
    /// 3.6 GiB of garbage, 200 ms goal), 3 interleaved reps, medians, all three
    /// arms from ONE binary except `base` which differs only in `gc/src/g1.rs`
    /// and `gc/src/region.rs`:
    ///
    /// | arm                    | wall    | pauses | total pause | p50     | p99     |
    /// |------------------------|---------|--------|-------------|---------|---------|
    /// | pre-audit baseline     | 5773 ms | 3      | 3801 ms     | 1082 ms | 1726 ms |
    /// | audit, this flag OFF   | 2633 ms | 3      |  719 ms     |  236 ms |  243 ms |
    /// | audit, this flag ON    | 2744 ms | 4      |  814 ms     |  187 ms |  250 ms |
    ///
    /// The 7x pause reduction in that table belongs to the audit's scan and
    /// scrub fixes, NOT to this flag — the middle row has it off. What the flag
    /// itself buys is the third row against the second: p50 -21%, p99 **+3%**,
    /// wall +4.2%, one extra pause.
    ///
    /// p99 is the quantity a pause GOAL is about, and it did not move. It
    /// cannot, on an adaptive scheme: the target only tightens after a pause
    /// has already overrun, so the first (largest) pause is always paid in
    /// full and it is the one p99 reports. The flag delivers a real median
    /// improvement and a real throughput cost, which is a trade a specific
    /// latency-sensitive workload may well want — which is why, on that
    /// measurement, it shipped opt-in.
    ///
    /// # Default ON since 2026-09-02 (ten-findings item 10)
    ///
    /// What made the goal unhonourable was not this sizing but the pause it
    /// was sizing against: every young pause walked the whole old generation
    /// in its fix-up phase (and any humongous span made that unconditional),
    /// and the mixed CSet's copy budget priced only the copying. With the
    /// fix-up narrow on every pause and the mixed budget charged for the
    /// walk's measured cost, the target is the only piece of `MaxGCPauseMillis`
    /// that was still off. `CRATONVM_G1_YOUNG_PAUSE_TARGET=0` restores the
    /// free-pool-only trigger.
    pub g1_young_pause_target: bool,
    /// `CRATONVM_G1_SCRUB_FREE` — zero a region's bytes when a collection
    /// frees it. **Opt-in** ([`parse::present`]); off means the allocator's own
    /// zeroing is relied on, which is where it always came from.
    ///
    /// G1 used to `fill(0)` every reclaimed region. That was the LARGEST single
    /// phase of a young pause — 42% of a 330 ms pause on
    /// `probes/G1ChurnPauseProbe 96 900` at `-Xmx2048m`, freeing 1.61 GB at
    /// 11.9 GB/s, which is memset bandwidth and nothing else. It was also
    /// entirely redundant: `G1Region::bump_alloc` zeroes exactly the range it
    /// hands out (the TLAB zeroing contract that gives a fresh object its
    /// default-zero fields), `alloc_humongous_locked` zeroes its whole span,
    /// every object size is a multiple of 8 so no inter-object padding exists,
    /// and no walker reads a `Free` region at all.
    ///
    /// `=1` restores the scrub. It is the first thing to try if a G1
    /// heap-corruption investigation wants the old "a freed region reads as
    /// zeros" world back — a use-after-free read is the one thing the scrub was
    /// really buying — and it is what makes the change a single-binary A/B.
    pub g1_scrub_free: bool,
    /// `CRATONVM_G1_NARROW_FIXUP` — restrict G1's Phase-4 reference fix-up to
    /// the regions that can actually need it, instead of every non-CSet region
    /// in the heap. Default **ON** ([`parse::on_unless_zero`]); `=0` restores
    /// the whole-heap walk.
    ///
    /// The walk is what makes a young pause O(LIVE HEAP) rather than O(young
    /// live set) — the property G1's region design exists to buy. It visits
    /// every object of every surviving region to rewrite forwarding pointers
    /// and rebuild GC-internal remembered-set edges. Neither job needs the
    /// whole heap: the rewrite is redundant with Phases 2 and 3 once every
    /// mutator store reaches `post_write_barrier_rset` (true since defect G1-2
    /// closed), and the rebuild only concerns regions this pause WROTE INTO.
    /// See `G1Collector::phase4_regions_to_walk`.
    ///
    /// `=0` is the bisection lever, and the FIRST thing to try for any
    /// suspected G1 dangling-reference or lost-edge defect: under it the
    /// collector re-walks the whole heap every pause, which is the behaviour
    /// every G1 result before 2026-08-18 was produced under.
    pub g1_narrow_fixup: bool,
    /// `CRATONVM_G1_CLEANUP_WALK` — make the concurrent-cycle cleanup pause
    /// recompute per-region liveness by WALKING every object of every non-Free
    /// region, instead of reading the per-region byte accumulator the marker
    /// maintains. Opt-in ([`parse::present`]).
    ///
    /// The walk was cleanup's only implementation until F-06: an O(heap)
    /// stop-the-world pass at the end of every concurrent cycle, growing with
    /// the old generation. Real G1 does not have it, because it accumulates the
    /// same number during marking; `G1Region::try_mark_and_account` now does.
    ///
    /// `=1` restores the walk as the authority. It is the single-binary A/B for
    /// the change and the FIRST thing to try if a G1 cycle is suspected of
    /// freeing a live Old region in place — an accumulated liveness is only as
    /// good as the claim that every mark site goes through the accumulator, and
    /// an UNDER-count is exactly what makes a live region look wholly dead. A
    /// debug build runs both and asserts they agree, so the claim is checked
    /// rather than asserted in prose.
    pub g1_cleanup_walk: bool,
    /// `CRATONVM_G1_ADAPTIVE_IHOP` — set the concurrent-marking threshold from
    /// the measured old-generation ALLOCATION RATE and mark duration, instead
    /// of from young-pause time. Default **ON** ([`parse::on_unless_zero`]);
    /// `=0` restores the pause-time model.
    ///
    /// IHOP answers one question: did the concurrent cycle start early enough
    /// that marking finished before the heap filled? The inputs to that are how
    /// fast the old generation grows and how long marking takes. The previous
    /// model fed it young-pause time, which is a property of the young live set
    /// and has no causal relationship to the question — a workload with fast
    /// young pauses and a fast-filling old generation got its threshold RAISED,
    /// which is exactly backwards. The code's own comment records that failure
    /// mode reaching production (61k young collections, no mark cycle, OOM with
    /// >80% of Old dead) and being fixed by clamping the ceiling rather than by
    /// changing the signal.
    ///
    /// `=0` is the bisection lever and the single-binary A/B. Both arms keep
    /// the same static ceiling (`-XX:InitiatingHeapOccupancyPercent`) and the
    /// same floor, so the difference between them is only which evidence moves
    /// the threshold between the two.
    pub g1_adaptive_ihop: bool,
    /// `CRATONVM_G1_ADAPTIVE_TENURING` — re-derive the tenuring threshold after
    /// every evacuation pause from an age histogram of surviving bytes, instead
    /// of always promoting at the configured `promotion_age`. Default **ON**
    /// ([`parse::on_unless_zero`]); `=0` restores the fixed threshold.
    ///
    /// The fixed threshold defaults to 15, so every surviving object was copied
    /// fifteen times before promotion regardless of how full survivor space
    /// was. That is right for a workload whose medium-lived objects are few and
    /// straightforwardly wasteful for one where they are not — a burst that
    /// lives a dozen pauses is copied a dozen times, and the copying is the
    /// expensive half of an evacuation pause.
    ///
    /// The adaptive rule is HotSpot's: the smallest age whose cumulative
    /// surviving bytes exceed the survivor target. It may only tenure EARLIER
    /// than configured, never later, so `-XX:MaxTenuringThreshold`-style intent
    /// is preserved as a ceiling.
    ///
    /// `=0` is the bisection lever for a suspected premature-promotion
    /// regression: under it the collector tenures exactly where every G1 result
    /// before this flag did.
    pub g1_adaptive_tenuring: bool,
    /// `CRATONVM_G1_RESERVE_HEAP` — RESERVE `-Xmx` as address space and COMMIT
    /// only what the collector has actually claimed, instead of allocating and
    /// zeroing the whole heap in the constructor. Default **ON**
    /// ([`parse::on_unless_zero`]); `=0` commits every byte up front.
    ///
    /// Before F-16 there was no `-Xms` (the flag was parsed and discarded), no
    /// expansion and no uncommit, so `-Xmx16g` charged 16 GiB against the
    /// process at startup whether or not a byte of it was used — on Windows,
    /// 16 GiB of commit charge against the page file immediately.
    ///
    /// The committed set is a PREFIX, not an arbitrary subset, because
    /// `gen_heap::publish_jit_read_bounds` asserts "a raw load anywhere in this
    /// range cannot fault" and that claim is only expressible as a range.
    ///
    /// `=0` is the bisection lever, and the first thing to try for any G1 fault
    /// at a heap address that looks mapped: under it the whole reservation is
    /// backed from the start, which is where every G1 result before this flag
    /// was produced. Note that a platform without a reservation implementation
    /// takes that path anyway — `G1Collector::heap_is_reserved` says which.
    pub g1_reserve_heap: bool,
    /// `CRATONVM_G1_UNCOMMIT` — return the pages of a trailing run of Free
    /// regions to the OS at the end of a concurrent-mark cleanup. **Opt-in**
    /// ([`parse::present`]); requires `CRATONVM_G1_RESERVE_HEAP` (the default).
    ///
    /// The other half of F-16. Growth on demand is what stops a large `-Xmx`
    /// costing memory it does not use; this is what lets a process that has
    /// finished a burst give the memory back instead of holding its high-water
    /// mark for its whole life.
    ///
    /// Opt-in because the two halves have different failure modes. Getting
    /// growth wrong is a missed optimisation. Getting the shrink wrong — in
    /// particular, unmapping pages the published JIT read bounds still describe
    /// as loadable — is a fault in compiled code, so it ships behind its own
    /// switch even though the ordering that makes it safe is written down and
    /// tested.
    pub g1_uncommit: bool,
    /// `CRATONVM_GEN_UNCOMMIT` — return the EVACUATED young semi-space to the
    /// OS at the end of each young collection, instead of only zeroing it.
    ///
    /// **Default-ON opt-out since 2026-09-05** ([`parse::on_unless_zero`]);
    /// `=0` restores the zero-only behaviour exactly. It shipped opt-in the same
    /// day and earned the default on a 90/90 HotSpot-differential regression
    /// suite with it (and the exact object-start bitmap) enabled on this
    /// collector, having returned 31457280 bytes of a 96 MB heap on the probe
    /// workload.
    ///
    /// The generational collector was the one backend that never gave memory
    /// back: ZGC does it by default, G1 on request ([`Self::g1_uncommit`]), and
    /// `gen_heap.rs` contained no `decommit` call at all. Its old generation
    /// still cannot — that is a `Vec<u8>`, committed in full at construction,
    /// with no reservation to shrink — but the two young semi-spaces are
    /// `HeapStore`-backed and the INACTIVE one is, by construction, entirely
    /// dead the moment the flip completes.
    ///
    /// THE COST, stated when the default was flipped ON: this collector
    /// publishes its young arenas' FULL reserved range into
    /// `JIT_REGION_BOUNDS` and `JIT_READ_BOUNDS`, and a decommitted granule
    /// FAULTS on touch rather than reading as zero. The window is not created
    /// here — the young arenas already commit lazily while the published bound
    /// covers the whole reservation — but it is WIDENED, from "granules never
    /// yet allocated into" to "granules that held objects one collection ago".
    /// A compiled access through a STALE reference into the evacuated
    /// semi-space therefore moves from reading a stale value to a SIGSEGV.
    ///
    /// # BACK TO OPT-IN, 2026-09-06, because that cost was paid
    ///
    /// `-XX:+UseGenerationalGC` over the H2 JDBC corpus SIGSEGVs in **10
    /// seconds** with this on, and completes with it off. Attribution, one
    /// binary, five arms, `org.h2.test.jdbc.TestPreparedStatement` alone at
    /// `--Xmx 512m`:
    ///
    /// | arm | rc |
    /// |---|---|
    /// | default (this flag ON) | 139, fault addr in a `site=unbumped-middle` span |
    /// | `CRATONVM_GEN_UNCOMMIT=0` | 0 |
    /// | `CRATONVM_GC_RESERVE=0` (nothing decommits) | 0 |
    /// | `--nojit` | 0 |
    /// | `CRATONVM_GC_OBJECT_STARTS=0` (the other flipped default) | 139 |
    ///
    /// `CRATONVM_DBG_STALE_FRAME_WORDS=1` names the holders:
    /// `org/h2/command/Command.stop` and `org/h2/mvstore/tx/Transaction.commit`
    /// keep young addresses this collection moved, in `gpr-safepoint-spill` and
    /// `operand-spill` slots, after every slot the oop maps name has been
    /// rewritten.
    ///
    /// So the fault is exactly the predicted one and the flag is behaving as
    /// documented -- the defect it exposes is a compiled frame's, not this
    /// flag's. **It is the DEFAULT that was wrong.** The evidence for the flip
    /// was a regression suite that does not run this corpus, and the same
    /// measurement that justified the flip put the benefit at "free to within
    /// noise" -- 2552 ms against 2562 ms on the probe workload. A change with
    /// no measurable benefit does not get to crash a supported collector on a
    /// real workload by default. The switch stays, and it is now the sharpest
    /// instrument in the tree for finding a stale young reference: it turns one
    /// from a silent stale read into an immediate, attributable SIGSEGV.
    ///
    /// See the retired cross-collector page's finding 7 for the ON-by-default
    /// reasoning this replaces, and the stale-compiled-frame-reference record
    /// for the defect underneath.
    pub gen_uncommit: bool,
    /// `CRATONVM_G1_CARD_RSET` — F-05: screen G1's Phase-2 remembered-set
    /// source walks against a per-arena CARD TABLE, instead of walking every
    /// byte of every named source region. Default **ON**
    /// ([`parse::on_unless_zero`]); `=0` restores the whole-region walk.
    ///
    /// A remembered-set entry names a source REGION, so acting on one edge cost
    /// a walk of the whole region — every object header validated, every
    /// reference slot visited, a region lookup per slot — i.e. a cost
    /// proportional to BYTES IN THE SOURCE rather than to the number of edges.
    /// The card table records, per 512 bytes, whether a cross-region reference
    /// store ever landed there, so a source with no dirty card is skipped
    /// outright and an object touching no dirty card is stepped over without
    /// any per-slot work. See `gc/src/g1_cards.rs`.
    ///
    /// `=0` is the bisection lever and the FIRST thing to try for a suspected
    /// G1 lost-edge or dangling-reference defect that appears after 2026-09-02:
    /// under it Phase 2 reads no card and walks each source exactly as it did
    /// before. The card table is still MAINTAINED under `=0` (the barrier's
    /// store is unconditional), so the flag isolates the READ side — which is
    /// the side that can lose an edge — rather than half-disabling both.
    pub g1_card_rset: bool,
    /// `CRATONVM_G1_CARD_CLEAN` — clean a card once its objects have been
    /// scanned, instead of leaving it dirty until its region is recycled.
    /// **Opt-in** ([`parse::present`]), and the measurement below is why.
    ///
    /// # The hypothesis, and what measuring it did to it
    ///
    /// F-05 added the card table and cleared a card in exactly one place:
    /// `G1Region::reset`. So the table only ever GAINS bits, and the obvious
    /// reading of the 0.2% byte skip-rate measured on
    /// `probes/HumongousChurn.java` at `-Xmx160m` was saturation: a long-lived
    /// Old region accumulating dirty cards until the screen answers "scan it"
    /// for everything.
    ///
    /// Cleaning is the fix for that, and it works — the unit tests pin all
    /// three directions (a card whose edge is gone is cleaned, a card whose
    /// edge survives is kept, and nothing past the walked extent is touched).
    /// It does not, however, pay for itself on the workload that motivated it.
    /// Four ABBA-interleaved release reps, `HumongousChurn 48 6000 512` at
    /// `-Xmx160m --nojit` (the arm where the screen is actually consulted),
    /// medians:
    ///
    /// | | cleaning off | cleaning on |
    /// |---|---:|---:|
    /// | wall | 7740 ms | 8392 ms |
    /// | total pause | 3054 ms | 3680 ms |
    /// | byte skip-rate | 28.7% | 23.4% |
    ///
    /// Slower, and the skip-rate did not reliably rise. A single earlier run
    /// showed 82% against 45% and would have made a much better story; it was
    /// noise, and four reps is what it took to see that. The cost is real (a
    /// per-region-walk snapshot and a rewrite pass) and the benefit on this
    /// shape is not, because most objects in a retained linked structure hold a
    /// cross-region reference and their cards are kept dirty anyway.
    ///
    /// So the 0.2% is NOT explained by saturation, and this flag is not the
    /// answer to it. What the same runs do show is that the JIT-warm arm skips
    /// 0.79% where the `--nojit` arm skips 20-50% — and the difference is not
    /// the table's contents but how many source regions reach the per-object
    /// screen at all, since a JIT-pinned source is walked WHOLESALE by design
    /// (`card_screen: bool` at `scan_source_region_for_cset_refs`). That is
    /// where the next measurement should go.
    ///
    /// Kept, off, because it is sound, tested, and the mechanism a card table
    /// needs the moment the screen's engagement problem is fixed.
    pub g1_card_clean: bool,
    /// `CRATONVM_G1_CARD_SCREEN_JIT_PINNED` — let Phase 2 apply the card screen
    /// to a JIT-PINNED source region too, instead of walking it wholesale.
    /// Default-on with a `=0` opt-out ([`parse::on_unless_zero`]).
    ///
    /// # The carve-out this removes, and why it was there
    ///
    /// `young_collection`/`mixed_collection` add every JIT-pinned region to the
    /// remembered set's own source list and pass `card_screen = false` for
    /// them, so they are walked end to end. The stated reason is that
    /// "JIT-compiled code may have installed those references through stores
    /// the collector cannot assume went through `post_write_barrier_rset`" — a
    /// card screen is derived from that same assumption, so applying it there
    /// would trust the belt it exists to double.
    ///
    /// # Why the assumption no longer holds under G1
    ///
    /// Every path by which compiled code can write a reference into the heap
    /// now reaches `post_write_barrier_rset`, which records the remembered-set
    /// entry AND dirties the holder's card:
    ///
    /// * **`putfield` (reference), every inline arm, both tiers.** G1-2's fix
    ///   gates each arm on `region_bounds_are_live(...)`, and G1 publishes
    ///   nothing into `JIT_REGION_BOUNDS` — the emptiness is load-bearing and
    ///   `publishing_the_g1_barrier_table_does_not_make_region_bounds_live`
    ///   pins it. So under G1 every arm takes `jit_putfield_object`, which goes
    ///   through `VmHeap::set_field`.
    /// * **`putfield` under `CRATONVM_G1_INLINE_BARRIER` (F-08).** The inline
    ///   filter elides only the two cases whose callee returns without
    ///   recording (a null value, and a same-region store); everything else
    ///   still calls `jit_g1_post_write_barrier`.
    /// * **`aastore`, single-pass tier.** Stores inline, then calls
    ///   `helpers.write_barrier` (`jit_write_barrier` → `VmHeap::write_barrier`
    ///   → `post_write_barrier_rset`). The inline card-mark shortcut beside it
    ///   is generational-only and `inline_card_mark_available()` is a constant
    ///   `false`.
    /// * **`aastore`, IR tier.** Refused outright — `ir_lower` latches a
    ///   bailout rather than emit a barrier-less reference store.
    /// * **Statics, natives, reflection, `Unsafe`, `VarHandle`, `arraycopy`.**
    ///   All funnel through the barriered accessors; none is compiled inline.
    ///
    /// # What is still true, and why this is safe by construction today
    ///
    /// With `CRATONVM_G1_CARD_CLEAN` off (its default), a card is cleared in
    /// exactly one place — `G1Region::reset`, i.e. when the region is recycled.
    /// So a dirty card means "some store ever named an object here" and a CLEAN
    /// card means "no store into this region's contents was ever recorded".
    /// Skipping an object with a clean card can therefore only skip one that no
    /// compiled or interpreted store has touched since the region was recycled.
    ///
    /// `=0` restores the wholesale walk, and is the first thing to try for a
    /// lost-edge or dangling-reference defect that appears after 2026-09-02.
    /// The checkers that would catch one are already wired into every pause:
    /// `verify_no_dangling_into_cset` (budgeted in release) and
    /// `dbg_verify_rset_completeness` (`CRATONVM_G1_DBG_RSET=1`).
    pub g1_card_screen_jit_pinned: bool,
    /// `CRATONVM_G1_INLINE_BARRIER` — F-08: let the JIT emit G1's post-write
    /// barrier inline instead of routing every compiled reference store to the
    /// `jit_putfield_object` helper. Opt-in ([`parse::present`]).
    ///
    /// Closing defect G1-2 (`g1-audit.md` §8.1, §10) made every
    /// JIT-compiled reference store an out-of-line call, because the inline
    /// fast paths are gated on the `JIT_REGION_BOUNDS` table, which G1
    /// deliberately never publishes. §10 measured the cost as falling on the
    /// `n.left = newChild` shape that dominates allocation-heavy code. This
    /// emits a real G1 post-barrier — same-region test, null test, then the
    /// out-of-line remembered-set call — against a SEPARATE published table, so
    /// the G1-2 gate is untouched.
    ///
    /// **Default ON since 2026-09-04**, opt out with
    /// `CRATONVM_G1_INLINE_BARRIER=0` ([`parse::on_unless_zero`]).
    ///
    /// It was default OFF on the reasoning that this is a code-generation
    /// change on an experimental collector, and because the last inline
    /// barrier this JIT had (`Compiler::inline_card_mark_available`, a
    /// different mechanism against a different table) was disabled after a
    /// WildFly boot audit found a missed dirty card — with the note that "`=1`
    /// is how it gets measured before it becomes a default". It was then never
    /// measured, and its only engagement signal was one `tracing::info!` line
    /// saying the arm had been emitted at least once.
    ///
    /// What it was measured at, once a census existed
    /// (`g1-inline-post-write-barrier-measured-20260903.md`):
    ///
    /// * `bt16` under G1, one binary, order alternated: **0.82 s against
    ///   2.22 s, 8 of 8 rounds**, ~2.7x, identical tree checksum in all
    ///   sixteen runs.
    /// * Run-time census: `skipped=29,961,707 called=2,785` — the filter's two
    ///   tests answer "nothing to remember" 99.99% of the time, which is what a
    ///   generational-shaped allocation pattern looks like to G1.
    /// * A 228-program differential soak under G1, and the
    ///   HotSpot-differential regression suite, both with the barrier on.
    /// * `CRATONVM_GC_VERIFY_RSET` clean — the audit that would catch the
    ///   failure mode the WildFly card miss was.
    ///
    /// Why the filter is sound rather than merely fast: both its tests are
    /// transcriptions of `post_write_barrier_rset`'s own first two early-outs
    /// (a null referent, and `src_region == dst_region`), so a skipped call is
    /// a call that would have returned having done nothing, and everything else
    /// reaches the collector's real barrier. The G1-2 gate is untouched: this
    /// reads a SEPARATE published table and `JIT_REGION_BOUNDS` stays empty.
    pub g1_inline_barrier: bool,
    /// `CRATONVM_G1_MARK_LOCK_YIELD` — F-10. Make G1's concurrent marker
    /// release and re-take the regions lock every few objects instead of
    /// holding it for a whole mark step. Default **ON**
    /// ([`parse::on_unless_zero`]); `=0` restores the one-acquisition-per-step
    /// behaviour.
    ///
    /// `ConcurrentMarkController`'s worker calls `concurrent_mark_step(256)` in
    /// a loop, and that call used to take the regions lock once and hold it
    /// until all 256 objects had been scanned. Everything else that touches the
    /// region table — every allocation, every write-barrier slow path, and the
    /// entire stop-the-world pause — waited behind it. "Concurrent" marking was
    /// therefore taking turns with the mutators rather than racing them, at the
    /// exact point in the cycle where the heap fills fastest.
    ///
    /// With `=1` (the default) the marker holds a READ guard for a short batch
    /// of objects and drops it between batches. `parking_lot`'s `RwLock` is
    /// task-fair, so a waiting writer — an STW pause, or a region claim — is
    /// admitted at the next batch boundary instead of at the end of the step.
    ///
    /// `=0` is the bisection lever for any suspected marking-soundness
    /// regression that appeared with F-10: under it the marker's view of the
    /// region table is once again atomic for a whole step, which is the
    /// behaviour every G1 mark cycle before 2026-09-02 ran under.
    pub g1_mark_lock_yield: bool,
    /// `CRATONVM_G1_SHARED_ALLOC` — F-11. Let G1 serve an object allocation or
    /// a TLAB refill out of the current Eden region under a SHARED regions
    /// guard, claiming space with an atomic compare-exchange on the region's
    /// bump cursor. Default **ON** ([`parse::on_unless_zero`]); `=0` sends
    /// every allocation down the exclusive path instead.
    ///
    /// `G1Region::cursor` was a plain `usize`, so bumping it needed
    /// `&mut G1Region`, so every allocation took the collector's one exclusive
    /// lock — the same lock the whole stop-the-world pause and (before F-10)
    /// the concurrent marker held. At the 256 KiB default TLAB against 1 MiB
    /// regions four refills exhaust a region, so this was not a rare path: it
    /// was every thread, continuously, at exactly the moment the heap fills.
    ///
    /// `=0` is the bisection lever. It does not select a different algorithm —
    /// it skips the shared probe and enters the identical slow path, which
    /// re-probes the current Eden under the exclusive guard — so a regression
    /// that survives `=0` is not about the lock. Try it first for any suspected
    /// G1 allocation-corruption or lost-TLAB-zeroing defect: under it the
    /// cursor moves only under exclusion, which is the behaviour every G1 run
    /// before 2026-09-02 was produced under.
    pub g1_shared_alloc: bool,
    /// `CRATONVM_G1_EDEN_STRIPES=<n>` — F-11. How many Eden regions G1 keeps
    /// open for mutator allocation at once. Unset means the machine-derived
    /// default (hardware parallelism, capped at an eighth of the heap's
    /// regions); `=1` is the single global Eden the collector had before F-11.
    ///
    /// Removing the exclusive lock from allocation (`CRATONVM_G1_SHARED_ALLOC`)
    /// only moves the bottleneck if the threads then bump DIFFERENT cursors.
    /// Measured at four threads, shared-guard allocation into one Eden region
    /// was about 1.9x slower per object than the exclusive lock it replaced:
    /// the threads compare-exchange the same word and, because objects are tens
    /// of bytes, write each other's cache lines on the way out, while the
    /// exclusive arm's barging mutex lets one thread run a long cache-hot
    /// burst. Striping is what makes the shared guard pay.
    ///
    /// The knob is a `usize` rather than a boolean because it is also the
    /// fragmentation dial: each stripe holds a partially-filled region that no
    /// pause has reclaimed yet, so `n` regions of Eden are in flight. `=1` is
    /// the bisection lever; a larger `n` than the default is a deliberate
    /// trade of footprint for allocation parallelism.
    pub g1_eden_stripes: Option<usize>,
    /// `CRATONVM_G1_PARALLEL_MARK` — F-12. Run G1's concurrent mark phase on
    /// several workers with per-worker gray deques and work stealing. Default
    /// **ON** ([`parse::on_unless_zero`]); `=0` pins it to the single worker
    /// `ConcurrentMarkController::spawn` used to start unconditionally.
    ///
    /// Marking was one thread draining one `Mutex<Vec<usize>>` gray set, so
    /// even a second worker would have contended on every push and pop. Mark
    /// duration is not only a CPU cost: it sets how much headroom the IHOP
    /// heuristic has to leave before starting a cycle, so a slow marker is paid
    /// for in heap.
    ///
    /// The worker count is a quarter of the evacuation worker count, rounded up
    /// — HotSpot's `ConcGCThreads` ergonomic, and deliberately not the pause's
    /// width, because these workers run BESIDE the application rather than
    /// inside a pause where every core is idle. `CRATONVM_G1_WORKERS=N` still
    /// reaches it through the evacuation count.
    ///
    /// `=0` is the bisection lever: the marking algorithm is identical at one
    /// worker (the deque is the worklist, no steal can succeed, the termination
    /// counter can only be this thread), so a defect that survives `=0` is not
    /// a parallel-marking race.
    pub g1_parallel_mark: bool,
    /// `CRATONVM_G1_DBG_RSET` — after every G1 evacuation pause, verify that
    /// every cross-region reference into a COLLECTABLE region is named in that
    /// region's remembered set. Opt-in diagnostic; whole-heap and O(live
    /// bytes), never a shipping default.
    ///
    /// The complement of `verify_no_dangling_into_cset`, which asks "did this
    /// pause leave a stale pointer?". This asks "will the NEXT pause know where
    /// to look?" — a missing edge means the pause that collects the target
    /// never scans the holder and frees a live object. It exists because the
    /// unit suite cannot discriminate a correct Phase-4 narrowing from one that
    /// walks nothing: on every constructible fixture the mutator barrier alone
    /// already records every edge.
    pub g1_dbg_rset: bool,
    /// `CRATONVM_G1_NO_EVAC_RETRY` — do not retry a failed evacuation.
    pub g1_no_evac_retry: bool,
    /// `CRATONVM_G1_COVERAGE_PIN` — **diagnostic bisection lever, default
    /// OFF.** Make G1 refuse to evacuate on any pause whose JIT root set is
    /// recorded as incomplete, by forcing an empty collection set.
    ///
    /// This is NOT a shipped safety default, and the reason is measured: on
    /// `probes/MovingYoungConcurrentProbe 6 400 2000` under `-XX:+UseG1GC`,
    /// 330263 of 330264 pauses report incomplete coverage, because the flag
    /// means "this collection's JIT roots are not REWRITABLE" — the normal
    /// state whenever any thread is in compiled code — not "this collection's
    /// JIT roots were not ENUMERATED". Refusing on it starves reclamation: the
    /// same run needed ONE collection with the lever off and took 330264 no-op
    /// pauses with it on.
    ///
    /// What it is for: deciding whether a G1-only crash is a root-coverage
    /// defect at all. Under this lever G1 moves nothing, so a crash that
    /// survives it is not caused by a relocation the root set failed to cover.
    /// See `G1Collector::root_coverage_incomplete_reason`, whose DETECTION is
    /// deliberately not gated on this flag — the counters report the rate in
    /// both arms.
    pub g1_coverage_pin: bool,
    /// `CRATONVM_G1_PIN_EMPTY_PUBLICATION` — **diagnostic bisection lever,
    /// default OFF.** Make G1 refuse to evacuate on any pause that runs with a
    /// live compiled frame and an EMPTY conservative JIT root publication, by
    /// forcing an empty collection set.
    ///
    /// The state it detects is a real defect: with `jit_active=true`,
    /// `pin_addrs=0` is being consumed as "there are no JIT roots" when it
    /// actually means "the conservative scan found none", which is unknown, not
    /// none. Evacuating against it is what moved a live
    /// `StringLatin1.newString` reference out from under a compiled frame
    /// (`bug-g1-evacuates-live-jit-reference-20260819-FIXED.md`).
    ///
    /// **It ships OFF because a refusal reclaims nothing**, so it can only buy
    /// time for a publication that later becomes non-empty. Measured before the
    /// root-scan fix below, when the state was permanent: 1444 consecutive
    /// refused pauses on `PolynomialTest` and `OutOfMemoryError` on 8 tests,
    /// instead of the original wrong answer on 1.
    ///
    /// That measurement also showed the predicate was not detecting what it
    /// claimed. `pin_addrs=0` was the ordinary appearance of PRECISE mode:
    /// `collect_roots` skipped the conservative JIT scan whenever the oop-map
    /// coverage proof passed, and G1's pin set is built from that scan alone.
    /// The real repair was to stop G1 taking that branch (see
    /// `CRATONVM_G1_PRECISE_ONLY_ROOTS`, which restores the broken behaviour
    /// for A/B). With the scan always running under G1, an empty publication
    /// means what this flag's name says again.
    ///
    /// The DETECTION counter
    /// (`gc_metrics::record_g1_pause_empty_jit_publication`) is deliberately
    /// NOT gated on this flag, so a normal run still reports how often the
    /// state occurs. It should now be zero; the refusal is a bisection lever
    /// for the day it is not.
    pub g1_pin_empty_publication: bool,
    /// `CRATONVM_G1_WORKERS` — override the G1 worker count, clamped to `>= 1`.
    /// [`parse::usize_min1`].
    pub g1_workers: Option<usize>,
    /// `CRATONVM_GC_SWEEP_ANCHOR_STRIDE` — byte spacing at which the parallel
    /// young sweep subsamples the allocator-recorded object grid. Values below
    /// 64 are ignored; default 8 MiB. Lowering it is how the GC-stress matrix
    /// forces multiple sweep chunks on a small young gen.
    pub gc_sweep_anchor_stride: usize,
    /// `CRATONVM_GC_PAR_THREADS` — explicit young-collector worker count.
    /// `0`/`1` disable parallelism; `>= 2` forces that many workers regardless
    /// of heap size. `None` = automatic. **Not** clamped to `>= 1`: zero is a
    /// meaningful value here, so this is [`parse::usize_opt`], not
    /// [`parse::usize_min1`].
    pub gc_par_threads: Option<usize>,
    /// `CRATONVM_GC_PAR_MIN_BYTES` — young-gen size below which parallelism
    /// never pays for itself; default 16 MiB.
    pub gc_par_min_bytes: usize,
    /// `CRATONVM_GC_PAR_EVAC` — run the generational young collector's MOVING
    /// (Cheney) copy phase on the same worker set its mark phase already uses.
    /// Default **ON** ([`parse::on_unless_zero`]); set `=0` to force the
    /// single-threaded evacuator.
    ///
    /// On by default because it cannot engage on its own: the copy phase only
    /// goes parallel when [`Self::gc_par_threads`] policy already asked for
    /// two or more workers (which needs a young gen past
    /// [`Self::gc_par_min_bytes`], or an explicit request) AND the cycle is a
    /// moving one AND to-space has the per-worker-buffer slack. Defaulting it
    /// off would leave the parallel copy inert on every workload that has the
    /// heap for it, which is the state a gated feature dies in.
    ///
    /// `=0` is the bisection lever: the serial and parallel evacuators produce
    /// the same forwarding map, so a suspected regression is one run apart.
    pub gc_par_evac: bool,
    /// `CRATONVM_DBG_GC_STRESS`, falling back to `CRATONVM_GC_STRESS` — force
    /// a GC every N bytes allocated. Values `<= 0` and unparseable values are
    /// treated as unset. Despite the `DBG_` name this changes GC scheduling,
    /// so it is classed as semantics-changing.
    pub gc_stress_bytes: Option<usize>,
    /// `CRATONVM_DBG_FORCE_MOVING` — force the moving collector.
    pub dbg_force_moving: bool,
    /// `CRATONVM_DBG_NO_NONMOVING_RECLAIM` — defer non-moving reclamation.
    pub dbg_no_nonmoving_reclaim: bool,
    /// `CRATONVM_DBG_COMPACT_LEGACY` — take the legacy compaction path.
    pub dbg_compact_legacy: bool,
    /// `CRATONVM_DBG_SEED_ALL_OLD` — seed marking from every old-gen object.
    pub dbg_seed_all_old: bool,
    /// `CRATONVM_DBG_STALE_OBJREF` — quarantine evacuated young arenas and
    /// panic on a stale `ObjectRef` read.
    pub dbg_stale_objref: bool,
    /// `CRATONVM_DBG_STALE_OBJREF_CYCLES` — how many minor cycles a quarantined
    /// arena is kept. Default 1; values below 1 are ignored.
    /// [`parse::usize_min1`], defaulted.
    pub dbg_stale_objref_cycles: usize,
    /// `CRATONVM_DBG_BLOCKED_ACCESS` — off / warn / panic.
    pub dbg_blocked_access: BlockedAccessMode,

    // ── Verification / assertions ─────────────────────────────────────────
    /// `CRATONVM_FWD_RESOLVE_STRICT` — hard-fail instead of warning when a
    /// forwarding pointer cannot be resolved.
    pub fwd_resolve_strict: bool,
    /// `CRATONVM_MOVING_YOUNG_VERIFY` — run the post-evacuation verifier.
    pub moving_young_verify: bool,
    /// `CRATONVM_SP_VERIFY` — verify selective promotion.
    pub sp_verify: bool,
    /// `CRATONVM_GC_ARRAY_GUARD_BT` — include a backtrace in the array-guard
    /// panic message.
    pub gc_array_guard_bt: bool,

    // ── Pure diagnostics ──────────────────────────────────────────────────
    /// `CRATONVM_DBG_A2`
    pub dbg_a2: bool,
    /// `CRATONVM_DBG_SWEEP_LIVENESS` — presence gate for the old-gen sweep's
    /// freed-while-referenced assertion. [`parse::present`].
    pub dbg_sweep_liveness: bool,
    /// `CRATONVM_DBG_SWEEP_LIVENESS` — the value, so `=rescue` selects the
    /// retain-instead-of-free differential mode; any other value reports only.
    pub dbg_sweep_liveness_value: Option<String>,
    /// `CRATONVM_DBG_BADREF`
    pub dbg_badref: bool,
    /// `CRATONVM_DBG_CELLCORRUPT`
    pub dbg_cellcorrupt: bool,
    /// `CRATONVM_DBG_DESCTRACE`
    pub dbg_desctrace: bool,
    /// `CRATONVM_DBG_FWDGUARD`
    pub dbg_fwdguard: bool,
    /// `CRATONVM_DBG_GCPAUSE`
    pub dbg_gcpause: bool,
    /// `CRATONVM_DBG_GCPHASE`
    pub dbg_gcphase: bool,
    /// `CRATONVM_DBG_GCWRITE`
    pub dbg_gcwrite: bool,
    /// `CRATONVM_DBG_HEAP_TRACE`
    pub dbg_heap_trace: bool,
    /// `CRATONVM_DBG_MIRRORPIN`
    pub dbg_mirrorpin: bool,
    /// `CRATONVM_DBG_OOBFIELD` — presence gate. [`parse::present`].
    pub dbg_oobfield: bool,
    /// `CRATONVM_DBG_OOBFIELD` — the value, when it is valid UTF-8; the same
    /// variable is read both ways at different sites. [`parse::utf8`].
    pub dbg_oobfield_value: Option<String>,
    /// `CRATONVM_DBG_PRECISE`
    pub dbg_precise: bool,
    /// `CRATONVM_DBG_RSET_AUDIT`
    pub dbg_rset_audit: bool,
    /// `CRATONVM_DBG_RSET_AUDIT_YOUNG_SCAN`
    pub dbg_rset_audit_young_scan: bool,
    /// `CRATONVM_DBG_SEEDHUNT`
    pub dbg_seedhunt: bool,
    /// `CRATONVM_DBG_SWEEP_CENSUS`
    pub dbg_sweep_census: bool,
    /// `CRATONVM_DBG_SWEEP_EDGES`
    pub dbg_sweep_edges: bool,
    /// `CRATONVM_DBG_SWEEP_ZERO`
    pub dbg_sweep_zero: bool,
    /// `CRATONVM_GC_LATE_RESOLVE_DROPPED` -- also run the late grid-resolution
    /// pass over the candidates `mark_young` DROPPED as free/gap space inside a
    /// proved anchor span, not only the ones it left unresolved. Over-retention
    /// only. Default off; see `gen_heap.rs` for the defect it was opened for.
    pub late_resolve_dropped: bool,
    /// `CRATONVM_DBG_WATCHREF`
    pub dbg_watchref: bool,
    /// `CRATONVM_DBG_WATCH_CELL` — hex address to watch, `0` when disabled.
    /// [`parse::hex_addr_or_zero`].
    pub dbg_watch_cell: usize,
    /// `CRATONVM_DBG_YOUNGSTATE`
    pub dbg_youngstate: bool,
    /// `CRATONVM_DBG_ZERO_RANGES`
    pub dbg_zero_ranges: bool,
    /// `CRATONVM_DIAG_HIB32`
    pub diag_hib32: bool,
    /// `CRATONVM_NO_EXACT_REFPROC_SURVIVAL` — bisection escape hatch:
    /// restore the pre-fix permissive survival predicate in post-GC reference
    /// processing (every old-gen address counts as a survivor, even in a cycle
    /// that reclaimed old gen). See `VmHeap::watched_pre_gc_addr_survived`;
    /// setting this reinstates the stale-address writes that fix repaired, so
    /// it is for A/B isolation only.
    pub no_exact_refproc_survival: bool,
    /// `CRATONVM_NO_REFERENT_IDENTITY_SCREEN` — measurement-only escape
    /// hatch: let the post-GC referent restore write back an object it cannot
    /// prove is still the referent, which is the pre-fix behaviour. Setting
    /// this reinstates a wrong-object store into `Reference.referent` under a
    /// compacting collector; it exists so the fix can be A/B'd in ONE binary.
    /// See `process_references_after_gc`'s restore loop.
    pub no_referent_identity_screen: bool,
    /// `CRATONVM_NO_OLDGEN_COALESCE` — bisection escape hatch: never merge
    /// adjacent old-gen free blocks outside `compact`, restoring the pre-fix
    /// behaviour in which an in-place sweep fragments the generation
    /// monotonically. See `OldGen::coalesce_free_blocks`.
    pub no_oldgen_coalesce: bool,
    /// `CRATONVM_G1_DBG_HEADERS`
    pub g1_dbg_headers: bool,
    /// `CRATONVM_DBG_G1DIAG` — print region-type counts (free/eden/survivor/
    /// old/pinned) before and after every `collect_garbage()` call, and a
    /// fuller breakdown (incl. humongous) right before the "out of heap
    /// space" abort. Diagnostic aid for tracing G1 region-pool exhaustion;
    /// see g1-native-alloc-no-safepoint-oom-FIXED.md.
    pub g1_dbg_diag: bool,
    /// `CRATONVM_DBG_G1ACCESSOR` — at VM exit, print how many of this
    /// collector's field/array accessor calls had to take the global `regions`
    /// lock, and how many answered from `may_be_humongous` without it.
    ///
    /// The load-independent half of the accessor-lock measurement: absolute
    /// wall time on a shared box is worth a factor of several, but "1 lock
    /// acquisition per store" versus "0" is a structural fact that no
    /// concurrent build can move. See `G1Collector::may_be_humongous`.
    pub g1_dbg_accessor: bool,
    /// `CRATONVM_G1_DBG_PINS`
    pub g1_dbg_pins: bool,
    /// `CRATONVM_G1_DBG_REACH`
    /// `CRATONVM_G1_VERIFY_HOLDERS` — re-validate a worklist holder's header
    /// before walking its slots (ten-findings item 4).
    ///
    /// Default OFF. The holder is a to-space copy `evacuate_object` already
    /// screened at both ends, and the slot loops clamp their bounds to the
    /// holder's own region rather than trusting its header, so the check buys
    /// no safety in the pause -- only one cold cache line per holder. It stays
    /// as an instrument for the one defect that would otherwise be invisible:
    /// a copy overwritten WITHIN the pause that made it.
    /// `CRATONVM_G1_HUMONGOUS_MARKS` — mark a humongous span "referenced" as
    /// the pause's slot loops scan into it (ten-findings item 1).
    ///
    /// Default **OFF**. Correct and cheap in isolation, but it reads
    /// `regions[idx].region_type` for every scanned slot, which after item 4 is
    /// the only region-table access left on the old->old path. Measured
    /// engagement on 2026-09-05 was ZERO spans decided, on every workload
    /// tried: a humongous span's holders are old objects the pause never scans,
    /// which is exactly why the remembered-set walk exists.
    pub g1_humongous_marks: bool,
    /// `CRATONVM_G1_IHOP_COUNTS_REGIONS=0` — measure old-generation occupancy
    /// for IHOP by summing live bytes instead of counting the regions the old
    /// generation has taken.
    ///
    /// Default **OFF**, on measurement rather than on principle. The argument
    /// for it is sound -- an Old region is unavailable whether it is 5% or
    /// 100% full, and 211 regions holding 44.7 MB read as 25% of a 179 MB
    /// threshold they can never cross -- and it does what it claims: concurrent
    /// marking engaged in 2 of 6 H2 runs against 0 of 6 for the byte count.
    ///
    /// But engaging is not helping. `to_space_exhausted` over the same 12 runs
    /// was 28/40/37/45/30/98 with it on against 4/21/55/22/25/16 with it off --
    /// no better, plausibly worse, on the only outcome that matters here. A
    /// mark cycle that starts is still not a mixed collection that reclaims,
    /// and until the rest of that chain is understood this changes when G1
    /// spends effort without changing what it gets back.
    pub g1_ihop_counts_regions: bool,
    pub g1_verify_holders: bool,
    pub g1_dbg_reach: bool,
    /// `CRATONVM_G1_DBG_ROOTCENSUS`
    pub g1_dbg_rootcensus: bool,
    /// `CRATONVM_G1_DBG_ZERO`
    pub g1_dbg_zero: bool,
    /// `CRATONVM_SP_STATS`
    pub sp_stats: bool,
    /// `CRATONVM_SP_TRACE`
    pub sp_trace: bool,

    /// `CRATONVM_IDENTITY_HASH_EVICT` — [`parse::on_unless_zero`].
    ///
    /// Whether the sweep tells the native side tables keyed by identity
    /// hash which of their keys just died
    /// (`cratonvm_types::identity_side_tables`). ON by default: with it
    /// off, every such table grows for the life of the process, which is
    /// the native-memory leak the flag exists to A/B rather than a
    /// behaviour anyone should choose.
    ///
    /// It is a kill switch for exactly that measurement — one binary,
    /// two arms — and for the case where a table turns out to key
    /// something whose lifetime is NOT the object's.
    pub identity_hash_evict: bool,
}

impl GcFlags {
    fn from_source(src: &dyn FlagSource) -> Self {
        use parse::*;
        Self {
            // Opt-OUT over the compiled-in default, with the historical
            // opt-in still honoured. Deliberately NOT `present`: this is the
            // one gate that decides whether the young generation compacts,
            // and it must be flippable by changing `DEFAULT_MOVING_YOUNG`
            // alone. See that constant for the remaining blocker.
            moving_young: if present(src, "CRATONVM_NO_MOVING_YOUNG") {
                false
            } else if present(src, "CRATONVM_MOVING_YOUNG") {
                true
            } else {
                DEFAULT_MOVING_YOUNG
            },
            moving_young_fallbacks: present(src, "CRATONVM_MOVING_YOUNG_FALLBACKS"),
            no_gc_promotion_guard: present(src, "CRATONVM_NO_GC_PROMOTION_GUARD"),
            promotion_oom_guard_broad: present(src, "CRATONVM_PROMOTION_OOM_GUARD_BROAD"),
            no_selective_promote: present(src, "CRATONVM_NO_SELECTIVE_PROMOTE"),
            sp_no_coalesce: present(src, "CRATONVM_SP_NO_COALESCE"),
            no_defrag_promote: present(src, "CRATONVM_NO_DEFRAG_PROMOTE"),
            card_table_only: present(src, "CRATONVM_CARD_TABLE_ONLY"),
            full_rset_scan: present(src, "CRATONVM_GC_FULL_RSET_SCAN"),
            verify_rset: present(src, "CRATONVM_GC_VERIFY_RSET"),
            gc_sync_young_wipe: present(src, "CRATONVM_GC_SYNC_YOUNG_WIPE"),
            gc_jit_ref_store_gates: on_unless_zero(src, "CRATONVM_GC_JIT_REF_STORE_GATES"),
            old_sweep_jit: on_unless_zero(src, "CRATONVM_OLD_SWEEP_JIT"),
            g1_parallel_evac: on_unless_zero(src, "CRATONVM_G1_PARALLEL_EVAC"),
            g1_parallel_evac_screen: on_unless_zero(src, "CRATONVM_G1_PARALLEL_EVAC_SCREEN"),
            g1_evac_ref_implausible_refuse: non_empty_non_zero(
                src,
                "CRATONVM_G1_EVAC_REF_IMPLAUSIBLE_REFUSE",
            ),
            g1_parallel_evac_in_jit: on_unless_zero(src, "CRATONVM_G1_PARALLEL_EVAC_IN_JIT"),
            g1_eager_humongous: on_unless_zero(src, "CRATONVM_G1_EAGER_HUMONGOUS"),
            g1_young_pause_target: on_unless_zero(src, "CRATONVM_G1_YOUNG_PAUSE_TARGET"),
            g1_scrub_free: present(src, "CRATONVM_G1_SCRUB_FREE"),
            g1_narrow_fixup: on_unless_zero(src, "CRATONVM_G1_NARROW_FIXUP"),
            g1_cleanup_walk: present(src, "CRATONVM_G1_CLEANUP_WALK"),
            g1_adaptive_ihop: on_unless_zero(src, "CRATONVM_G1_ADAPTIVE_IHOP"),
            g1_adaptive_tenuring: on_unless_zero(src, "CRATONVM_G1_ADAPTIVE_TENURING"),
            g1_reserve_heap: on_unless_zero(src, "CRATONVM_G1_RESERVE_HEAP"),
            g1_uncommit: present(src, "CRATONVM_G1_UNCOMMIT"),
            gen_uncommit: non_empty_non_zero(src, "CRATONVM_GEN_UNCOMMIT"),
            g1_card_rset: on_unless_zero(src, "CRATONVM_G1_CARD_RSET"),
            g1_card_clean: present(src, "CRATONVM_G1_CARD_CLEAN"),
            g1_card_screen_jit_pinned: on_unless_zero(
                src,
                "CRATONVM_G1_CARD_SCREEN_JIT_PINNED",
            ),
            g1_inline_barrier: on_unless_zero(src, "CRATONVM_G1_INLINE_BARRIER"),
            g1_mark_lock_yield: on_unless_zero(src, "CRATONVM_G1_MARK_LOCK_YIELD"),
            g1_shared_alloc: on_unless_zero(src, "CRATONVM_G1_SHARED_ALLOC"),
            g1_eden_stripes: usize_min1(src, "CRATONVM_G1_EDEN_STRIPES"),
            g1_parallel_mark: on_unless_zero(src, "CRATONVM_G1_PARALLEL_MARK"),
            identity_hash_evict: on_unless_zero(src, "CRATONVM_IDENTITY_HASH_EVICT"),
            g1_dbg_rset: present(src, "CRATONVM_G1_DBG_RSET"),
            g1_no_evac_retry: present(src, "CRATONVM_G1_NO_EVAC_RETRY"),
            g1_coverage_pin: present(src, "CRATONVM_G1_COVERAGE_PIN"),
            g1_pin_empty_publication: present(src, "CRATONVM_G1_PIN_EMPTY_PUBLICATION"),
            g1_workers: usize_min1(src, "CRATONVM_G1_WORKERS"),
            gc_sweep_anchor_stride: usize_opt(src, "CRATONVM_GC_SWEEP_ANCHOR_STRIDE")
                .filter(|&n| n >= 64)
                .unwrap_or(8 * 1024 * 1024),
            gc_par_threads: usize_opt(src, "CRATONVM_GC_PAR_THREADS"),
            gc_par_min_bytes: usize_opt(src, "CRATONVM_GC_PAR_MIN_BYTES")
                .unwrap_or(16 * 1024 * 1024),
            gc_par_evac: on_unless_zero(src, "CRATONVM_GC_PAR_EVAC"),
            gc_stress_bytes: utf8(src, "CRATONVM_DBG_GC_STRESS")
                .or_else(|| utf8(src, "CRATONVM_GC_STRESS"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|&v| v > 0),
            dbg_force_moving: present(src, "CRATONVM_DBG_FORCE_MOVING"),
            dbg_no_nonmoving_reclaim: present(src, "CRATONVM_DBG_NO_NONMOVING_RECLAIM"),
            dbg_compact_legacy: present(src, "CRATONVM_DBG_COMPACT_LEGACY"),
            dbg_seed_all_old: present(src, "CRATONVM_DBG_SEED_ALL_OLD"),
            dbg_stale_objref: present(src, "CRATONVM_DBG_STALE_OBJREF"),
            dbg_stale_objref_cycles: usize_opt_untrimmed(src, "CRATONVM_DBG_STALE_OBJREF_CYCLES")
                .filter(|&n| n >= 1)
                .unwrap_or(1),
            dbg_blocked_access: match utf8(src, "CRATONVM_DBG_BLOCKED_ACCESS") {
                Some(ref v) if v == "warn" => BlockedAccessMode::Warn,
                Some(ref v) if !v.is_empty() && v != "0" => BlockedAccessMode::Panic,
                _ => BlockedAccessMode::Off,
            },

            fwd_resolve_strict: present(src, "CRATONVM_FWD_RESOLVE_STRICT"),
            moving_young_verify: present(src, "CRATONVM_MOVING_YOUNG_VERIFY"),
            sp_verify: present(src, "CRATONVM_SP_VERIFY"),
            gc_array_guard_bt: present(src, "CRATONVM_GC_ARRAY_GUARD_BT"),

            dbg_a2: present(src, "CRATONVM_DBG_A2"),
            dbg_sweep_liveness: present(src, "CRATONVM_DBG_SWEEP_LIVENESS"),
            dbg_sweep_liveness_value: utf8(src, "CRATONVM_DBG_SWEEP_LIVENESS"),
            dbg_badref: present(src, "CRATONVM_DBG_BADREF"),
            dbg_cellcorrupt: present(src, "CRATONVM_DBG_CELLCORRUPT"),
            dbg_desctrace: present(src, "CRATONVM_DBG_DESCTRACE"),
            dbg_fwdguard: present(src, "CRATONVM_DBG_FWDGUARD"),
            dbg_gcpause: present(src, "CRATONVM_DBG_GCPAUSE"),
            dbg_gcphase: present(src, "CRATONVM_DBG_GCPHASE"),
            dbg_gcwrite: present(src, "CRATONVM_DBG_GCWRITE"),
            dbg_heap_trace: present(src, "CRATONVM_DBG_HEAP_TRACE"),
            dbg_mirrorpin: present(src, "CRATONVM_DBG_MIRRORPIN"),
            dbg_oobfield: present(src, "CRATONVM_DBG_OOBFIELD"),
            dbg_oobfield_value: utf8(src, "CRATONVM_DBG_OOBFIELD"),
            dbg_precise: present(src, "CRATONVM_DBG_PRECISE"),
            dbg_rset_audit: present(src, "CRATONVM_DBG_RSET_AUDIT"),
            dbg_rset_audit_young_scan: present(src, "CRATONVM_DBG_RSET_AUDIT_YOUNG_SCAN"),
            dbg_seedhunt: present(src, "CRATONVM_DBG_SEEDHUNT"),
            dbg_sweep_census: present(src, "CRATONVM_DBG_SWEEP_CENSUS"),
            dbg_sweep_edges: present(src, "CRATONVM_DBG_SWEEP_EDGES"),
            dbg_sweep_zero: present(src, "CRATONVM_DBG_SWEEP_ZERO"),
            late_resolve_dropped: present(src, "CRATONVM_GC_LATE_RESOLVE_DROPPED"),
            dbg_watchref: present(src, "CRATONVM_DBG_WATCHREF"),
            dbg_watch_cell: hex_addr_or_zero(src, "CRATONVM_DBG_WATCH_CELL"),
            dbg_youngstate: present(src, "CRATONVM_DBG_YOUNGSTATE"),
            dbg_zero_ranges: present(src, "CRATONVM_DBG_ZERO_RANGES"),
            diag_hib32: present(src, "CRATONVM_DIAG_HIB32"),
            no_exact_refproc_survival: present(src, "CRATONVM_NO_EXACT_REFPROC_SURVIVAL"),
            no_referent_identity_screen: present(src, "CRATONVM_NO_REFERENT_IDENTITY_SCREEN"),
            no_oldgen_coalesce: present(src, "CRATONVM_NO_OLDGEN_COALESCE"),
            g1_dbg_headers: present(src, "CRATONVM_G1_DBG_HEADERS"),
            g1_dbg_diag: present(src, "CRATONVM_DBG_G1DIAG"),
            g1_dbg_accessor: present(src, "CRATONVM_DBG_G1ACCESSOR"),
            g1_dbg_pins: present(src, "CRATONVM_G1_DBG_PINS"),
            g1_humongous_marks: present(src, "CRATONVM_G1_HUMONGOUS_MARKS"),
            g1_ihop_counts_regions: present(src, "CRATONVM_G1_IHOP_COUNTS_REGIONS"),
            g1_verify_holders: present(src, "CRATONVM_G1_VERIFY_HOLDERS"),
            g1_dbg_reach: present(src, "CRATONVM_G1_DBG_REACH"),
            g1_dbg_rootcensus: present(src, "CRATONVM_G1_DBG_ROOTCENSUS"),
            g1_dbg_zero: present(src, "CRATONVM_G1_DBG_ZERO"),
            sp_stats: present(src, "CRATONVM_SP_STATS"),
            sp_trace: present(src, "CRATONVM_SP_TRACE"),
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// JIT flags
// ───────────────────────────────────────────────────────────────────────────

/// Flags read by `cratonvm-jit`. Only the entries that are *shared* with other
/// crates are here so far; the rest arrive with the `jit` migration.
#[derive(Debug, Clone, Default)]
pub struct JitFlags {
    /// `CRATONVM_DISABLE_JIT` — process-wide interpreter-only kill switch.
    ///
    /// This is typed because the launcher must be able to overlay `--nojit`
    /// before publishing the immutable configuration snapshot.
    pub disable_jit: bool,
    /// `CRATONVM_SHADOW_STACK` — use the JIT shadow stack for root discovery.
    /// Read from `gc`, `jit` and `vm`, which is why it is in the shared config
    /// rather than a crate-private cache.
    pub shadow_stack: bool,
    /// `CRATONVM_DBG_JIT_METHOD_STATS=1` — emit the tier/promotion summary
    /// exactly once during controlled VM shutdown. Kept in the shared snapshot
    /// so the supported `CRATONVM_DBG=jit-method-stats` spelling and the legacy
    /// spelling cannot diverge at the launcher.
    pub method_stats: bool,
}

impl JitFlags {
    fn from_source(src: &dyn FlagSource) -> Self {
        Self {
            disable_jit: parse::non_empty_non_zero(src, "CRATONVM_DISABLE_JIT"),
            shadow_stack: parse::present(src, "CRATONVM_SHADOW_STACK"),
            method_stats: parse::exactly_one(src, "CRATONVM_DBG_JIT_METHOD_STATS"),
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Class-loading flags
// ───────────────────────────────────────────────────────────────────────────

/// Flags read by `cratonvm-classloading`.
#[derive(Debug, Clone)]
pub struct LoaderFlags {
    /// `CRATONVM_LOADER_AWARE_RESOLUTION` — resolve implicit class constants
    /// through the defining class's own loader. **Default ON**; empty or `0`
    /// turns it off. [`parse::non_empty_non_zero_default_true`].
    pub loader_aware_resolution: bool,
    /// `CRATONVM_BOOT_MODULE_REGISTRY` — register JDK modules at boot.
    /// **Default ON**; `0` / `false` / `no` turn it off.
    /// [`parse::on_unless_off_word`].
    pub boot_module_registry: bool,
    /// `CRATONVM_CLASSPATH_JAR_UNNAMED_MODULE` — a class from a modular jar
    /// reached through the CLASS path belongs to the unnamed module, as on
    /// HotSpot, rather than to the module its `module-info.class` declares.
    /// **Default ON**; `0` / `false` restore the pre-2026-09-05 behaviour in
    /// which `--add-opens …=ALL-UNNAMED` could not reach such a class. See
    /// `ModuleRegistry::named_module_for_package`.
    /// [`parse::on_unless_zero_or_false`].
    pub classpath_jar_unnamed_module: bool,
    /// `CRATONVM_ALLOW_JSR_RET` — accept `jsr`/`ret` in the verifier.
    /// [`parse::non_empty_non_zero`].
    pub allow_jsr_ret: bool,
    /// `CRATONVM_HARDEN_MANIFEST_CLASSPATH` — drop manifest `Class-Path`
    /// entries that escape the JAR's directory. [`parse::non_empty_non_zero`].
    pub harden_manifest_classpath: bool,
    /// `CRATONVM_DISABLE_JAR_MMAP` — read JARs with `read()` instead of `mmap`.
    pub disable_jar_mmap: bool,
    /// `CRATONVM_TRUST_PEM` — path to an extra PEM trust bundle.
    /// [`parse::utf8`].
    pub trust_pem: Option<String>,
    /// `CRATONVM_TRACE_UNIMPLEMENTED`
    pub trace_unimplemented: bool,
    /// `CRATONVM_DBG_ACCESS` — [`parse::present_utf8`].
    pub dbg_access: bool,
    /// `CRATONVM_DBG_CLASSPATH`
    pub dbg_classpath: bool,
    /// `CRATONVM_DBG_DEFINE` — [`parse::present_utf8`].
    pub dbg_define: bool,
    /// `CRATONVM_DBG_DEFINE_CENSUS` — dump a per-class tally of every class
    /// DEFINITION at exit, hottest first. [`parse::present_utf8`].
    ///
    /// Exists because "what is still defining classes in steady state?" had no
    /// instrument at all. `runtime::diagnostics::classes_loaded` was declared,
    /// reset, formatted and unit-tested, and incremented by nothing — so it
    /// reported a confident zero, while `JitCache::invalidate_for_class`, which
    /// runs ONLY on a class definition, sat at ~1% of an H2 profile taken long
    /// past warm-up with nothing able to say what was calling it.
    pub dbg_define_census: bool,
    /// `CRATONVM_DBG_DUPCLASS` — [`parse::present_utf8`].
    pub dbg_dupclass: bool,
    /// `CRATONVM_DBG_DUPCLASS_BT`
    pub dbg_dupclass_bt: bool,
    /// `CRATONVM_DBG_DUPCLASS_FILTER` -- [`parse::utf8`]. Restricts the
    /// `CRATONVM_DBG_DUPCLASS` trace to class names containing this substring.
    pub dbg_dupclass_filter: Option<String>,
    /// `CRATONVM_DBG_TYPECHECK_FILTER` -- [`parse::utf8`]. Traces every compiled
    /// `checkcast`/`instanceof` whose TARGET class name contains this substring,
    /// naming the branch that decided it and the ids it compared.
    ///
    /// A compiled type check that disagrees with the interpreter has no other
    /// witness. The bytecode has already collapsed to a taken/not-taken branch
    /// by the time anything observable happens, so the only symptom is a wrong
    /// answer somewhere downstream — `Spr15042`-style bean errors, or a
    /// `super.` call that should never have been reached.
    /// `CRATONVM_JIT_DENY=<Class>.<method>` localises WHICH method miscompiles;
    /// this says WHY.
    pub dbg_typecheck_filter: Option<String>,
    /// `CRATONVM_DBG_FBCGLIB`
    pub dbg_fbcglib: bool,
    /// `CRATONVM_DBG_GETRESOURCES` — [`parse::non_empty_non_zero`].
    pub dbg_getresources: bool,
    /// `CRATONVM_DBG_LAYOUT`
    pub dbg_layout: bool,
    /// `CRATONVM_DBG_LOADCLASS`
    pub dbg_loadclass: bool,
    /// `CRATONVM_DBG_MODPROV` — [`parse::present_utf8`].
    pub dbg_modprov: bool,
    /// `CRATONVM_DBG_OBSREG`
    pub dbg_obsreg: bool,
    /// `CRATONVM_DBG_RESOURCE_TIMING`
    pub dbg_resource_timing: bool,
}

impl Default for LoaderFlags {
    fn default() -> Self {
        Self::from_source(&MapSource::empty())
    }
}

impl LoaderFlags {
    fn from_source(src: &dyn FlagSource) -> Self {
        use parse::*;
        Self {
            loader_aware_resolution: non_empty_non_zero_default_true(
                src,
                "CRATONVM_LOADER_AWARE_RESOLUTION",
            ),
            boot_module_registry: on_unless_off_word(src, "CRATONVM_BOOT_MODULE_REGISTRY"),
            classpath_jar_unnamed_module: on_unless_zero_or_false(
                src,
                "CRATONVM_CLASSPATH_JAR_UNNAMED_MODULE",
            ),
            allow_jsr_ret: non_empty_non_zero(src, "CRATONVM_ALLOW_JSR_RET"),
            harden_manifest_classpath: non_empty_non_zero(
                src,
                "CRATONVM_HARDEN_MANIFEST_CLASSPATH",
            ),
            disable_jar_mmap: present(src, "CRATONVM_DISABLE_JAR_MMAP"),
            trust_pem: utf8(src, "CRATONVM_TRUST_PEM"),
            trace_unimplemented: present(src, "CRATONVM_TRACE_UNIMPLEMENTED"),
            dbg_access: present_utf8(src, "CRATONVM_DBG_ACCESS"),
            dbg_classpath: present(src, "CRATONVM_DBG_CLASSPATH"),
            dbg_define: present_utf8(src, "CRATONVM_DBG_DEFINE"),
            dbg_define_census: present_utf8(src, "CRATONVM_DBG_DEFINE_CENSUS"),
            dbg_dupclass: present_utf8(src, "CRATONVM_DBG_DUPCLASS"),
            dbg_dupclass_bt: present(src, "CRATONVM_DBG_DUPCLASS_BT"),
            dbg_dupclass_filter: utf8(src, "CRATONVM_DBG_DUPCLASS_FILTER"),
            dbg_typecheck_filter: utf8(src, "CRATONVM_DBG_TYPECHECK_FILTER"),
            dbg_fbcglib: present(src, "CRATONVM_DBG_FBCGLIB"),
            dbg_getresources: non_empty_non_zero(src, "CRATONVM_DBG_GETRESOURCES"),
            dbg_layout: present(src, "CRATONVM_DBG_LAYOUT"),
            dbg_loadclass: present(src, "CRATONVM_DBG_LOADCLASS"),
            dbg_modprov: present_utf8(src, "CRATONVM_DBG_MODPROV"),
            dbg_obsreg: present(src, "CRATONVM_DBG_OBSREG"),
            dbg_resource_timing: present(src, "CRATONVM_DBG_RESOURCE_TIMING"),
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// I/O and networking flags
// ───────────────────────────────────────────────────────────────────────────

/// Flags read by `cratonvm-native-io`.
#[derive(Debug, Clone, Default)]
pub struct IoFlags {
    /// `CRATONVM_CONFINE_IO` — the confinement profile: enable CWD
    /// confinement, fail closed. [`parse::truthy_word`].
    pub confine_io: bool,
    /// `CRATONVM_UNTRUSTED_CODE` — the strict defence-in-depth profile: the
    /// same fail-closed CWD confinement as `confine_io` (it aborts too, it does
    /// not warn and continue), plus it implies `CRATONVM_REQUIRE_POLICY` and
    /// unconditionally denies host-native access (JNI library loads,
    /// `SymbolLookup`, FFM downcalls). [`parse::truthy_word`].
    pub untrusted_code: bool,
    /// `CRATONVM_BLOCK_PRIVATE_NETS` — deny outbound to loopback and RFC1918.
    /// [`parse::truthy_word`].
    pub block_private_nets: bool,
    /// `CRATONVM_RESOLVE_OUTBOUND_HOST` — resolve outbound hostnames and apply
    /// the per-IP policy to every address. [`parse::truthy_word`].
    pub resolve_outbound_host: bool,
    /// `CRATONVM_REAL_NET_SOCKETS` — use real OS sockets instead of the
    /// synthetic implementations.
    pub real_net_sockets: bool,
    /// `CRATONVM_SYNTHETIC_NET_SOCKETS` explicitly selects the legacy surface.
    pub synthetic_net_sockets_forced: bool,
    /// `CRATONVM_SYNTHETIC_FILEWRITER=1` — opt back into the synthetic (known
    /// lossy) `FileWriter`. Consumers want `!synthetic_filewriter_forced`.
    /// [`parse::exactly_one`].
    pub synthetic_filewriter_forced: bool,
    /// `CRATONVM_SYNTHETIC_RAF=1` — opt back into the synthetic
    /// `RandomAccessFile`. Consumers want `!synthetic_raf_forced`.
    /// [`parse::exactly_one`].
    pub synthetic_raf_forced: bool,
    /// `CRATONVM_SYNTHETIC_NETTY_TCNATIVE=1` — opt back into the
    /// `io/netty/internal/tcnative` stub surface instead of running Netty's
    /// real `netty_tcnative` library.
    ///
    /// Default (unset) loads the real library: its `JNI_OnLoad` runs and the
    /// stubs stand down, which is what makes `OpenSsl.isAvailable()` true and
    /// lets netty's suites generate their `SslProvider.OPENSSL` parameters at
    /// all. Set this only on a host where that library genuinely misbehaves;
    /// with it set, `OpenSsl.isAvailable()` is false exactly as before.
    /// Consumers want `!synthetic_netty_tcnative_forced`.
    /// [`parse::exactly_one`].
    pub synthetic_netty_tcnative_forced: bool,
    /// `CRATONVM_SOCKET_CAPTURE` — non-empty path prefix for socket capture
    /// files. [`parse::non_empty_string`].
    pub socket_capture_prefix: Option<String>,
    /// `CRATONVM_SELECT_MAX_BLOCK_MS` — cap on an indefinite
    /// `Selector.select()`, in ms; the caller supplies the default.
    /// [`parse::i32_positive`].
    pub select_max_block_ms: Option<i32>,
    /// `CRATONVM_NO_SELECTOR_CONNECT_PROBE` — disable the Windows selector's
    /// connect-completion probe. [`parse::non_empty_non_zero_non_false`].
    pub no_selector_connect_probe: bool,
    /// `CRATONVM_ZIP_MAX_ENTRY_BYTES` — per-entry inflate cap; the caller
    /// supplies the default. [`parse::u64_positive`].
    pub zip_max_entry_bytes: Option<u64>,
    /// `CRATONVM_DIAG_JAR_LIST` — path to a classpath file for the jar-open
    /// timing probe. [`parse::utf8`].
    pub diag_jar_list: Option<String>,
    /// `CRATONVM_DBG_SELECTOR` — [`parse::non_empty_non_zero_non_false`].
    pub dbg_selector: bool,
    /// `CRATONVM_SUREFIRE_IPC_DBG` — [`parse::non_empty_non_zero_non_false`].
    pub surefire_ipc_dbg: bool,
    /// `CRATONVM_DBG_AIO`
    pub dbg_aio: bool,
    /// `CRATONVM_DBG_JAR`
    pub dbg_jar: bool,
    /// `CRATONVM_DBG_JETTY` — [`parse::present_utf8`].
    pub dbg_jetty: bool,
    /// `CRATONVM_DBG_NET`
    pub dbg_net: bool,
    /// `CRATONVM_DBG_PB`
    pub dbg_pb: bool,
    /// `CRATONVM_DBG_SC_CLOSE`
    pub dbg_sc_close: bool,
    /// `CRATONVM_DBG_SC_READ`
    pub dbg_sc_read: bool,
    /// `CRATONVM_DBG_SC_WRITE`
    pub dbg_sc_write: bool,
    /// `CRATONVM_DBG_EINTR_INJECT` — synthesise an `EINTR` on every *n*-th
    /// socket operation that passes through `cratonvm_native_io::eintr`.
    /// Test-only; the defect it reproduces is documented on that module.
    /// [`parse::u64_positive`].
    pub dbg_eintr_inject: Option<u64>,
    /// `CRATONVM_DBG_EINTR_NO_RETRY` — do *not* absorb `EINTR` in
    /// `cratonvm_native_io::eintr`, i.e. behave as the code did before that
    /// module existed. Test-only, and the other half of the A/B above.
    /// [`parse::non_empty_non_zero_non_false`].
    pub dbg_eintr_no_retry: bool,
}

impl IoFlags {
    fn from_source(src: &dyn FlagSource) -> Self {
        use parse::*;
        Self {
            confine_io: truthy_word(src, "CRATONVM_CONFINE_IO"),
            untrusted_code: truthy_word(src, "CRATONVM_UNTRUSTED_CODE"),
            block_private_nets: truthy_word(src, "CRATONVM_BLOCK_PRIVATE_NETS"),
            resolve_outbound_host: truthy_word(src, "CRATONVM_RESOLVE_OUTBOUND_HOST"),
            real_net_sockets: !present(src, "CRATONVM_SYNTHETIC_NET_SOCKETS")
                || present(src, "CRATONVM_REAL_NET_SOCKETS"),
            synthetic_net_sockets_forced: present(src, "CRATONVM_SYNTHETIC_NET_SOCKETS"),
            synthetic_filewriter_forced: exactly_one(src, "CRATONVM_SYNTHETIC_FILEWRITER"),
            synthetic_raf_forced: exactly_one(src, "CRATONVM_SYNTHETIC_RAF"),
            synthetic_netty_tcnative_forced: exactly_one(src, "CRATONVM_SYNTHETIC_NETTY_TCNATIVE"),
            socket_capture_prefix: non_empty_string(src, "CRATONVM_SOCKET_CAPTURE"),
            select_max_block_ms: i32_positive(src, "CRATONVM_SELECT_MAX_BLOCK_MS"),
            no_selector_connect_probe: non_empty_non_zero_non_false(
                src,
                "CRATONVM_NO_SELECTOR_CONNECT_PROBE",
            ),
            zip_max_entry_bytes: u64_positive(src, "CRATONVM_ZIP_MAX_ENTRY_BYTES"),
            diag_jar_list: utf8(src, "CRATONVM_DIAG_JAR_LIST"),
            dbg_selector: non_empty_non_zero_non_false(src, "CRATONVM_DBG_SELECTOR"),
            surefire_ipc_dbg: non_empty_non_zero_non_false(src, "CRATONVM_SUREFIRE_IPC_DBG"),
            dbg_aio: present(src, "CRATONVM_DBG_AIO"),
            dbg_jar: present(src, "CRATONVM_DBG_JAR"),
            dbg_jetty: present_utf8(src, "CRATONVM_DBG_JETTY"),
            dbg_net: present(src, "CRATONVM_DBG_NET"),
            dbg_pb: present(src, "CRATONVM_DBG_PB"),
            dbg_sc_close: present(src, "CRATONVM_DBG_SC_CLOSE"),
            dbg_sc_read: present(src, "CRATONVM_DBG_SC_READ"),
            dbg_sc_write: present(src, "CRATONVM_DBG_SC_WRITE"),
            dbg_eintr_inject: u64_positive(src, "CRATONVM_DBG_EINTR_INJECT"),
            dbg_eintr_no_retry: non_empty_non_zero_non_false(src, "CRATONVM_DBG_EINTR_NO_RETRY"),
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// native-builtins flags
// ───────────────────────────────────────────────────────────────────────────

/// Flags read by `cratonvm-native-builtins`.
///
/// That crate reads 156 flag names directly and 127 of them are read *only*
/// there, so they get their own struct rather than being folded into the
/// per-subsystem ones. Unless a field says otherwise it was built with
/// [`parse::present`], i.e. it is `true` whenever the variable is set to
/// anything at all — including `0` and the empty string.
#[derive(Debug, Clone, Default)]
pub struct NativeFlags {
    /// `CRATONVM_ANN_TRACE` — [`parse::present_utf8`].
    pub ann_trace: bool,

    /// `CRATONVM_ASYNC_HANDOFF_SLEEP_FLOOR_MS` — [`parse::i64_opt_untrimmed`].
    ///
    /// handoff sleep floor, milliseconds
    pub async_handoff_sleep_floor_ms: Option<i64>,

    /// `CRATONVM_ASYNC_SUBMIT_GRACE_MS` — [`parse::u64_opt_untrimmed`].
    ///
    /// submit grace period, milliseconds; `None` falls back to 20
    pub async_submit_grace_ms: Option<u64>,

    /// `CRATONVM_ASYNC_WORKER_SLEEP_FLOOR_MS` — [`parse::i64_opt_untrimmed`].
    ///
    /// worker sleep floor, milliseconds
    pub async_worker_sleep_floor_ms: Option<i64>,

    /// `CRATONVM_AWAIT_NO_SHORTCIRCUIT`
    pub await_no_shortcircuit: bool,

    /// `CRATONVM_BD_DEBUG`
    pub bd_debug: bool,

    /// `CRATONVM_CANON_OPENFILE`
    pub canon_openfile: bool,

    /// `CRATONVM_CL_BOOTSTRAP_SCOPED` — [`parse::on_unless_zero`].
    pub cl_bootstrap_scoped: bool,

    /// `CRATONVM_DBG`
    pub dbg: bool,

    /// `CRATONVM_DBG_ANNPROXY_WRAP`
    pub dbg_annproxy_wrap: bool,

    /// `CRATONVM_DBG_AQS_TRACE`
    pub dbg_aqs_trace: bool,

    /// `CRATONVM_DBG_ARRAYCOPY` — [`parse::exactly_one`].
    pub dbg_arraycopy: bool,

    /// `CRATONVM_DBG_ASSERTJ_ARR`
    pub dbg_assertj_arr: bool,

    /// `CRATONVM_DBG_ATOMIC_UPDATER`
    pub dbg_atomic_updater: bool,

    /// `CRATONVM_DBG_BB` — [`parse::present_utf8`].
    pub dbg_bb: bool,

    /// `CRATONVM_DBG_CALLER` — [`parse::present_utf8`].
    pub dbg_caller: bool,

    /// `CRATONVM_DBG_CAPVAL`
    pub dbg_capval: bool,

    /// `CRATONVM_DBG_CATALINA` — [`parse::present_utf8`].
    pub dbg_catalina: bool,

    /// `CRATONVM_DBG_CAUSE`
    pub dbg_cause: bool,

    /// `CRATONVM_DBG_CCECACHE`
    pub dbg_ccecache: bool,

    /// `CRATONVM_DBG_CCE_BT`
    pub dbg_cce_bt: bool,

    /// `CRATONVM_DBG_CLONE`
    pub dbg_clone: bool,

    /// `CRATONVM_DBG_COERCE`
    pub dbg_coerce: bool,

    /// `CRATONVM_DBG_COMPONENT_TYPE` — [`parse::present_utf8`].
    pub dbg_component_type: bool,

    /// `CRATONVM_DBG_DEFLATE`
    pub dbg_deflate: bool,

    /// `CRATONVM_DBG_DOPRIV`
    pub dbg_dopriv: bool,

    /// `CRATONVM_DBG_EQE`
    pub dbg_eqe: bool,

    /// `CRATONVM_DBG_EXEC`
    pub dbg_exec: bool,

    /// `CRATONVM_DBG_EXIT` — [`parse::exactly_one`].
    pub dbg_exit: bool,

    /// `CRATONVM_DBG_FBREF`
    pub dbg_fbref: bool,

    /// `CRATONVM_DBG_FIELD_GET` — [`parse::present_utf8`].
    pub dbg_field_get: bool,

    /// `CRATONVM_DBG_FSP` — [`parse::present_utf8`].
    pub dbg_fsp: bool,

    /// `CRATONVM_DBG_GOCBF`
    pub dbg_gocbf: bool,

    /// `CRATONVM_DBG_H2TRACE`
    pub dbg_h2trace: bool,

    /// `CRATONVM_DBG_HTTPSRV`
    pub dbg_httpsrv: bool,

    /// `CRATONVM_DBG_INVOKE_COERCE` — [`parse::exactly_one`].
    pub dbg_invoke_coerce: bool,

    /// `CRATONVM_DBG_ISINSTANCE`
    pub dbg_isinstance: bool,

    /// `CRATONVM_DBG_JLM` — [`parse::present_utf8`].
    pub dbg_jlm: bool,

    /// `CRATONVM_DBG_LAMBDA_GENERIC` — [`parse::present_utf8`].
    pub dbg_lambda_generic: bool,

    /// `CRATONVM_DBG_LINKER`
    pub dbg_linker: bool,

    /// `CRATONVM_DBG_LOADER_TRACE`
    pub dbg_loader_trace: bool,

    /// `CRATONVM_DBG_LOGPROV`
    pub dbg_logprov: bool,

    /// `CRATONVM_DBG_LOOKUP` — [`parse::present_utf8`].
    pub dbg_lookup: bool,

    /// `CRATONVM_DBG_MCL`
    pub dbg_mcl: bool,

    /// `CRATONVM_DBG_METHOD_INVOKE_BOX`
    pub dbg_method_invoke_box: bool,

    /// `CRATONVM_DBG_MH_DISPATCH`
    pub dbg_mh_dispatch: bool,

    /// `CRATONVM_DBG_MINVOKE`
    pub dbg_minvoke: bool,

    /// `CRATONVM_DBG_MSC`
    pub dbg_msc: bool,

    /// `CRATONVM_DBG_NETTY_QUEUE`
    pub dbg_netty_queue: bool,

    /// `CRATONVM_DBG_NEXTINT` — [`parse::present_utf8`].
    pub dbg_nextint: bool,

    /// `CRATONVM_DBG_NIO_BIND`
    pub dbg_nio_bind: bool,

    /// `CRATONVM_DBG_NULL_NATIVE`
    pub dbg_null_native: bool,

    /// `CRATONVM_DBG_OBJECTS`
    pub dbg_objects: bool,

    /// `CRATONVM_DBG_OBJ_EQUALS`
    pub dbg_obj_equals: bool,

    /// `CRATONVM_DBG_PBE`
    pub dbg_pbe: bool,

    /// `CRATONVM_DBG_PICOCLI_STYLE` — [`parse::present_utf8`].
    pub dbg_picocli_style: bool,

    /// `CRATONVM_DBG_PROXY` — [`parse::present_utf8`].
    pub dbg_proxy: bool,

    /// `CRATONVM_DBG_RAF_GETFD`
    pub dbg_raf_getfd: bool,

    /// `CRATONVM_DBG_RAF_INIT`
    pub dbg_raf_init: bool,

    /// `CRATONVM_DBG_RE5`
    pub dbg_re5: bool,

    /// `CRATONVM_DBG_REFERSTO`
    pub dbg_refersto: bool,

    /// `CRATONVM_DBG_REFLECTION_FACTORY`
    pub dbg_reflection_factory: bool,

    /// `CRATONVM_DBG_REPLOVR`
    pub dbg_replovr: bool,

    /// `CRATONVM_DBG_RESOLVE_SHIM`
    pub dbg_resolve_shim: bool,

    /// `CRATONVM_DBG_SBLOAD`
    pub dbg_sbload: bool,

    /// `CRATONVM_DBG_SEL`
    pub dbg_sel: bool,

    /// `CRATONVM_DBG_SLEEP_TRACE`
    pub dbg_sleep_trace: bool,

    /// `CRATONVM_DBG_SOCK`
    pub dbg_sock: bool,

    /// `CRATONVM_DBG_SOCK_BYTES`
    pub dbg_sock_bytes: bool,

    /// `CRATONVM_DBG_STREAMSUPP` — [`parse::present_utf8`].
    pub dbg_streamsupp: bool,

    /// `CRATONVM_DBG_STTRACE`
    pub dbg_sttrace: bool,

    /// `CRATONVM_DBG_TLS_AUTH`
    ///
    /// This crate probes the flag with BOTH `var_os(..).is_some()` and
    /// `var(..).is_ok()`. The two disagree on a non-UTF-8 value, so both are
    /// kept rather than silently unified.
    pub dbg_tls_auth: bool,

    /// `CRATONVM_DBG_TLS_AUTH` — [`parse::present_utf8`].
    pub dbg_tls_auth_ok: bool,

    /// `CRATONVM_DBG_TLS_HS`
    ///
    /// This crate probes the flag with BOTH `var_os(..).is_some()` and
    /// `var(..).is_ok()`. The two disagree on a non-UTF-8 value, so both are
    /// kept rather than silently unified.
    pub dbg_tls_hs: bool,

    /// `CRATONVM_DBG_TLS_HS` — [`parse::present_utf8`].
    pub dbg_tls_hs_ok: bool,

    /// `CRATONVM_DBG_TLS_PLS`
    pub dbg_tls_pls: bool,

    /// `CRATONVM_DBG_TLS_SOCK`
    pub dbg_tls_sock: bool,

    /// `CRATONVM_DBG_TLS_SRV`
    pub dbg_tls_srv: bool,

    /// `CRATONVM_DBG_TOARRAY`
    ///
    /// This crate probes the flag with BOTH `var_os(..).is_some()` and
    /// `var(..).is_ok()`. The two disagree on a non-UTF-8 value, so both are
    /// kept rather than silently unified.
    pub dbg_toarray: bool,

    /// `CRATONVM_DBG_TOARRAY` — [`parse::present_utf8`].
    pub dbg_toarray_ok: bool,

    /// `CRATONVM_DBG_TOHEX`
    pub dbg_tohex: bool,

    /// `CRATONVM_DBG_UCLREG`
    pub dbg_uclreg: bool,

    /// `CRATONVM_DBG_UCLRES`
    pub dbg_uclres: bool,

    /// `CRATONVM_DBG_URLCL`
    pub dbg_urlcl: bool,

    /// `CRATONVM_DBG_UTE`
    pub dbg_ute: bool,

    /// `CRATONVM_DBG_VDISP`
    pub dbg_vdisp: bool,

    /// `CRATONVM_DBG_VISITFILE`
    pub dbg_visitfile: bool,

    /// `CRATONVM_DBG_WATCH_CAUSE_SELF` — [`parse::utf8`].
    ///
    /// class name to watch for self-causing Throwables
    pub dbg_watch_cause_self: Option<String>,

    /// `CRATONVM_DBG_WF`
    pub dbg_wf: bool,

    /// `CRATONVM_DBG_XNIO_TCP`
    pub dbg_xnio_tcp: bool,

    /// `CRATONVM_DEBUG_SFI`
    pub debug_sfi: bool,

    /// `CRATONVM_DEBUG_STACKWALK`
    pub debug_stackwalk: bool,

    /// `CRATONVM_DIAG_JBOSS_SERVICES` — [`parse::utf8`].
    ///
    /// raw value: this flag falls back to `CRATONVM_DIAG_SERVICELOADER` only
    /// when it is *unset*, which needs the raw Option
    pub diag_jboss_services: Option<String>,

    /// `CRATONVM_DIAG_JCA`
    pub diag_jca: bool,

    /// `CRATONVM_DIAG_METHOD_INVOKE_NULL`
    pub diag_method_invoke_null: bool,

    /// `CRATONVM_DIAG_PROPERTIES` — [`parse::one_true_yes_exact`].
    pub diag_properties: bool,

    /// `CRATONVM_DIAG_SERVICELOADER` — [`parse::one_true_yes_exact`].
    pub diag_serviceloader: bool,

    /// `CRATONVM_ENABLE_ASSERTIONS`
    pub enable_assertions: bool,

    /// `CRATONVM_EQE_SYNC_EXECUTE`
    pub eqe_sync_execute: bool,

    /// `CRATONVM_FORNAME_TRACE`
    pub forname_trace: bool,

    /// `CRATONVM_HTTP_MAX_BODY` — [`parse::usize_positive`].
    ///
    /// HTTP response body cap, bytes
    pub http_max_body: Option<usize>,

    /// `CRATONVM_IAE_TRACE`
    ///
    /// This crate probes the flag with BOTH `var_os(..).is_some()` and
    /// `var(..).is_ok()`. The two disagree on a non-UTF-8 value, so both are
    /// kept rather than silently unified.
    pub iae_trace: bool,

    /// `CRATONVM_IAE_TRACE` — [`parse::present_utf8`].
    pub iae_trace_ok: bool,

    /// `CRATONVM_IAE_TRACE2` — [`parse::present_utf8`].
    pub iae_trace2: bool,

    /// `CRATONVM_INHERIT_THREAD_CCL` — [`parse::on_unless_zero_or_false`].
    pub inherit_thread_ccl: bool,

    /// `CRATONVM_INHERIT_TL_WORKAROUND` — [`parse::on_unless_zero_or_false`].
    pub inherit_tl_workaround: bool,

    /// `CRATONVM_JBOSS_BOOT_LOG_FILE` — [`parse::utf8`].
    ///
    /// path for the JBoss boot log
    pub jboss_boot_log_file: Option<String>,

    /// `CRATONVM_JBOSS_BRUTE_FORCE_JARS` — [`parse::exactly_one`].
    pub jboss_brute_force_jars: bool,

    /// `CRATONVM_JBOSS_LOGGER_BASE_EMIT` — [`parse::exactly_one`].
    pub jboss_logger_base_emit: bool,

    /// `CRATONVM_LOADER_UNLOAD` — [`parse::on_unless_zero`].
    pub loader_unload: bool,

    /// `CRATONVM_MAX_INFLATED_BYTES` — [`parse::utf8`].
    ///
    /// raw value: the gzip cap distinguishes unparseable (default) from an
    /// explicit `0` (disabled), so the bespoke parse stays at the call site
    pub max_inflated_bytes: Option<String>,

    /// `CRATONVM_MSC_REAL_START` — [`parse::on_unless_off_word_cased`].
    pub msc_real_start: bool,

    /// `CRATONVM_NATIVE_EC_MULTIPLY`
    pub native_ec_multiply: bool,

    /// `CRATONVM_NATIVE_MATCHER_FIND` — [`parse::on_unless_zero_or_false`].
    pub native_matcher_find: bool,

    /// `CRATONVM_NATIVE_PBE_KEYFACTORY` — [`parse::exactly_one`].
    pub native_pbe_keyfactory: bool,

    /// `CRATONVM_NATIVE_STRING_REGEX` — [`parse::on_unless_zero_or_false`].
    pub native_string_regex: bool,

    /// `CRATONVM_NETTY_QUEUE_BRIDGE` — [`parse::on_unless_zero_or_false`].
    pub netty_queue_bridge: bool,

    /// `CRATONVM_REAL_AGROAL`
    pub real_agroal: bool,

    /// `CRATONVM_REAL_ANNOTATIONS` — [`parse::on_unless_off_word_cased`].
    pub real_annotations: bool,

    /// `CRATONVM_REAL_AQS`
    pub real_aqs: bool,
    /// Real JDK ForkJoinPool is default; the synthetic implementation is opt-in.
    pub real_forkjoinpool: bool,

    /// `CRATONVM_REAL_JCA`
    pub real_jca: bool,

    /// `CRATONVM_REAL_PROXY` — [`parse::truthy_word_default_true`].
    pub real_proxy: bool,

    /// `CRATONVM_REAL_PROXY_STRICT` — [`parse::affirmative_word`].
    pub real_proxy_strict: bool,

    /// `CRATONVM_REAL_PROXY_SUPER` — [`parse::truthy_word_default_true`].
    pub real_proxy_super: bool,

    /// `CRATONVM_REAL_PROXY_SUPER`
    ///
    /// Bare presence of the same variable: one test-only site probes
    /// `var_os(..).is_none()` rather than the value.
    pub real_proxy_super_set: bool,

    /// `CRATONVM_REAL_QUARKUS_START`
    pub real_quarkus_start: bool,

    /// `CRATONVM_REAL_STAX_FACTORY` — [`parse::on_unless_zero`].
    pub real_stax_factory: bool,

    /// `CRATONVM_REAL_VERTX`
    pub real_vertx: bool,

    /// `CRATONVM_REQUIRE_POLICY`
    pub require_policy: bool,

    /// `CRATONVM_S111_DBG` — [`parse::present_utf8`].
    pub s111_dbg: bool,

    /// `CRATONVM_SFI_NULL_TRACE`
    pub sfi_null_trace: bool,

    /// `CRATONVM_SOFT_EXIT` — [`parse::exactly_one`].
    pub soft_exit: bool,

    /// `CRATONVM_SPRING_DBG`
    pub spring_dbg: bool,

    /// `CRATONVM_SYNTHETIC_AGROAL`
    pub synthetic_agroal: bool,

    /// `CRATONVM_SYNTHETIC_ANNOTATIONS`
    pub synthetic_annotations: bool,

    /// `CRATONVM_SYNTHETIC_AQS`
    pub synthetic_aqs: bool,

    /// `CRATONVM_SYNTHETIC_DSA`
    pub synthetic_dsa: bool,

    /// `CRATONVM_SYNTHETIC_EC`
    pub synthetic_ec: bool,

    /// `CRATONVM_SYNTHETIC_EQE` — [`parse::present_utf8`].
    pub synthetic_eqe: bool,
    /// `CRATONVM_SYNTHETIC_FORKJOINPOOL`
    pub synthetic_forkjoinpool: bool,

    /// `CRATONVM_SYNTHETIC_MEMORYUSAGE_TOSTRING=1|true|yes` — answer
    /// `java.lang.management.MemoryUsage.toString()` from the shim instead of
    /// the class's own bytecode. On a real-JDK run the real bytecode is the
    /// default. [`parse::one_true_yes_exact`].
    pub synthetic_memoryusage_tostring: bool,

    /// `CRATONVM_SYNTHETIC_MXBEAN_MAPPING=1|true|yes` — opt back into the
    /// synthetic JMX MXBean type mapping that types every unrecognised Java
    /// type as `SimpleType.STRING` and converts nothing. The real JDK
    /// machinery is the default; consumers want
    /// `!synthetic_mxbean_mapping`. [`parse::one_true_yes_exact`].
    pub synthetic_mxbean_mapping: bool,

    /// `CRATONVM_SYNTHETIC_PQC`
    pub synthetic_pqc: bool,

    /// `CRATONVM_SYNTHETIC_QUARKUS_START`
    pub synthetic_quarkus_start: bool,

    /// `CRATONVM_SYNTHETIC_RSA`
    pub synthetic_rsa: bool,

    /// `CRATONVM_SYNTHETIC_VERTX`
    pub synthetic_vertx: bool,

    /// `CRATONVM_TLS_OPENSSL_CLIENT` — back the default `SSLSocket` client
    /// path with a raw `openssl::SslConnector` instead of
    /// `native_tls::TlsConnector`. **Default ON** on Unix (no effect
    /// elsewhere — `openssl` is a Unix-only dependency); `0` turns it off.
    /// [`parse::on_unless_zero`].
    ///
    /// The kill switch exists so the two can be A/B'd in ONE binary. What
    /// only the raw connector can do: hand out the peer's FULL certificate
    /// chain (`SSL_get_peer_cert_chain`, which native-tls 0.2 does not
    /// expose — it has `peer_certificate()` and nothing else), and set the
    /// certificate security level. Both are load-bearing: an application
    /// TrustManager cannot build a path from a one-element chain, and
    /// OpenSSL's default security level of 2 is stricter than the JDK's own
    /// 1024-bit floor.
    pub tls_openssl_client: bool,
    /// `CRATONVM_TRACE_ARRAYS_HASHCODE`
    pub trace_arrays_hashcode: bool,

    /// `CRATONVM_TRACE_CLASSVALUE`
    pub trace_classvalue: bool,

    /// `CRATONVM_TRACE_PTI_ARGS`
    pub trace_pti_args: bool,

    /// `CRATONVM_UEH_DEBUG`
    pub ueh_debug: bool,

    /// `CRATONVM_URI_STRICT_CHARS` — [`parse::on_unless_zero`].
    pub uri_strict_chars: bool,

    /// `CRATONVM_USE_WILDFLY_REFLECT_SHIM` — [`parse::exactly_one`].
    pub use_wildfly_reflect_shim: bool,

    /// `CRATONVM_USE_WILDFLY_SYNTH_BYTECODE` — [`parse::exactly_one`].
    pub use_wildfly_synth_bytecode: bool,
}

impl NativeFlags {
    fn from_source(src: &dyn FlagSource) -> Self {
        use parse::*;
        Self {
            ann_trace: present_utf8(src, "CRATONVM_ANN_TRACE"),
            async_handoff_sleep_floor_ms: i64_opt_untrimmed(
                src,
                "CRATONVM_ASYNC_HANDOFF_SLEEP_FLOOR_MS",
            ),
            async_submit_grace_ms: u64_opt_untrimmed(src, "CRATONVM_ASYNC_SUBMIT_GRACE_MS"),
            async_worker_sleep_floor_ms: i64_opt_untrimmed(
                src,
                "CRATONVM_ASYNC_WORKER_SLEEP_FLOOR_MS",
            ),
            await_no_shortcircuit: present(src, "CRATONVM_AWAIT_NO_SHORTCIRCUIT"),
            bd_debug: present(src, "CRATONVM_BD_DEBUG"),
            canon_openfile: present(src, "CRATONVM_CANON_OPENFILE"),
            cl_bootstrap_scoped: on_unless_zero(src, "CRATONVM_CL_BOOTSTRAP_SCOPED"),
            // Was the bare `CRATONVM_DBG`. That name is now the debug *group*
            // variable (`CRATONVM_DBG=topic,topic`), so this one call site —
            // `quarkus_staticinit.rs` — gets an ordinary topic of its own:
            // `CRATONVM_DBG=quarkus-staticinit`.
            dbg: present(src, "CRATONVM_DBG_QUARKUS_STATICINIT"),
            dbg_annproxy_wrap: present(src, "CRATONVM_DBG_ANNPROXY_WRAP"),
            dbg_aqs_trace: present(src, "CRATONVM_DBG_AQS_TRACE"),
            dbg_arraycopy: exactly_one(src, "CRATONVM_DBG_ARRAYCOPY"),
            dbg_assertj_arr: present(src, "CRATONVM_DBG_ASSERTJ_ARR"),
            dbg_atomic_updater: present(src, "CRATONVM_DBG_ATOMIC_UPDATER"),
            dbg_bb: present_utf8(src, "CRATONVM_DBG_BB"),
            dbg_caller: present_utf8(src, "CRATONVM_DBG_CALLER"),
            dbg_capval: present(src, "CRATONVM_DBG_CAPVAL"),
            dbg_catalina: present_utf8(src, "CRATONVM_DBG_CATALINA"),
            dbg_cause: present(src, "CRATONVM_DBG_CAUSE"),
            dbg_ccecache: present(src, "CRATONVM_DBG_CCECACHE"),
            dbg_cce_bt: present(src, "CRATONVM_DBG_CCE_BT"),
            dbg_clone: present(src, "CRATONVM_DBG_CLONE"),
            dbg_coerce: present(src, "CRATONVM_DBG_COERCE"),
            dbg_component_type: present_utf8(src, "CRATONVM_DBG_COMPONENT_TYPE"),
            dbg_deflate: present(src, "CRATONVM_DBG_DEFLATE"),
            dbg_dopriv: present(src, "CRATONVM_DBG_DOPRIV"),
            dbg_eqe: present(src, "CRATONVM_DBG_EQE"),
            dbg_exec: present(src, "CRATONVM_DBG_EXEC"),
            dbg_exit: exactly_one(src, "CRATONVM_DBG_EXIT"),
            dbg_fbref: present(src, "CRATONVM_DBG_FBREF"),
            dbg_field_get: present_utf8(src, "CRATONVM_DBG_FIELD_GET"),
            dbg_fsp: present_utf8(src, "CRATONVM_DBG_FSP"),
            dbg_gocbf: present(src, "CRATONVM_DBG_GOCBF"),
            dbg_h2trace: present(src, "CRATONVM_DBG_H2TRACE"),
            dbg_httpsrv: present(src, "CRATONVM_DBG_HTTPSRV"),
            dbg_invoke_coerce: exactly_one(src, "CRATONVM_DBG_INVOKE_COERCE"),
            dbg_isinstance: present(src, "CRATONVM_DBG_ISINSTANCE"),
            dbg_jlm: present_utf8(src, "CRATONVM_DBG_JLM"),
            dbg_lambda_generic: present_utf8(src, "CRATONVM_DBG_LAMBDA_GENERIC"),
            dbg_linker: present(src, "CRATONVM_DBG_LINKER"),
            dbg_loader_trace: present(src, "CRATONVM_DBG_LOADER_TRACE"),
            dbg_logprov: present(src, "CRATONVM_DBG_LOGPROV"),
            dbg_lookup: present_utf8(src, "CRATONVM_DBG_LOOKUP"),
            dbg_mcl: present(src, "CRATONVM_DBG_MCL"),
            dbg_method_invoke_box: present(src, "CRATONVM_DBG_METHOD_INVOKE_BOX"),
            dbg_mh_dispatch: present(src, "CRATONVM_DBG_MH_DISPATCH"),
            dbg_minvoke: present(src, "CRATONVM_DBG_MINVOKE"),
            dbg_msc: present(src, "CRATONVM_DBG_MSC"),
            dbg_netty_queue: present(src, "CRATONVM_DBG_NETTY_QUEUE"),
            dbg_nextint: present_utf8(src, "CRATONVM_DBG_NEXTINT"),
            dbg_nio_bind: present(src, "CRATONVM_DBG_NIO_BIND"),
            dbg_null_native: present(src, "CRATONVM_DBG_NULL_NATIVE"),
            dbg_objects: present(src, "CRATONVM_DBG_OBJECTS"),
            dbg_obj_equals: present(src, "CRATONVM_DBG_OBJ_EQUALS"),
            dbg_pbe: present(src, "CRATONVM_DBG_PBE"),
            dbg_picocli_style: present_utf8(src, "CRATONVM_DBG_PICOCLI_STYLE"),
            dbg_proxy: present_utf8(src, "CRATONVM_DBG_PROXY"),
            dbg_raf_getfd: present(src, "CRATONVM_DBG_RAF_GETFD"),
            dbg_raf_init: present(src, "CRATONVM_DBG_RAF_INIT"),
            dbg_re5: present(src, "CRATONVM_DBG_RE5"),
            dbg_refersto: present(src, "CRATONVM_DBG_REFERSTO"),
            dbg_reflection_factory: present(src, "CRATONVM_DBG_REFLECTION_FACTORY"),
            dbg_replovr: present(src, "CRATONVM_DBG_REPLOVR"),
            dbg_resolve_shim: present(src, "CRATONVM_DBG_RESOLVE_SHIM"),
            dbg_sbload: present(src, "CRATONVM_DBG_SBLOAD"),
            dbg_sel: present(src, "CRATONVM_DBG_SEL"),
            dbg_sleep_trace: present(src, "CRATONVM_DBG_SLEEP_TRACE"),
            dbg_sock: present(src, "CRATONVM_DBG_SOCK"),
            dbg_sock_bytes: present(src, "CRATONVM_DBG_SOCK_BYTES"),
            dbg_streamsupp: present_utf8(src, "CRATONVM_DBG_STREAMSUPP"),
            dbg_sttrace: present(src, "CRATONVM_DBG_STTRACE"),
            dbg_tls_auth: present(src, "CRATONVM_DBG_TLS_AUTH"),
            dbg_tls_auth_ok: present_utf8(src, "CRATONVM_DBG_TLS_AUTH"),
            dbg_tls_hs: present(src, "CRATONVM_DBG_TLS_HS"),
            dbg_tls_hs_ok: present_utf8(src, "CRATONVM_DBG_TLS_HS"),
            dbg_tls_pls: present(src, "CRATONVM_DBG_TLS_PLS"),
            dbg_tls_sock: present(src, "CRATONVM_DBG_TLS_SOCK"),
            dbg_tls_srv: present(src, "CRATONVM_DBG_TLS_SRV"),
            dbg_toarray: present(src, "CRATONVM_DBG_TOARRAY"),
            dbg_toarray_ok: present_utf8(src, "CRATONVM_DBG_TOARRAY"),
            dbg_tohex: present(src, "CRATONVM_DBG_TOHEX"),
            dbg_uclreg: present(src, "CRATONVM_DBG_UCLREG"),
            dbg_uclres: present(src, "CRATONVM_DBG_UCLRES"),
            dbg_urlcl: present(src, "CRATONVM_DBG_URLCL"),
            dbg_ute: present(src, "CRATONVM_DBG_UTE"),
            dbg_vdisp: present(src, "CRATONVM_DBG_VDISP"),
            dbg_visitfile: present(src, "CRATONVM_DBG_VISITFILE"),
            dbg_watch_cause_self: utf8(src, "CRATONVM_DBG_WATCH_CAUSE_SELF"),
            dbg_wf: present(src, "CRATONVM_DBG_WF"),
            dbg_xnio_tcp: present(src, "CRATONVM_DBG_XNIO_TCP"),
            debug_sfi: present(src, "CRATONVM_DEBUG_SFI"),
            debug_stackwalk: present(src, "CRATONVM_DEBUG_STACKWALK"),
            diag_jboss_services: utf8(src, "CRATONVM_DIAG_JBOSS_SERVICES"),
            diag_jca: present(src, "CRATONVM_DIAG_JCA"),
            diag_method_invoke_null: present(src, "CRATONVM_DIAG_METHOD_INVOKE_NULL"),
            diag_properties: one_true_yes_exact(src, "CRATONVM_DIAG_PROPERTIES"),
            diag_serviceloader: one_true_yes_exact(src, "CRATONVM_DIAG_SERVICELOADER"),
            enable_assertions: present(src, "CRATONVM_ENABLE_ASSERTIONS"),
            eqe_sync_execute: present(src, "CRATONVM_EQE_SYNC_EXECUTE"),
            forname_trace: present(src, "CRATONVM_FORNAME_TRACE"),
            http_max_body: usize_positive(src, "CRATONVM_HTTP_MAX_BODY"),
            iae_trace: present(src, "CRATONVM_IAE_TRACE"),
            iae_trace_ok: present_utf8(src, "CRATONVM_IAE_TRACE"),
            iae_trace2: present_utf8(src, "CRATONVM_IAE_TRACE2"),
            inherit_thread_ccl: on_unless_zero_or_false(src, "CRATONVM_INHERIT_THREAD_CCL"),
            inherit_tl_workaround: on_unless_zero_or_false(src, "CRATONVM_INHERIT_TL_WORKAROUND"),
            jboss_boot_log_file: utf8(src, "CRATONVM_JBOSS_BOOT_LOG_FILE"),
            jboss_brute_force_jars: exactly_one(src, "CRATONVM_JBOSS_BRUTE_FORCE_JARS"),
            jboss_logger_base_emit: exactly_one(src, "CRATONVM_JBOSS_LOGGER_BASE_EMIT"),
            loader_unload: on_unless_zero(src, "CRATONVM_LOADER_UNLOAD"),
            max_inflated_bytes: utf8(src, "CRATONVM_MAX_INFLATED_BYTES"),
            msc_real_start: on_unless_off_word_cased(src, "CRATONVM_MSC_REAL_START"),
            native_ec_multiply: present(src, "CRATONVM_NATIVE_EC_MULTIPLY"),
            native_matcher_find: on_unless_zero_or_false(src, "CRATONVM_NATIVE_MATCHER_FIND"),
            native_pbe_keyfactory: exactly_one(src, "CRATONVM_NATIVE_PBE_KEYFACTORY"),
            native_string_regex: on_unless_zero_or_false(src, "CRATONVM_NATIVE_STRING_REGEX"),
            netty_queue_bridge: on_unless_zero_or_false(src, "CRATONVM_NETTY_QUEUE_BRIDGE"),
            real_agroal: present(src, "CRATONVM_REAL_AGROAL"),
            real_annotations: on_unless_off_word_cased(src, "CRATONVM_REAL_ANNOTATIONS"),
            real_aqs: present(src, "CRATONVM_REAL_AQS"),
            real_forkjoinpool: !present(src, "CRATONVM_SYNTHETIC_FORKJOINPOOL")
                || present(src, "CRATONVM_REAL_FORKJOINPOOL"),
            real_jca: present(src, "CRATONVM_REAL_JCA"),
            real_proxy: truthy_word_default_true(src, "CRATONVM_REAL_PROXY"),
            real_proxy_strict: affirmative_word(src, "CRATONVM_REAL_PROXY_STRICT"),
            real_proxy_super: truthy_word_default_true(src, "CRATONVM_REAL_PROXY_SUPER"),
            real_proxy_super_set: present(src, "CRATONVM_REAL_PROXY_SUPER"),
            real_quarkus_start: present(src, "CRATONVM_REAL_QUARKUS_START"),
            real_stax_factory: on_unless_zero(src, "CRATONVM_REAL_STAX_FACTORY"),
            real_vertx: present(src, "CRATONVM_REAL_VERTX"),
            require_policy: present(src, "CRATONVM_REQUIRE_POLICY"),
            s111_dbg: present_utf8(src, "CRATONVM_S111_DBG"),
            sfi_null_trace: present(src, "CRATONVM_SFI_NULL_TRACE"),
            soft_exit: exactly_one(src, "CRATONVM_SOFT_EXIT"),
            spring_dbg: present(src, "CRATONVM_SPRING_DBG"),
            synthetic_agroal: present(src, "CRATONVM_SYNTHETIC_AGROAL"),
            synthetic_annotations: present(src, "CRATONVM_SYNTHETIC_ANNOTATIONS"),
            synthetic_aqs: present(src, "CRATONVM_SYNTHETIC_AQS"),
            synthetic_dsa: present(src, "CRATONVM_SYNTHETIC_DSA"),
            synthetic_ec: present(src, "CRATONVM_SYNTHETIC_EC"),
            synthetic_eqe: present_utf8(src, "CRATONVM_SYNTHETIC_EQE"),
            synthetic_forkjoinpool: present(src, "CRATONVM_SYNTHETIC_FORKJOINPOOL"),
            synthetic_memoryusage_tostring: one_true_yes_exact(
                src,
                "CRATONVM_SYNTHETIC_MEMORYUSAGE_TOSTRING",
            ),
            synthetic_mxbean_mapping: one_true_yes_exact(src, "CRATONVM_SYNTHETIC_MXBEAN_MAPPING"),
            synthetic_pqc: present(src, "CRATONVM_SYNTHETIC_PQC"),
            synthetic_quarkus_start: present(src, "CRATONVM_SYNTHETIC_QUARKUS_START"),
            synthetic_rsa: present(src, "CRATONVM_SYNTHETIC_RSA"),
            synthetic_vertx: present(src, "CRATONVM_SYNTHETIC_VERTX"),
            tls_openssl_client: on_unless_zero(src, "CRATONVM_TLS_OPENSSL_CLIENT"),
            trace_arrays_hashcode: present(src, "CRATONVM_TRACE_ARRAYS_HASHCODE"),
            trace_classvalue: present(src, "CRATONVM_TRACE_CLASSVALUE"),
            trace_pti_args: present(src, "CRATONVM_TRACE_PTI_ARGS"),
            ueh_debug: present(src, "CRATONVM_UEH_DEBUG"),
            uri_strict_chars: on_unless_zero(src, "CRATONVM_URI_STRICT_CHARS"),
            use_wildfly_reflect_shim: exactly_one(src, "CRATONVM_USE_WILDFLY_REFLECT_SHIM"),
            use_wildfly_synth_bytecode: exactly_one(src, "CRATONVM_USE_WILDFLY_SYNTH_BYTECODE"),
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Root
// ───────────────────────────────────────────────────────────────────────────

/// The whole typed configuration, populated once from a [`FlagSource`].
#[derive(Debug, Clone, Default)]
pub struct VmFlags {
    /// Garbage-collector flags.
    pub gc: GcFlags,
    /// JIT flags.
    pub jit: JitFlags,
    /// Class-loading flags.
    pub loader: LoaderFlags,
    /// I/O and networking flags.
    pub io: IoFlags,
    /// Flags read only by `native-builtins`.
    pub natives: NativeFlags,
    /// The subsystems migrated off direct environment reads under report P1:
    /// the JIT IR verifier and metrics, the GC card counters, the thread-state
    /// tripwire and the capability model. See [`crate::subsystem_config`] for
    /// why these live in their own module rather than as more `bool` fields
    /// here — every one of them is a tri-state, a path, a capacity or a mode
    /// word, and the type is the point.
    pub subsystems: crate::subsystem_config::SubsystemConfig,
    /// Resolved legacy values retained for configuration consumers that have
    /// not yet been converted to a typed field. Private so new code cannot
    /// widen the public configuration surface.
    legacy_values: MapSource,
    /// Test overrides for names the inventory does NOT declare.
    ///
    /// **Empty in every snapshot built from the process environment**, so the
    /// production read path is unchanged: [`runtime_var`] and
    /// [`runtime_var_os`] consult this only while [`overrides_active`] is true.
    ///
    /// It exists so a test can override an ordinary process variable —
    /// `JBOSS_HOME`, the proxy variables, `org.jboss.boot.log.file` — without
    /// calling `std::env::set_var`. That call is sound only in a
    /// single-threaded program (and `unsafe` in edition 2024) because `setenv`
    /// may reallocate and free the `environ` array while another thread is
    /// inside `getenv`; a `--lib` run doing it from three test modules across
    /// thousands of parallel tests is a process-wide data race, not a local
    /// one. Declared flags do not need this — they are already served from the
    /// latched snapshot.
    undeclared_edits: FxHashMapStrOpt,
}

impl VmFlags {
    /// Build from an explicit source. This is the testable entry point — it
    /// touches no globals, so unit tests never have to mutate process
    /// environment.
    pub fn from_source(src: &dyn FlagSource) -> Self {
        Self {
            gc: GcFlags::from_source(src),
            jit: JitFlags::from_source(src),
            loader: LoaderFlags::from_source(src),
            io: IoFlags::from_source(src),
            natives: NativeFlags::from_source(src),
            subsystems: crate::subsystem_config::SubsystemConfig::from_source(src),
            legacy_values: MapSource::declared_snapshot(src),
            // Empty here by construction: only `from_env_with_edits` populates
            // it, so a snapshot built from the real environment carries none
            // and the production read path never consults it.
            undeclared_edits: FxHashMapStrOpt::default(),
        }
    }

    fn legacy_var_os(&self, name: &str) -> Option<OsString> {
        self.legacy_values.get(name)
    }

    /// The override for an undeclared `name`, if this snapshot carries one.
    ///
    /// `Some(Some(v))` = overridden to `v`; `Some(None)` = overridden to
    /// "as if unset"; `None` = not overridden, so the caller reads the real
    /// environment. See [`VmFlags::undeclared_edits`].
    fn undeclared_edit(&self, name: &str) -> Option<&Option<OsString>> {
        self.undeclared_edits.get(name)
    }

    /// Build from the process environment.
    ///
    /// Snapshots `environ` once via [`MapSource::from_process_env`] instead of
    /// calling `getenv` per field; the values are identical.
    ///
    /// The snapshot is passed through [`crate::flag_groups::resolve`] first, so
    /// the ten grouped variables (`CRATONVM_JIT=-bce,unroll` and friends) reach
    /// every field below. Legacy per-flag names are what the grouped tokens
    /// expand *to*, so setting one directly still works unchanged.
    pub fn from_env() -> Self {
        Self::from_env_with_overrides(MapSource::empty())
    }

    /// Build from one process-environment snapshot plus launcher overrides.
    ///
    /// Overrides win over inherited variables, then grouped flag expressions
    /// are resolved across the combined source. Launchers should use this
    /// instead of mutating `environ` after argument parsing: once any
    /// CratonVM flag is read, the process configuration is immutable.
    pub fn from_env_with_overrides(overrides: MapSource) -> Self {
        Self::from_env_with_overrides_and_unsets(overrides, &[])
    }

    /// [`from_env_with_overrides`](Self::from_env_with_overrides), plus names to
    /// resolve as if they had **never been exported**.
    ///
    /// An overlay can only add or replace, and the majority parser
    /// ([`parse::present`]) reads *any* value — including `0` and the empty
    /// string — as **on**. So a launcher translating an explicit off-switch
    /// (`java -da` against an inherited `CRATONVM_ENABLE_ASSERTIONS`) cannot say
    /// what it means with an override alone; `.with(name, "0")` would turn the
    /// flag *on*. The unset list is applied after the overrides, so a name in
    /// both ends up absent.
    ///
    /// Same shape as [`from_env_with_edits`](Self::from_env_with_edits), which
    /// exists for tests; this one keeps the builder-style `MapSource` the
    /// launcher already assembles.
    pub fn from_env_with_overrides_and_unsets(overrides: MapSource, unset: &[&str]) -> Self {
        let mut raw = MapSource::from_process_env();
        raw.0.extend(overrides.0);
        for name in unset {
            raw.0.remove(*name);
        }
        Self::from_source(&crate::flag_groups::resolve(&raw))
    }

    /// Build from the process environment with per-name edits applied.
    ///
    /// `Some(value)` sets the variable; `None` makes it look **unset**, which
    /// [`from_env_with_overrides`](Self::from_env_with_overrides) cannot
    /// express (a `MapSource` overlay can only add). That asymmetry is what a
    /// test wanting "resolve as if `CRATONVM_JAVA_HOME` were never exported"
    /// runs into, so it gets its own constructor rather than a sentinel value.
    ///
    /// The edits are applied to the raw snapshot *before*
    /// [`crate::flag_groups::resolve`], so overriding either a grouped
    /// expression (`CRATONVM_JIT`) or the legacy key it expands to
    /// (`CRATONVM_JIT_THRESHOLD`) behaves exactly as exporting it would.
    pub fn from_env_with_edits(edits: &[(&str, Option<&str>)]) -> Self {
        let mut raw = MapSource::from_process_env();
        for (name, value) in edits {
            match value {
                Some(v) => {
                    raw.0.insert((*name).to_string(), OsString::from(*v));
                }
                None => {
                    raw.0.remove(*name);
                }
            }
        }
        let mut cfg = Self::from_source(&crate::flag_groups::resolve(&raw));
        // Edits naming something the inventory does not declare cannot survive
        // `declared_snapshot`, so keep them here instead. Without this the only
        // way to override such a name was to mutate `environ` — see the field.
        cfg.undeclared_edits = edits
            .iter()
            .filter(|(name, _)| !declared_flag_names().contains(*name))
            .map(|(name, value)| ((*name).to_string(), value.map(OsString::from)))
            .collect();
        cfg
    }
}

static FLAGS: OnceLock<VmFlags> = OnceLock::new();

fn declared_flag_names() -> &'static FxHashSetStr {
    static NAMES: OnceLock<FxHashSetStr> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names = FxHashSetStr::default();
        for entry in crate::flag_groups::INVENTORY {
            if let Some(name) = entry.on_key {
                names.insert(name);
            }
            if let Some(name) = entry.off_key {
                names.insert(name);
            }
        }
        names.extend(crate::flag_groups::SCALARS.iter().copied());
        names.extend(
            crate::flag_groups::Group::ALL
                .iter()
                .copied()
                .map(crate::flag_groups::Group::var),
        );
        names
    })
}

/// Number of live [`FlagOverride`] guards, process-wide.
///
/// This exists to keep [`flags()`] free on the production path. It is written
/// only by the test hooks below, so in a real VM process it is a never-dirtied
/// cache line whose load folds into the same predicted branch the `OnceLock`
/// probe already emits, and neither the thread-local nor the process slot is
/// touched at all. That is why the override support is compiled in
/// unconditionally instead of hiding behind a Cargo feature: a feature would
/// have to be enabled for the *whole* dependency graph during `cargo test`,
/// flipping it on every `cargo test` / `cargo build` alternation and forcing a
/// full workspace rebuild each way.
static OVERRIDES_LIVE: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// The calling thread's override, if any. See [`override_thread`].
    ///
    /// `Cell<Option<&'static _>>` has no destructor, so this registers no TLS
    /// dtor and [`LocalKey::with`](std::thread::LocalKey::with) can never
    /// observe a destroyed slot — but [`active_override`] still uses
    /// `try_with`, because `flags()` is reachable from teardown paths.
    static THREAD_OVERRIDE: Cell<Option<&'static VmFlags>> = const { Cell::new(None) };
}

/// The process-wide override, if any. See [`override_process`].
///
/// Only ever holds pointers obtained from `Box::leak`, so a load can never
/// observe a dangling pointer — the worst a racing reader can see is a
/// *stale* configuration, never freed memory.
static PROCESS_OVERRIDE: AtomicPtr<VmFlags> = AtomicPtr::new(std::ptr::null_mut());

/// The override in effect for the calling thread, thread scope winning.
///
/// Deliberately out of line: `flags()` is inlined at thousands of call sites
/// and the thread-local access must not be duplicated into every one of them.
#[inline(never)]
fn active_override() -> Option<&'static VmFlags> {
    if let Ok(Some(cfg)) = THREAD_OVERRIDE.try_with(|slot| slot.get()) {
        return Some(cfg);
    }
    let raw = PROCESS_OVERRIDE.load(Ordering::Acquire);
    if raw.is_null() {
        return None;
    }
    // SAFETY: `PROCESS_OVERRIDE` is written only by `override_process`, which
    // stores `Box::leak`ed pointers. Those stay valid for the remainder of the
    // process, so this reference is genuinely `'static`, and it is only ever
    // handed out as a shared reference.
    Some(unsafe { &*raw })
}

/// The process-wide configuration.
///
/// Initialised from the environment on first call. See the module docs for the
/// latching rules, and [`override_thread`] / [`override_process`] for the
/// test-support escape hatch from them.
#[inline]
pub fn flags() -> &'static VmFlags {
    if OVERRIDES_LIVE.load(Ordering::Relaxed) != 0 {
        if let Some(cfg) = active_override() {
            return cfg;
        }
    }
    FLAGS.get_or_init(VmFlags::from_env)
}

/// Whether any [`FlagOverride`] is currently installed, anywhere in the
/// process.
///
/// **For downstream memo layers.** `vm/src/runtime/env_cache.rs` memoises ~90
/// hot flags on top of this snapshot, so a test override would otherwise be
/// invisible to them: the memo latches on first read and never consults
/// [`flags()`] again. A memo whose value is not `Copy`-cheap enough for
/// [`MemoSlot`] consults this and recomputes from source while it is true.
///
/// A memo must NOT populate itself while this is true, or it would latch an
/// override's value permanently and poison the rest of the process.
#[inline]
pub fn overrides_active() -> bool {
    OVERRIDES_LIVE.load(Ordering::Relaxed) != 0
}

// ───────────────────────────────────────────────────────────────────────────
// Invalidatable memo slots
// ───────────────────────────────────────────────────────────────────────────

/// One downstream memo of a flag, which installing an override invalidates.
///
/// # Why this exists rather than "just check [`overrides_active`]"
///
/// `vm/src/runtime/env_cache.rs` memoises ~90 flags that are read from the
/// interpreter's per-bytecode path. Having each read ask "is an override
/// installed?" measured at **+1.15% of `execute_instruction`** — the reader
/// pays, forever, for a question whose answer is no in every real VM process.
///
/// So the cost moves to the writer instead. A slot holds its value inline in
/// one `AtomicU8`, a read is a single relaxed load, and installing or dropping
/// a [`FlagOverride`] walks every registered slot and resets it to unset. The
/// memo then re-derives on its next read, and while the override is live
/// [`publish`](Self::publish) declines to store, so nothing latches.
///
/// The encoding above `UNSET` is the caller's: `env_cache` uses 1/2 for
/// `false`/`true` and 1/2/3 for `None`/`Some(false)`/`Some(true)`. Anything
/// that does not fit a `u8` keeps the [`overrides_active`] check — none of
/// those are on the per-bytecode path.
#[derive(Debug)]
pub struct MemoSlot {
    state: AtomicU8,
}

/// The "not yet derived" state of a [`MemoSlot`]; every other value is the
/// caller's own encoding.
pub const MEMO_UNSET: u8 = 0;

/// Every [`MemoSlot`] that has published at least once, so an override can
/// find it. Written only on a memo's cold path and by override install/drop.
static MEMO_SLOTS: std::sync::Mutex<Vec<&'static MemoSlot>> = std::sync::Mutex::new(Vec::new());

impl MemoSlot {
    /// A slot that has not derived its value yet.
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(MEMO_UNSET),
        }
    }

    /// The memoised value, or [`MEMO_UNSET`] if it must be derived.
    ///
    /// The whole point of the type: one relaxed load of a static that is
    /// dirtied only by a test installing an override. Deliberately returns the
    /// raw byte rather than an `Option<u8>` — `u8` has no niche, so the
    /// `Option` costs a discriminant materialisation on the hottest read in
    /// the interpreter (worth 0.2% of `execute_instruction`, measured).
    #[inline]
    pub fn load(&self) -> u8 {
        self.state.load(Ordering::Relaxed)
    }

    /// Memoise `value`, unless an override is installed.
    ///
    /// Registers the slot so a later override can reset it. Takes the registry
    /// lock, which is what makes "no override is live" and "store" one step
    /// with respect to a concurrent [`override_thread`] — without that, an
    /// override installed between the check and the store would find the slot
    /// already walked and leave a stale base value behind it.
    ///
    /// # Panics
    ///
    /// If `value` is [`MEMO_UNSET`] — that would encode "derive me again" and
    /// spin the cold path forever.
    pub fn publish(&'static self, value: u8) {
        assert!(value != MEMO_UNSET, "MEMO_UNSET is not a publishable value");
        let mut slots = MEMO_SLOTS.lock().unwrap_or_else(|p| p.into_inner());
        if overrides_active() {
            return;
        }
        if !slots.iter().any(|s| std::ptr::eq(*s, self)) {
            slots.push(self);
        }
        self.state.store(value, Ordering::Relaxed);
    }
}

impl Default for MemoSlot {
    fn default() -> Self {
        Self::new()
    }
}

/// Reset every registered memo, and apply `delta` to the live-override count
/// under the same lock so a concurrent [`MemoSlot::publish`] cannot interleave.
fn invalidate_memos(delta: isize) {
    let slots = MEMO_SLOTS.lock().unwrap_or_else(|p| p.into_inner());
    if delta > 0 {
        OVERRIDES_LIVE.fetch_add(1, Ordering::Release);
    } else {
        OVERRIDES_LIVE.fetch_sub(1, Ordering::Release);
    }
    for slot in slots.iter() {
        slot.state.store(MEMO_UNSET, Ordering::Relaxed);
    }
}

/// Which threads a [`FlagOverride`] applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverrideScope {
    Thread,
    Process,
}

/// Restores the previous flag configuration when dropped.
///
/// Holds a raw pointer so it is neither `Send` nor `Sync`: a thread-scoped
/// guard that travelled to another thread would restore the wrong slot.
#[must_use = "the override is reverted the moment the guard is dropped"]
pub struct FlagOverride {
    scope: OverrideScope,
    previous: *mut VmFlags,
}

impl std::fmt::Debug for FlagOverride {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlagOverride")
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl Drop for FlagOverride {
    fn drop(&mut self) {
        match self.scope {
            OverrideScope::Thread => {
                let previous = as_static(self.previous);
                let _ = THREAD_OVERRIDE.try_with(|slot| slot.set(previous));
            }
            OverrideScope::Process => {
                PROCESS_OVERRIDE.store(self.previous, Ordering::Release);
            }
        }
        invalidate_memos(-1);
    }
}

fn as_static(raw: *mut VmFlags) -> Option<&'static VmFlags> {
    if raw.is_null() {
        None
    } else {
        // SAFETY: as in `active_override` — every non-null pointer stored in
        // either slot came from `Box::leak`.
        Some(unsafe { &*raw })
    }
}

fn as_raw(cfg: Option<&'static VmFlags>) -> *mut VmFlags {
    cfg.map_or(std::ptr::null_mut(), |r| {
        r as *const VmFlags as *mut VmFlags
    })
}

/// **Test support.** Replace the flag snapshot seen by the *calling thread*
/// until the returned guard is dropped.
///
/// This is the supported answer to "my test needs `CRATONVM_FOO=1`". Setting
/// the environment variable is not: declared flags are served from one
/// process-wide snapshot latched on first read (see the module docs), so
/// `set_var` after any other test in the same binary has touched [`flags()`]
/// changes `environ` and nothing else. A test written that way passes only
/// when it wins the race to initialise the snapshot, and silently exercises
/// the developer's ambient environment when it loses — the failure mode
/// documented in
/// `libcratonvm-no-jdk-test-order-dependent-fixed-20260730.md`.
///
/// Thread scope is the default because `cargo test` runs tests in parallel
/// within one binary: an override installed here cannot perturb a concurrent
/// test. Use [`override_process`] only when the code under test reads the flag
/// from a thread this one spawned.
///
/// # Limitations
///
/// Flags cached *downstream* of this snapshot are not affected —
/// `vm/src/runtime/env_cache.rs` memoises ~37 hot flags in their own
/// `OnceLock`s, and those latch independently on first read. Overriding one of
/// those is only reliable before its first read in the process.
///
/// `cfg` is leaked, so this is for tests and not for a hot loop.
pub fn override_thread(cfg: VmFlags) -> FlagOverride {
    let leaked: &'static VmFlags = Box::leak(Box::new(cfg));
    invalidate_memos(1);
    let previous = THREAD_OVERRIDE.with(|slot| slot.replace(Some(leaked)));
    FlagOverride {
        scope: OverrideScope::Thread,
        previous: as_raw(previous),
    }
}

/// **Test support.** Replace the flag snapshot seen by *every* thread until
/// the returned guard is dropped.
///
/// Prefer [`override_thread`]. Reach for this only when the reader runs on a
/// thread the test did not create — a booted `Vm`'s workers, a background JIT
/// compile — since the whole process is affected and a concurrently running
/// test that reads the same flag will see this value. Callers must serialise
/// process-scoped overrides among themselves (a `static Mutex` in the test
/// module is the usual way); nesting is supported, concurrent installs are
/// not.
///
/// Same `env_cache` limitation and same leak as [`override_thread`].
pub fn override_process(cfg: VmFlags) -> FlagOverride {
    let leaked: &'static VmFlags = Box::leak(Box::new(cfg));
    invalidate_memos(1);
    let previous = PROCESS_OVERRIDE.swap(as_raw(Some(leaked)), Ordering::AcqRel);
    FlagOverride {
        scope: OverrideScope::Process,
        previous,
    }
}

/// **Test support.** Run `f` with the process environment's flags plus `edits`,
/// visible to the calling thread only.
///
/// The ergonomic form of [`override_thread`] +
/// [`VmFlags::from_env_with_edits`]. `None` means "as if unset".
///
/// ```no_run
/// # use cratonvm_types::flags;
/// flags::with_thread_overrides(&[("CRATONVM_JAVA_HOME", Some("/tmp/empty"))], || {
///     // resolution here sees the override, whoever latched the snapshot first
/// });
/// ```
pub fn with_thread_overrides<R>(edits: &[(&str, Option<&str>)], f: impl FnOnce() -> R) -> R {
    let _guard = override_thread(VmFlags::from_env_with_edits(edits));
    f()
}

/// **Test support.** Process-scoped sibling of [`with_thread_overrides`].
///
/// See [`override_process`] for when this is the right one and for the
/// serialisation the caller owes.
pub fn with_process_overrides<R>(edits: &[(&str, Option<&str>)], f: impl FnOnce() -> R) -> R {
    let _guard = override_process(VmFlags::from_env_with_edits(edits));
    f()
}

/// Resolve a numeric operator knob that has a default and a hard ceiling,
/// **saying out loud when the value asked for is not the value in force**.
///
/// # Why this exists
///
/// `CRATONVM_NATIVE_SHADOW_SINK_CAP` was read as
///
/// ```ignore
/// runtime_var(NAME).ok().and_then(|v| v.trim().parse().ok())
///     .filter(|n| *n > 0 && *n <= MAX).unwrap_or(DEFAULT)
/// ```
///
/// which has one failure mode that is worse than the others: a value ABOVE the
/// ceiling falls back to the **default**, so `…=200000` against a ceiling of
/// 65,536 and a default of 4,096 yields **4,096** — an order of magnitude LESS
/// than the ceiling the operator was trying to exceed, silently. The operator
/// then reads `truncated: true` again and concludes the knob does not work.
///
/// It is self-inflicted: `vm-cli` advises `CRATONVM_NATIVE_SHADOW_SINK_CAP=`
/// `(recorded + dropped) * 2`, which exceeds 65,536 for any workload with more
/// than ~32,768 shadows — so the VM can print advice it then discards.
///
/// Two rules, and the asymmetry is the point:
///
/// * **too big → CLAMP to `max`.** The ceiling exists so a mistyped value
///   cannot turn a diagnostic sink into a memory leak; clamping preserves that
///   exactly while giving the operator the largest value the rule allows, which
///   is strictly closer to the intent than the default is.
/// * **zero, negative, empty or non-numeric → the DEFAULT.** There is no
///   "closer" value to fall back to, and a cap of zero would report an empty
///   population as a complete one — the exact failure this whole area exists to
///   remove. A diagnostic must never be the thing that fails.
///
/// Either way the run **prints one `[cratonvm]` line naming the value asked
/// for, the value in force, and why**. `what` names the sink so two callers
/// sharing one knob produce two distinguishable lines rather than the same
/// sentence twice.
///
/// Returns `default` when the variable is unset — the common case, silent.
pub fn resolve_capped_usize(name: &str, what: &str, default: usize, max: usize) -> usize {
    let (value, warning) =
        capped_var_verdict(runtime_var(name).ok().as_deref(), name, what, default, max);
    if let Some(w) = warning {
        eprintln!("{w}");
    }
    value
}

/// The decision and the sentence, with no I/O — so the PROSE is testable.
///
/// Split out after the first version of these messages shipped with 18-space
/// gaps in them (a patch script's Python `\`+newline joined the lines and kept
/// the indentation inside the Rust literal). That was found by running a
/// six-minute LTO build and reading stderr, which is the wrong loop for a
/// string literal; `the_warnings_read_as_one_sentence` now catches it in
/// `cargo test -p cratonvm-types`.
///
/// Returns `(value in force, Some(warning) when the value asked for is not it)`.
fn capped_var_verdict(
    raw: Option<&str>,
    name: &str,
    what: &str,
    default: usize,
    max: usize,
) -> (usize, Option<String>) {
    debug_assert!(
        default <= max,
        "a default above the ceiling can never be reached"
    );
    let Some(raw) = raw else {
        return (default, None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return (default, None);
    }
    match trimmed.parse::<usize>() {
        Ok(0) => (
            default,
            Some(format!(
                "[cratonvm] warning: {name}=0 is not a usable {what} cap — a cap of zero \
                 reports an empty population as a complete one. Using the default {default}."
            )),
        ),
        Ok(n) if n > max => (
            max,
            Some(format!(
                "[cratonvm] warning: {name}={n} exceeds the {what} ceiling of {max}; CLAMPED \
                 to {max}. (It used to fall back to the default {default}, which is smaller \
                 than the ceiling and said nothing.)"
            )),
        ),
        Ok(n) => (n, None),
        Err(_) => (
            default,
            Some(format!(
                "[cratonvm] warning: {name}={trimmed:?} is not a number — the {what} cap \
                 stays at the default {default}."
            )),
        ),
    }
}

/// Read an environment value through the runtime configuration boundary.
///
/// Declared CratonVM flags come from the one immutable [`VmFlags`] snapshot.
/// Ordinary process variables (`HOME`, `TZ`, application `System.getenv`
/// names, and so on) retain `std::env`'s live-read semantics.
#[inline]
pub fn runtime_var<K: AsRef<OsStr>>(key: K) -> Result<String, std::env::VarError> {
    let key = key.as_ref();
    if let Some(name) = key.to_str() {
        if declared_flag_names().contains(name) {
            return match flags().legacy_var_os(name) {
                Some(value) => value.into_string().map_err(std::env::VarError::NotUnicode),
                None => Err(std::env::VarError::NotPresent),
            };
        }
        if overrides_active() {
            if let Some(edit) = flags().undeclared_edit(name) {
                return match edit {
                    Some(value) => value
                        .clone()
                        .into_string()
                        .map_err(std::env::VarError::NotUnicode),
                    None => Err(std::env::VarError::NotPresent),
                };
            }
        }
    }
    std::env::var(key)
}

/// OS-native sibling of [`runtime_var`].
#[inline]
pub fn runtime_var_os<K: AsRef<OsStr>>(key: K) -> Option<OsString> {
    let key = key.as_ref();
    // Per-name read census (`CRATONVM_DBG_FLAGREADS=1`). Kept because it is the
    // instrument that found the `CRATONVM_DBG_COMPACT_INLINE` read in
    // `jit_getfield` — `perf` put `runtime_var_os` at 1.21% of a `BigDecimal`
    // benchmark, but inlining defeated stack attribution and a dwarf capture
    // pointed at the callee side of the JIT->heap boundary. The KEY names the
    // caller directly, and did so in one run. Off, it is one relaxed atomic load.
    flag_read_census(key);
    if let Some(name) = key.to_str() {
        if declared_flag_names().contains(name) {
            return flags().legacy_var_os(name);
        }
        if overrides_active() {
            if let Some(edit) = flags().undeclared_edit(name) {
                return edit.clone();
            }
        }
    }
    std::env::var_os(key)
}

/// Per-flag-name read census — see the call in [`runtime_var_os`].
///
/// `CRATONVM_DBG_FLAGREADS=1` prints the top offenders every 200k reads. A flag
/// read is supposed to be rare (every gate is expected to cache its answer), so
/// a name appearing here in the millions IS the bug — which is exactly how
/// `CRATONVM_DBG_COMPACT_INLINE` was found at 4,560,891 of 4,600,000 reads
/// (99.1%) on a 50k-iteration `BigDecimal` run, uncached inside `jit_getfield`.
/// After that fix the same run does not reach the first 200k report at all.
fn flag_read_census(key: &OsStr) {
    use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
    static GATE: AtomicU8 = AtomicU8::new(0); // 0 unknown, 1 off, 2 on
    let gate = match GATE.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            // std::env directly: reading through this module would recurse.
            let on = std::env::var_os("CRATONVM_DBG_FLAGREADS").is_some();
            GATE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    };
    if !gate {
        return;
    }
    static TOTAL: AtomicU64 = AtomicU64::new(0);
    static COUNTS: OnceLock<std::sync::Mutex<HashMap<String, u64>>> = OnceLock::new();
    let map = COUNTS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let name = key.to_string_lossy().into_owned();
    let n = TOTAL.fetch_add(1, Ordering::Relaxed) + 1;
    if let Ok(mut g) = map.lock() {
        *g.entry(name).or_insert(0) += 1;
        if n % 200_000 == 0 {
            let mut v: Vec<(String, u64)> = g.iter().map(|(k, c)| (k.clone(), *c)).collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            eprintln!("[flagreads] total={n}");
            for (k, c) in v.iter().take(8) {
                eprintln!("[flagreads]   {c:>10}  {k}");
            }
        }
    }
}

/// Publish an explicitly-built configuration.
///
/// Call this from the launcher, before anything reads a flag, to layer `-XX:`
/// options over the environment. Returns `Err` with the rejected value if the
/// configuration has already been latched — which means something read a flag
/// first, and the caller's overrides would have been silently ignored.
pub fn install(cfg: VmFlags) -> Result<(), VmFlags> {
    FLAGS.set(cfg)
}

/// Whether the configuration has been latched yet.
pub fn is_installed() -> bool {
    FLAGS.get().is_some()
}

/// Opt back in to the pre-2026-07-27 Mockito selector overrides.
///
/// CratonVM used to intercept `LocationFactory.create` and
/// `ModuleMemberAccessor.delegate` with natives that unconditionally produced
/// Mockito's *fallback* implementations (a `Java8LocationImpl` carrying a
/// hardcoded `"-> at <<unknown line>>"`, and `ReflectionMemberAccessor`).
/// HotSpot picks `LocationImpl` (StackWalker) and
/// `InstrumentationMemberAccessor`; both real selectors now run here too.
///
/// `CRATONVM_COMPAT=mockito-legacy-selectors` (or the legacy spelling
/// `CRATONVM_MOCKITO_LEGACY_SELECTORS=1`) restores the old interception as an
/// escape hatch. Read through `runtime_var` so it stays inside the declared
/// flag surface; cached, and not on any hot path — every caller gates on a
/// class-name match first.
pub fn mockito_legacy_selectors() -> bool {
    static LEGACY: OnceLock<bool> = OnceLock::new();
    *LEGACY.get_or_init(|| {
        matches!(
            runtime_var("CRATONVM_MOCKITO_LEGACY_SELECTORS").as_deref(),
            Ok("1") | Ok("true")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(pairs: &[(&str, &str)]) -> MapSource {
        MapSource::new(pairs.iter().copied())
    }

    #[test]
    fn vm_flags_retain_resolved_legacy_values_for_boundary_reads() {
        let f = VmFlags::from_source(&src(&[("CRATONVM_DBG_DEOPT", "enabled")]));
        assert_eq!(
            f.legacy_var_os("CRATONVM_DBG_DEOPT"),
            Some(OsString::from("enabled"))
        );
        assert_eq!(f.legacy_var_os("CRATONVM_DBG_AIOOBE"), None);
    }

    /// An UNDECLARED name is overridable, so a test never has to write to
    /// `environ` to arrange one.
    ///
    /// This is the whole point of `VmFlags::undeclared_edits`.
    /// `std::env::set_var` is sound only in a single-threaded program — glibc's
    /// `setenv` may reallocate and free the `environ` array while another
    /// thread is inside `getenv` — and `native-builtins`' `--lib` was doing it
    /// from three test modules while ~4,130 tests ran on parallel threads. The
    /// override serves the same reads with no write at all.
    #[test]
    fn an_undeclared_name_is_overridable_without_writing_to_environ() {
        // Deliberately not a `CRATONVM_*` name: this must exercise the
        // undeclared path, and the declared path already has its own tests.
        const NAME: &str = "cratonvm_undeclared_override_probe";
        assert!(
            std::env::var_os(NAME).is_none(),
            "fixture name must not exist in the real environment"
        );

        with_thread_overrides(&[(NAME, Some("set-by-override"))], || {
            assert_eq!(runtime_var(NAME).ok().as_deref(), Some("set-by-override"));
            assert_eq!(
                runtime_var_os(NAME),
                Some(OsString::from("set-by-override"))
            );
        });

        // The override wrote nothing, so the process environment is untouched
        // and the read outside it is the real (absent) one.
        assert!(std::env::var_os(NAME).is_none());
        assert!(runtime_var_os(NAME).is_none());

        // The other direction: an existing variable overridden to "as if
        // unset". `PATH` is undeclared and always present.
        assert!(std::env::var_os("PATH").is_some(), "PATH must be set");
        with_thread_overrides(&[("PATH", None)], || {
            assert!(
                runtime_var_os("PATH").is_none(),
                "an override to None must read as unset"
            );
            assert!(matches!(
                runtime_var("PATH"),
                Err(std::env::VarError::NotPresent)
            ));
        });
        assert!(runtime_var_os("PATH").is_some(), "and it comes back after");
    }

    /// `synthetic_memoryusage_tostring` is served by the snapshot, and reads
    /// the same three truth words its siblings do.
    ///
    /// `native-builtins`' `memoryusage_tostring_shim_enabled` read this name
    /// with a raw `std::env::var` and a bespoke `Ok("1") | Ok("true")` table
    /// when it landed, so the value the VM acted on came from the live process
    /// environment — invisible to `with_thread_overrides`, unreachable from
    /// `CRATONVM_REAL=-memoryusage-tostring`, and silently the developer's
    /// ambient environment in any test that tried to arrange it. This pins the
    /// parse; `flag_groups::tests::the_20260811_declarations_expand_from_their_group_spelling`
    /// pins the grouped spelling that feeds it.
    #[test]
    fn synthetic_memoryusage_tostring_is_snapshot_backed_and_reads_one_true_yes() {
        assert!(
            !VmFlags::from_source(&MapSource::empty())
                .natives
                .synthetic_memoryusage_tostring,
            "the real JDK bytecode is the default",
        );
        for word in ["1", "true", "yes"] {
            assert!(
                VmFlags::from_source(&src(&[("CRATONVM_SYNTHETIC_MEMORYUSAGE_TOSTRING", word)]))
                    .natives
                    .synthetic_memoryusage_tostring,
                "`{word}` must turn the shim back on",
            );
        }
        for word in ["0", "false", "no", ""] {
            assert!(
                !VmFlags::from_source(&src(&[("CRATONVM_SYNTHETIC_MEMORYUSAGE_TOSTRING", word)]))
                    .natives
                    .synthetic_memoryusage_tostring,
                "`{word}` must leave the default alone",
            );
        }
    }

    #[test]
    fn undeclared_process_values_keep_live_standard_environment_semantics() {
        let key = if cfg!(windows) { "PATH" } else { "HOME" };
        assert_eq!(runtime_var_os(key), std::env::var_os(key));
        assert_eq!(runtime_var(key), std::env::var(key));
    }

    #[test]
    fn empty_source_matches_all_documented_defaults() {
        let f = VmFlags::from_source(&MapSource::empty());
        // Every opt-in flag is off…
        // `moving_young` is no longer an opt-in. Pin the shipped default
        // directly so an accidental reversion cannot hide behind the constant.
        //
        // If you are here because this assertion failed, read
        // `DEFAULT_MOVING_YOUNG`'s doc comment before changing it: this flag is
        // read by fixes that have nothing to do with compaction, and flipping
        // it has twice disarmed one of them silently, in both directions,
        // without failing any other test. A green suite is not evidence that a
        // flip was safe — the last one shipped with the whole tomcat,
        // hibernate, gc, jit and HotSpot-differential sweep green and still
        // re-opened a class-unloading defect that had been fixed the day
        // before.
        assert!(DEFAULT_MOVING_YOUNG);
        assert!(f.gc.moving_young);
        assert!(!f.gc.card_table_only);
        assert!(!f.gc.dbg_a2);
        assert!(!f.jit.shadow_stack);
        assert!(!f.jit.method_stats);
        // …and every default-ON flag is on.
        assert!(f.gc.old_sweep_jit);
        assert!(f.io.real_net_sockets);
        assert!(f.natives.real_forkjoinpool);
        // Value flags fall back.
        assert_eq!(f.gc.g1_workers, None);
        assert_eq!(f.gc.gc_stress_bytes, None);
        assert_eq!(f.gc.dbg_stale_objref_cycles, 1);
        assert_eq!(f.gc.dbg_watch_cell, 0);
        assert_eq!(f.gc.dbg_blocked_access, BlockedAccessMode::Off);
        assert!(!f.jit.disable_jit);
    }

    #[test]
    fn concurrency_and_socket_legacy_surfaces_require_explicit_opt_out() {
        let f = VmFlags::from_source(&src(&[
            ("CRATONVM_SYNTHETIC_FORKJOINPOOL", "1"),
            ("CRATONVM_SYNTHETIC_NET_SOCKETS", "1"),
        ]));
        assert!(!f.natives.real_forkjoinpool);
        assert!(!f.io.real_net_sockets);

        let f = VmFlags::from_source(&src(&[
            ("CRATONVM_SYNTHETIC_FORKJOINPOOL", "1"),
            ("CRATONVM_REAL_FORKJOINPOOL", "1"),
            ("CRATONVM_SYNTHETIC_NET_SOCKETS", "1"),
            ("CRATONVM_REAL_NET_SOCKETS", "1"),
        ]));
        assert!(f.natives.real_forkjoinpool);
        assert!(f.io.real_net_sockets);
    }

    #[test]
    fn launcher_override_and_grouped_flags_share_one_resolved_snapshot() {
        let mut raw = src(&[("CRATONVM_DBG", "jit-method-stats")]);
        raw.0
            .extend(MapSource::empty().with("CRATONVM_DISABLE_JIT", "1").0);
        let f = VmFlags::from_source(&crate::flag_groups::resolve(&raw));
        assert!(f.jit.disable_jit);
        assert!(f.jit.method_stats);
    }

    #[test]
    fn jit_method_stats_uses_the_grouped_and_legacy_spelling_consistently() {
        let grouped = src(&[("CRATONVM_DBG", "jit-method-stats")]);
        let grouped = crate::flag_groups::resolve(&grouped);
        assert!(VmFlags::from_source(&grouped).jit.method_stats);

        assert!(
            VmFlags::from_source(&src(&[("CRATONVM_DBG_JIT_METHOD_STATS", "1")]))
                .jit
                .method_stats
        );
        assert!(
            !VmFlags::from_source(&src(&[("CRATONVM_DBG_JIT_METHOD_STATS", "0")]))
                .jit
                .method_stats
        );
    }

    #[test]
    fn gc_par_threads_keeps_zero_because_zero_disables_parallelism() {
        // `CRATONVM_GC_PAR_THREADS=0` means "no parallel young GC". It is
        // therefore `usize_opt`, NOT `usize_min1` like its neighbour
        // `CRATONVM_G1_WORKERS` — clamping it to >= 1 would silently turn the
        // documented kill-switch into "one worker", and the caller
        // (`young_mark::young_gc_threads`) already does its own `.max(1)`.
        let f = VmFlags::from_source(&src(&[("CRATONVM_GC_PAR_THREADS", "0")]));
        assert_eq!(f.gc.gc_par_threads, Some(0));
        // The lookalike really is clamped — the two must not be unified.
        let g = VmFlags::from_source(&src(&[("CRATONVM_G1_WORKERS", "0")]));
        assert_eq!(g.gc.g1_workers, Some(1));
    }

    #[test]
    fn gc_sweep_anchor_stride_ignores_values_below_64() {
        // Sub-64-byte strides would mint an anchor inside almost every object;
        // the pre-config code filtered them out and fell back to the default.
        let lo = VmFlags::from_source(&src(&[("CRATONVM_GC_SWEEP_ANCHOR_STRIDE", "32")]));
        assert_eq!(lo.gc.gc_sweep_anchor_stride, 8 * 1024 * 1024);
        let ok = VmFlags::from_source(&src(&[("CRATONVM_GC_SWEEP_ANCHOR_STRIDE", "64")]));
        assert_eq!(ok.gc.gc_sweep_anchor_stride, 64);
        let junk = VmFlags::from_source(&src(&[("CRATONVM_GC_SWEEP_ANCHOR_STRIDE", "nope")]));
        assert_eq!(junk.gc.gc_sweep_anchor_stride, 8 * 1024 * 1024);
    }

    #[test]
    fn gc_parallel_value_flags_default_as_documented() {
        let f = VmFlags::from_source(&MapSource::empty());
        assert_eq!(f.gc.gc_sweep_anchor_stride, 8 * 1024 * 1024);
        assert_eq!(f.gc.gc_par_threads, None);
        assert_eq!(f.gc.gc_par_min_bytes, 16 * 1024 * 1024);
    }

    #[test]
    fn presence_parser_treats_zero_as_set() {
        // This is the surprising-but-existing majority semantics: `X=0` is ON
        // for every `var_os(..).is_some()` site. Locked down so the migration
        // cannot quietly "fix" it. `moving_young` keeps this semantics for its
        // compatibility opt-in — `jit/src/x64.rs` still parses the same
        // variable with `var_os(..).is_some()`, and the two MUST agree.
        let f = VmFlags::from_source(&src(&[("CRATONVM_MOVING_YOUNG", "0")]));
        assert!(f.gc.moving_young);
        let f = VmFlags::from_source(&src(&[("CRATONVM_MOVING_YOUNG", "")]));
        assert!(f.gc.moving_young);
    }

    /// The moving-young gate is an opt-OUT over [`DEFAULT_MOVING_YOUNG`], not
    /// an opt-in, and the opt-out wins over the compatibility opt-in.
    ///
    /// There is deliberately no second flag to also satisfy: the removed
    /// `CRATONVM_ALLOW_MOVING_YOUNG` had to be set *in addition* to
    /// `CRATONVM_MOVING_YOUNG` before `gen_heap` would run a moving cycle under
    /// a live JIT frame, so the documented way to enable the feature could
    /// never actually enable it in the case it exists for.
    #[test]
    fn moving_young_is_an_opt_out_with_a_compatibility_opt_in() {
        assert!(
            VmFlags::from_source(&MapSource::empty()).gc.moving_young,
            "the shipped generational collector must compact young by default"
        );
        assert!(
            !VmFlags::from_source(&src(&[("CRATONVM_NO_MOVING_YOUNG", "1")]))
                .gc
                .moving_young
        );
        // Opt-out beats opt-in — "turn it off" must always be honoured.
        let both = VmFlags::from_source(&src(&[
            ("CRATONVM_MOVING_YOUNG", "1"),
            ("CRATONVM_NO_MOVING_YOUNG", "1"),
        ]));
        assert!(!both.gc.moving_young);
        // And the opt-in alone is sufficient: nothing else has to be set.
        assert!(
            VmFlags::from_source(&src(&[("CRATONVM_MOVING_YOUNG", "1")]))
                .gc
                .moving_young
        );
    }

    #[test]
    fn old_sweep_jit_is_opt_out_on_zero_only() {
        assert!(
            VmFlags::from_source(&src(&[("CRATONVM_OLD_SWEEP_JIT", "1")]))
                .gc
                .old_sweep_jit
        );
        assert!(
            VmFlags::from_source(&src(&[("CRATONVM_OLD_SWEEP_JIT", "no")]))
                .gc
                .old_sweep_jit
        );
        assert!(
            !VmFlags::from_source(&src(&[("CRATONVM_OLD_SWEEP_JIT", "0")]))
                .gc
                .old_sweep_jit
        );
    }

    #[test]
    fn g1_parallel_evac_is_default_on_with_a_zero_opt_out() {
        // Flipped 2026-08-13. This test used to pin the OPT-IN semantics
        // (`one_or_true`): unset meant serial. Parallel evacuation is now the
        // default, so the meaningful assertions are the inverse — unset means
        // parallel, and only an explicit "0" gets you the single-threaded
        // evacuator, which is the bisection lever for a suspected
        // parallel-evacuation regression.
        assert!(
            VmFlags::from_source(&src(&[])).gc.g1_parallel_evac,
            "unset must now select the parallel evacuator"
        );
        assert!(
            !VmFlags::from_source(&src(&[("CRATONVM_G1_PARALLEL_EVAC", "0")]))
                .gc
                .g1_parallel_evac,
            "=0 is the documented opt-out and the bisection lever"
        );
        // Anything that is not "0" leaves the default in force, including the
        // spellings the old opt-in parser rejected. That is the `on_unless_zero`
        // contract, shared with `CRATONVM_OLD_SWEEP_JIT`.
        for v in ["1", "TRUE", "yes", "2", ""] {
            assert!(
                VmFlags::from_source(&src(&[("CRATONVM_G1_PARALLEL_EVAC", v)]))
                    .gc
                    .g1_parallel_evac,
                "{v:?} is not the opt-out, so parallel stays on"
            );
        }
    }

    #[test]
    fn g1_workers_clamps_to_one() {
        assert_eq!(
            VmFlags::from_source(&src(&[("CRATONVM_G1_WORKERS", "0")]))
                .gc
                .g1_workers,
            Some(1)
        );
        assert_eq!(
            VmFlags::from_source(&src(&[("CRATONVM_G1_WORKERS", " 6 ")]))
                .gc
                .g1_workers,
            Some(6)
        );
        assert_eq!(
            VmFlags::from_source(&src(&[("CRATONVM_G1_WORKERS", "many")]))
                .gc
                .g1_workers,
            None
        );
    }

    #[test]
    fn gc_stress_prefers_dbg_spelling_and_rejects_zero() {
        let f = VmFlags::from_source(&src(&[
            ("CRATONVM_DBG_GC_STRESS", "4096"),
            ("CRATONVM_GC_STRESS", "8192"),
        ]));
        assert_eq!(f.gc.gc_stress_bytes, Some(4096));
        let f = VmFlags::from_source(&src(&[("CRATONVM_GC_STRESS", "8192")]));
        assert_eq!(f.gc.gc_stress_bytes, Some(8192));
        let f = VmFlags::from_source(&src(&[("CRATONVM_GC_STRESS", "0")]));
        assert_eq!(f.gc.gc_stress_bytes, None);
    }

    #[test]
    fn blocked_access_modes() {
        assert_eq!(
            VmFlags::from_source(&src(&[("CRATONVM_DBG_BLOCKED_ACCESS", "warn")]))
                .gc
                .dbg_blocked_access,
            BlockedAccessMode::Warn
        );
        assert_eq!(
            VmFlags::from_source(&src(&[("CRATONVM_DBG_BLOCKED_ACCESS", "1")]))
                .gc
                .dbg_blocked_access,
            BlockedAccessMode::Panic
        );
        assert_eq!(
            VmFlags::from_source(&src(&[("CRATONVM_DBG_BLOCKED_ACCESS", "0")]))
                .gc
                .dbg_blocked_access,
            BlockedAccessMode::Off
        );
    }

    #[test]
    fn watch_cell_accepts_hex_with_or_without_prefix() {
        assert_eq!(
            VmFlags::from_source(&src(&[("CRATONVM_DBG_WATCH_CELL", "0x7fff00")]))
                .gc
                .dbg_watch_cell,
            0x7fff00
        );
        assert_eq!(
            VmFlags::from_source(&src(&[("CRATONVM_DBG_WATCH_CELL", "7fff00")]))
                .gc
                .dbg_watch_cell,
            0x7fff00
        );
        assert_eq!(
            VmFlags::from_source(&src(&[("CRATONVM_DBG_WATCH_CELL", "zzz")]))
                .gc
                .dbg_watch_cell,
            0
        );
    }

    #[test]
    fn stale_objref_cycles_defaults_and_floors_at_one() {
        assert_eq!(
            VmFlags::from_source(&src(&[("CRATONVM_DBG_STALE_OBJREF_CYCLES", "4")]))
                .gc
                .dbg_stale_objref_cycles,
            4
        );
        assert_eq!(
            VmFlags::from_source(&src(&[("CRATONVM_DBG_STALE_OBJREF_CYCLES", "0")]))
                .gc
                .dbg_stale_objref_cycles,
            1
        );
    }

    #[test]
    fn oobfield_is_readable_both_as_gate_and_as_value() {
        let f = VmFlags::from_source(&src(&[("CRATONVM_DBG_OOBFIELD", "myField")]));
        assert!(f.gc.dbg_oobfield);
        assert_eq!(f.gc.dbg_oobfield_value.as_deref(), Some("myField"));
    }

    #[test]
    fn overlay_source_beats_environment() {
        // No env var set for this name, so the overlay is what shows through.
        let overlay = OverlaySource::new(MapSource::empty().with("CRATONVM_CARD_TABLE_ONLY", "1"));
        assert!(VmFlags::from_source(&overlay).gc.card_table_only);
    }

    #[test]
    fn loader_defaults_are_on_where_the_call_sites_defaulted_on() {
        let f = VmFlags::from_source(&MapSource::empty());
        assert!(f.loader.loader_aware_resolution);
        assert!(f.loader.boot_module_registry);
        assert!(!f.loader.allow_jsr_ret);
        assert!(!f.loader.harden_manifest_classpath);
    }

    #[test]
    fn loader_aware_resolution_is_off_only_for_empty_or_zero() {
        for (v, want) in [("", false), ("0", false), ("1", true), ("no", true)] {
            assert_eq!(
                VmFlags::from_source(&src(&[("CRATONVM_LOADER_AWARE_RESOLUTION", v)]))
                    .loader
                    .loader_aware_resolution,
                want,
                "value {v:?}"
            );
        }
    }

    #[test]
    fn boot_module_registry_off_words_are_exact_and_lowercase() {
        for (v, want) in [
            ("0", false),
            ("false", false),
            ("no", false),
            ("NO", true),
            ("", true),
        ] {
            assert_eq!(
                VmFlags::from_source(&src(&[("CRATONVM_BOOT_MODULE_REGISTRY", v)]))
                    .loader
                    .boot_module_registry,
                want,
                "value {v:?}"
            );
        }
    }

    #[test]
    fn io_profile_flags_use_the_word_truth_table() {
        for (v, want) in [
            ("1", true),
            ("yes", true),
            ("0", false),
            ("off", false),
            ("FALSE", false),
            ("", false),
        ] {
            assert_eq!(
                VmFlags::from_source(&src(&[("CRATONVM_CONFINE_IO", v)]))
                    .io
                    .confine_io,
                want,
                "value {v:?}"
            );
        }
    }

    #[test]
    fn synthetic_opt_ins_need_the_exact_string_one() {
        assert!(
            VmFlags::from_source(&src(&[("CRATONVM_SYNTHETIC_RAF", "1")]))
                .io
                .synthetic_raf_forced
        );
        assert!(
            !VmFlags::from_source(&src(&[("CRATONVM_SYNTHETIC_RAF", "true")]))
                .io
                .synthetic_raf_forced
        );
        assert!(
            !VmFlags::from_source(&src(&[("CRATONVM_SYNTHETIC_RAF", "01")]))
                .io
                .synthetic_raf_forced
        );
    }

    #[test]
    fn selector_flags_reject_zero_empty_and_false() {
        for (v, want) in [
            ("1", true),
            ("  x ", true),
            ("0", false),
            ("", false),
            ("FaLsE", false),
        ] {
            assert_eq!(
                VmFlags::from_source(&src(&[("CRATONVM_DBG_SELECTOR", v)]))
                    .io
                    .dbg_selector,
                want,
                "value {v:?}"
            );
        }
    }

    #[test]
    fn numeric_io_caps_reject_non_positive() {
        assert_eq!(
            VmFlags::from_source(&src(&[("CRATONVM_SELECT_MAX_BLOCK_MS", "0")]))
                .io
                .select_max_block_ms,
            None
        );
        assert_eq!(
            VmFlags::from_source(&src(&[("CRATONVM_SELECT_MAX_BLOCK_MS", "250")]))
                .io
                .select_max_block_ms,
            Some(250)
        );
        assert_eq!(
            VmFlags::from_source(&src(&[("CRATONVM_ZIP_MAX_ENTRY_BYTES", "0")]))
                .io
                .zip_max_entry_bytes,
            None
        );
    }

    #[test]
    fn socket_capture_empty_is_none() {
        assert_eq!(
            VmFlags::from_source(&src(&[("CRATONVM_SOCKET_CAPTURE", "")]))
                .io
                .socket_capture_prefix,
            None
        );
        assert_eq!(
            VmFlags::from_source(&src(&[("CRATONVM_SOCKET_CAPTURE", "/tmp/cap")]))
                .io
                .socket_capture_prefix
                .as_deref(),
            Some("/tmp/cap")
        );
    }

    #[test]
    fn stale_objref_cycles_does_not_trim_matching_the_original_call_site() {
        // The pre-refactor parse was `s.parse::<usize>()` with no `.trim()`,
        // so a padded value fell back to the default. Locked down so the
        // migration is byte-exact rather than merely "reasonable".
        assert_eq!(
            VmFlags::from_source(&src(&[("CRATONVM_DBG_STALE_OBJREF_CYCLES", " 4 ")]))
                .gc
                .dbg_stale_objref_cycles,
            1
        );
    }

    #[test]
    fn boolean_parsers_disagree_exactly_as_documented() {
        // Regression lock on census §10: five parsers, five answers for "0".
        let s = src(&[("X", "0")]);
        assert!(parse::present(&s, "X"));
        assert!(!parse::non_empty_non_zero(&s, "X"));
        assert!(!parse::truthy_word(&s, "X"));
        assert!(!parse::affirmative_word(&s, "X"));
        assert!(!parse::one_or_true(&s, "X"));
        assert!(!parse::on_unless_zero(&s, "X"));

        let s = src(&[("X", "off")]);
        assert!(parse::present(&s, "X"));
        assert!(parse::non_empty_non_zero(&s, "X"));
        assert!(!parse::truthy_word(&s, "X"));
        assert!(!parse::affirmative_word(&s, "X"));
        assert!(parse::on_unless_zero(&s, "X"));
    }

    /// The `native-builtins` migration (T3.5b) surfaced four more parsers that
    /// no existing one matched. Census §10 said the count of disagreeing truth
    /// tables was a lower bound; it was. This locks the new ones apart from
    /// their nearest neighbours so none of them can be quietly unified.
    #[test]
    fn native_builtins_parsers_add_four_more_truth_tables() {
        let empty = MapSource::empty();

        // Unset splits the two default-ON word parsers from their default-OFF
        // twins, and `one_true_yes_exact` from everything default-ON.
        assert!(!parse::truthy_word(&empty, "X"));
        assert!(parse::truthy_word_default_true(&empty, "X"));
        assert!(parse::on_unless_zero_or_false(&empty, "X"));
        assert!(parse::on_unless_off_word_cased(&empty, "X"));
        assert!(!parse::one_true_yes_exact(&empty, "X"));

        // The empty string is OFF for `truthy_word` and ON for its twin — the
        // divergence is not only in the default.
        let s = src(&[("X", "")]);
        assert!(!parse::truthy_word(&s, "X"));
        assert!(parse::truthy_word_default_true(&s, "X"));
        assert!(parse::on_unless_zero_or_false(&s, "X"));
        assert!(!parse::non_empty_non_zero_non_false(&s, "X"));

        // Mixed case is a genuine three-way split.
        let s = src(&[("X", "False")]);
        assert!(!parse::on_unless_zero_or_false(&s, "X"));
        assert!(parse::on_unless_off_word_cased(&s, "X"));
        assert!(parse::on_unless_off_word(&s, "X"));
        assert!(!parse::truthy_word_default_true(&s, "X"));

        // `OFF` splits the two `on_unless_off_word*` parsers the other way.
        let s = src(&[("X", "OFF")]);
        assert!(!parse::on_unless_off_word_cased(&s, "X"));
        assert!(parse::on_unless_off_word(&s, "X"));
        assert!(!parse::truthy_word_default_true(&s, "X"));

        // `on` is affirmative but not one-true-yes; whitespace is trimmed by
        // `affirmative_word` and not by `one_true_yes_exact`.
        let s = src(&[("X", "on")]);
        assert!(parse::affirmative_word(&s, "X"));
        assert!(!parse::one_true_yes_exact(&s, "X"));
        let s = src(&[("X", " 1 ")]);
        assert!(parse::affirmative_word(&s, "X"));
        assert!(!parse::one_true_yes_exact(&s, "X"));
        assert!(!parse::exactly_one(&s, "X"));
    }

    /// `VmFlags::from_env` snapshots `environ` once instead of calling `getenv`
    /// per field. The values must be identical to reading through `EnvSource`.
    #[test]
    fn environ_snapshot_matches_per_lookup_env_source() {
        let snapshot = MapSource::from_process_env();
        for (name, _) in std::env::vars_os() {
            let Ok(name) = name.into_string() else {
                continue;
            };
            assert_eq!(
                snapshot.get(&name),
                EnvSource.get(&name),
                "snapshot disagrees with var_os for {name}"
            );
        }
        // And the whole config built either way is the same shape.
        assert_eq!(
            format!("{:?}", VmFlags::from_source(&snapshot)),
            format!("{:?}", VmFlags::from_source(&EnvSource)),
        );
    }

    /// Nothing in `NativeFlags` is accidentally default-ON.
    #[test]
    fn native_flags_defaults_match_the_original_call_sites() {
        let f = VmFlags::from_source(&MapSource::empty());
        // Opt-in debug gates are off…
        assert!(!f.natives.dbg_loader_trace);
        assert!(!f.natives.dbg_h2trace);
        assert!(!f.natives.soft_exit);
        assert!(!f.natives.diag_serviceloader);
        assert!(!f.natives.real_proxy_strict);
        // …and every gate whose call site defaulted ON still does.
        assert!(f.natives.cl_bootstrap_scoped);
        assert!(f.natives.loader_unload);
        assert!(f.natives.uri_strict_chars);
        assert!(f.natives.real_stax_factory);
        assert!(f.natives.real_proxy);
        assert!(f.natives.real_proxy_super);
        assert!(f.natives.real_annotations);
        assert!(f.natives.msc_real_start);
        assert!(f.natives.inherit_thread_ccl);
        assert!(f.natives.inherit_tl_workaround);
        assert!(f.natives.native_string_regex);
        assert!(f.natives.native_matcher_find);
        assert!(f.natives.netty_queue_bridge);
        // Value flags fall back to None so the call site's default applies.
        assert_eq!(f.natives.http_max_body, None);
        assert_eq!(f.natives.async_submit_grace_ms, None);
        assert_eq!(f.natives.max_inflated_bytes, None);
    }

    // ── Test-support overrides ────────────────────────────────────────────
    //
    // The property under test in every case below is the one the whole
    // mechanism exists for: the override must win *after* the process
    // snapshot has already latched. Each of these deliberately touches
    // `flags()` first so the `OnceLock` is initialised before the override
    // is installed — reproducing the situation in which `set_var` silently
    // stops working.

    /// The scalar used throughout: declared (so it is served from the frozen
    /// snapshot rather than `std::env`), and inert — nothing in this crate
    /// changes behaviour based on it, so an override cannot perturb a
    /// concurrent test even in process scope.
    const PROBE: &str = "CRATONVM_MAVEN_REPO_LOCAL";

    fn probe() -> Option<String> {
        runtime_var(PROBE).ok()
    }

    #[test]
    fn thread_override_wins_after_the_snapshot_has_latched() {
        let _lock = process_override_lock();
        assert!(
            declared_flag_names().contains(PROBE),
            "probe must be declared"
        );
        let latched = probe();
        with_thread_overrides(&[(PROBE, Some("/scoped/repo"))], || {
            assert_eq!(probe().as_deref(), Some("/scoped/repo"));
        });
        assert_eq!(probe(), latched, "the guard must restore the snapshot");
    }

    #[test]
    fn thread_override_can_make_a_set_flag_look_absent() {
        let _lock = process_override_lock();
        with_thread_overrides(&[(PROBE, Some("/present"))], || {
            assert_eq!(probe().as_deref(), Some("/present"));
            // Nested, and this time removing it entirely.
            with_thread_overrides(&[(PROBE, None)], || {
                assert_eq!(probe(), None);
            });
            assert_eq!(
                probe().as_deref(),
                Some("/present"),
                "the inner guard must restore the OUTER override, not the snapshot"
            );
        });
    }

    #[test]
    fn thread_override_is_invisible_to_other_threads() {
        let _lock = process_override_lock();
        let outside = probe();
        with_thread_overrides(&[(PROBE, Some("/only/mine"))], || {
            assert_eq!(probe().as_deref(), Some("/only/mine"));
            let seen = std::thread::spawn(probe).join().expect("probe thread");
            assert_eq!(
                seen, outside,
                "a thread-scoped override must not leak to a spawned thread"
            );
        });
    }

    #[test]
    fn process_override_reaches_spawned_threads() {
        // Serialised against the other process-scoped test: `override_process`
        // is explicitly documented as requiring caller serialisation.
        let _lock = process_override_lock();
        let outside = probe();
        with_process_overrides(&[(PROBE, Some("/every/thread"))], || {
            let seen = std::thread::spawn(probe).join().expect("probe thread");
            assert_eq!(seen.as_deref(), Some("/every/thread"));
        });
        assert_eq!(probe(), outside, "the guard must restore the snapshot");
    }

    #[test]
    fn thread_scope_wins_over_process_scope() {
        let _lock = process_override_lock();
        with_process_overrides(&[(PROBE, Some("/process"))], || {
            assert_eq!(probe().as_deref(), Some("/process"));
            with_thread_overrides(&[(PROBE, Some("/thread"))], || {
                assert_eq!(probe().as_deref(), Some("/thread"));
            });
            assert_eq!(probe().as_deref(), Some("/process"));
        });
    }

    /// Dropping the last guard must take the fast path in [`flags`] back out
    /// of circulation — otherwise every later `flags()` call in the process
    /// pays for a thread-local probe forever.
    #[test]
    fn the_fast_path_gate_returns_to_zero() {
        let _lock = process_override_lock();
        assert_eq!(OVERRIDES_LIVE.load(Ordering::Relaxed), 0);
        {
            let _outer = override_process(VmFlags::from_env_with_edits(&[(PROBE, Some("a"))]));
            let _inner = override_thread(VmFlags::from_env_with_edits(&[(PROBE, Some("b"))]));
            assert_eq!(OVERRIDES_LIVE.load(Ordering::Relaxed), 2);
        }
        assert_eq!(OVERRIDES_LIVE.load(Ordering::Relaxed), 0);
    }

    /// An override must be a *complete* configuration, not a patch: every flag
    /// the caller did not name keeps the value it would have had.
    #[test]
    fn an_override_only_changes_the_named_flags() {
        let _lock = process_override_lock();
        let before = format!("{:?}", flags().jit);
        with_thread_overrides(&[(PROBE, Some("/repo"))], || {
            assert_eq!(format!("{:?}", flags().jit), before);
        });
    }

    /// Edits land before group resolution, so overriding a grouped expression
    /// reaches the legacy keys it expands to.
    #[test]
    fn edits_are_resolved_through_the_flag_groups() {
        let _lock = process_override_lock();
        with_thread_overrides(&[("CRATONVM_JIT", Some("threshold=7"))], || {
            assert_eq!(
                runtime_var("CRATONVM_JIT_THRESHOLD").ok().as_deref(),
                Some("7")
            );
        });
    }

    /// `resolve_capped_usize`, every branch.
    ///
    /// The branch that was the DEFECT is `too_big_is_clamped_not_defaulted`:
    /// `CRATONVM_NATIVE_SHADOW_SINK_CAP=200000` used to resolve to the DEFAULT
    /// (4096) rather than the ceiling (65,536), so an operator asking for more
    /// got an order of magnitude LESS than the rule allows, silently. The other
    /// four exist so that fixing it cannot quietly change the cases that were
    /// already right — a `0` cap in particular must never be honoured, because
    /// it would report an empty population as a complete one.
    const CAPVAR: &str = "CRATONVM_NATIVE_SHADOW_SINK_CAP";

    fn cap_with(value: Option<&str>) -> usize {
        with_thread_overrides(&[(CAPVAR, value)], || {
            resolve_capped_usize(CAPVAR, "test sink", 4096, 65_536)
        })
    }

    #[test]
    fn unset_resolves_to_the_default() {
        let _lock = process_override_lock();
        assert_eq!(cap_with(None), 4096);
    }

    #[test]
    fn an_in_range_value_is_honoured_exactly() {
        let _lock = process_override_lock();
        assert_eq!(cap_with(Some("9000")), 9000);
        assert_eq!(
            cap_with(Some("  9000  ")),
            9000,
            "surrounding space is trimmed"
        );
        assert_eq!(
            cap_with(Some("65536")),
            65_536,
            "the ceiling itself is in range"
        );
    }

    #[test]
    fn too_big_is_clamped_not_defaulted() {
        let _lock = process_override_lock();
        // THE BUG. This asserted 4096 for two months by construction.
        assert_eq!(
            cap_with(Some("200000")),
            65_536,
            "a value above the ceiling must CLAMP to the ceiling; falling back to \
             the default hands the operator LESS than the rule allows",
        );
    }

    #[test]
    fn zero_and_nonsense_keep_the_default() {
        let _lock = process_override_lock();
        assert_eq!(
            cap_with(Some("0")),
            4096,
            "a cap of zero reports empty as complete"
        );
        assert_eq!(cap_with(Some("banana")), 4096);
        assert_eq!(cap_with(Some("-1")), 4096, "usize::from_str rejects a sign");
        assert_eq!(cap_with(Some("")), 4096);
        assert_eq!(cap_with(Some("   ")), 4096);
    }

    /// The warnings must read as ONE SENTENCE.
    ///
    /// The first version of them shipped with 18-space gaps —
    /// `ceiling of 65536;                  CLAMPED to 65536` — because a patch
    /// script's Python `\`+newline joined the source lines and kept the
    /// indentation INSIDE the Rust literal. `cat -A` on the patched line passed,
    /// because the damage was in a string literal and not in a control
    /// character. It was found by running a six-minute LTO build and reading
    /// stderr. This is that check, in 0.03 s.
    #[test]
    fn the_warnings_read_as_one_sentence() {
        for raw in ["200000", "0", "banana"] {
            let (_, w) = capped_var_verdict(
                Some(raw),
                "CRATONVM_NATIVE_SHADOW_SINK_CAP",
                "interpreter observation sink",
                4096,
                65_536,
            );
            let w = w.expect("this input must warn");
            assert!(
                !w.contains("   "),
                "the warning for {raw:?} carries a run of 3+ spaces, so a line \
                 continuation was eaten: {w:?}",
            );
            assert!(!w.contains('\n'), "one line, not several: {w:?}");
            assert!(w.starts_with("[cratonvm] warning: "), "{w:?}");
            assert!(
                w.contains("CRATONVM_NATIVE_SHADOW_SINK_CAP"),
                "a warning that does not name the variable is not actionable: {w:?}",
            );
        }
    }

    /// A value that IS honoured must say nothing at all. A knob that warns on
    /// its own happy path trains the reader to ignore it.
    #[test]
    fn an_honoured_value_is_silent() {
        for raw in [
            None,
            Some("9000"),
            Some("65536"),
            Some("  9000  "),
            Some(""),
            Some("   "),
        ] {
            let (_, w) = capped_var_verdict(raw, "X", "sink", 4096, 65_536);
            assert!(w.is_none(), "{raw:?} must not warn, got {w:?}");
        }
    }

    /// The verdict and the printed value never disagree: the number in the
    /// message is the number returned.
    #[test]
    fn the_message_quotes_the_value_actually_in_force() {
        let (v, w) = capped_var_verdict(Some("200000"), "X", "sink", 4096, 65_536);
        assert_eq!(v, 65_536);
        assert!(w.unwrap().contains("CLAMPED to 65536"));
        let (v, w) = capped_var_verdict(Some("0"), "X", "sink", 4096, 65_536);
        assert_eq!(v, 4096);
        assert!(w.unwrap().contains("default 4096"));
    }

    /// The two sinks share ONE knob, so they must resolve it identically. This
    /// pins the property the shared helper exists to guarantee: before it, the
    /// same twelve lines were duplicated in `vm_exec.rs` and `jit/helpers.rs`
    /// and could drift.
    #[test]
    fn both_sinks_resolve_one_knob_the_same_way() {
        let _lock = process_override_lock();
        for v in ["200000", "0", "9000", "banana"] {
            let a = with_thread_overrides(&[(CAPVAR, Some(v))], || {
                resolve_capped_usize(CAPVAR, "interpreter observation sink", 4096, 65_536)
            });
            let b = with_thread_overrides(&[(CAPVAR, Some(v))], || {
                resolve_capped_usize(CAPVAR, "JIT fast-path violation sink", 4096, 65_536)
            });
            assert_eq!(a, b, "the two sinks disagreed about {v}");
        }
    }

    fn process_override_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }
}
