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
    ///
    /// # Every `CRATONVM_G1_*` boolean reads through here, as of 2026-09-20
    ///
    /// All twenty-three of them used [`present`] — presence is truth — which
    /// means `CRATONVM_G1_CARD_CLEAN=0` **enabled card cleaning**. That is not
    /// a documentation problem. It silently destroys the A/B measurement that
    /// the kill switches exist to make possible: an experimenter writes `=0`
    /// for the control arm, gets the treatment arm twice, and reads the
    /// difference between two identical configurations as an effect.
    ///
    /// It happened. Two separate measurements in the 2026-09-20 G1 round used
    /// `=0` as a control and had to be retracted — see
    /// `docs/feature-designs/concurrent-gc-maturation.md` §4 and
    /// §7. The second one was caught only because a lane printed the flag's
    /// effective value beside the result and noticed `enabled=true` on both
    /// arms, which is the general lesson: a measurement should read back what
    /// the switch actually did, not what the command line asked for.
    ///
    /// Switching them costs nothing anyone relied on. Unset still means off,
    /// which is what every default was and still is; the only behaviour that
    /// changed is `=0`, from "on" to "off", which is the reading every one of
    /// these flags' own doc comments already claimed.
    ///
    /// Flags outside the `CRATONVM_G1_*` family were deliberately left alone.
    /// The same trap is there — see the `present` call sites — but changing a
    /// collector's controls is the owning round's job, not a drive-by.
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
    /// defence-in-depth deployment profiles. The value rule is shared with
    /// [`super::runtime_flag_on`], so the two cannot drift apart.
    #[inline]
    pub fn truthy_word(src: &dyn FlagSource, name: &str) -> bool {
        match utf8(src, name) {
            Some(v) => super::word_is_on(&v),
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
    /// Not set, empty, `0`, `false`, `off` or `no` (the `truthy_word` off
    /// words, since gci i4/fl).
    #[default]
    Off,
    /// Exactly `warn`.
    Warn,
    /// Any other value.
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

/// The DEFAULT of every allocation-path policy arm, in one place (gen
/// r4w4/alloc4, 2026-09-24).
///
/// The generational rounds landed these arms opt-in and handed the default
/// decision to an A/B measurement. Flipping one is this one line — the parser
/// (`alloc_policy_switch`, private to this file) and every test that asserts a
/// default read the constant. The flag's row in `flag_groups.rs` carries `off_word: Some("0")`
/// for a default-ON arm, which only the generated flag documentation reads;
/// set it together with the constant so the rendered inventory says what the
/// code does. `=0` / `false` / `off` / `no` turn any of these off, and any
/// other set value turns it on, whichever way the default points.
///
/// # Per-backend defaults (gen r5w4/defaults8, 2026-09-26)
///
/// A switch whose A/B was won on ONE backend gets a `BackendDefault`
/// instead of a `bool`: its `GcFlags` field is an `Option<bool>` (`None` =
/// unset or empty, parsed by `backend_policy_switch`), and the heap resolves
/// it per VM with `BackendDefault::resolve` where the VM already knows which
/// heap it built (`VmHeap::tlab_share_sizer_enabled`,
/// `ReferenceProcessor::new_for_heap`). An explicit value wins on every
/// backend, so `=1` still opts G1 and ZGC in and `=0` still opts the
/// Generational heap out. No process global is involved: the flag snapshot
/// is the same one every other arm reads, and the backend is the VM's own.
/// The row's `off_word` is `Some("0")` (the minus token must write an
/// explicit off, or the Generational default would survive it).
pub mod alloc_policy_defaults {
    /// A default that depends on the heap the VM built: `generational` on the
    /// Generational backend (`-XX:+UseGenerationalGC`), `other` on G1 and ZGC.
    /// See the module doc's "Per-backend defaults".
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct BackendDefault {
        /// The default on the Generational heap.
        pub generational: bool,
        /// The default on every other backend (G1, ZGC).
        pub other: bool,
    }

    impl BackendDefault {
        /// The switch's value on a heap of this family: the explicit setting
        /// (`Some`) wherever it was given, else this default for the family.
        /// `generational` is `VmHeap::is_generational()` of the VM asking; a
        /// caller with no heap in hand passes `false` (the non-Generational
        /// default), which is what every backend did before a per-backend
        /// default existed.
        #[inline]
        pub const fn resolve(self, explicit: Option<bool>, generational: bool) -> bool {
            match explicit {
                Some(on) => on,
                None if generational => self.generational,
                None => self.other,
            }
        }
    }

    /// `CRATONVM_TLAB_FILLER_SKIP_ZERO` — a Java thread's retire skips the
    /// tail filler's data memset.
    pub const TLAB_FILLER_SKIP_ZERO: bool = false;
    /// `CRATONVM_TLAB_GATE_BUMP_FLOOR` — the JIT refill gate probes the young
    /// bump tail for the fragmentation floor.
    pub const TLAB_GATE_BUMP_FLOOR: bool = false;
    /// `CRATONVM_TLAB_SHARE_SIZER` — the HotSpot-shaped per-thread TLAB sizer
    /// (`Tlab::share_refill_request`): desired size from the thread's
    /// exponentially averaged share of young allocation per collection, plus
    /// the refill-waste limit. Supersedes the ladder's retired arms
    /// (`CRATONVM_TLAB_SIZE_RETIRED` / `_WASTE_SHRINK`, removed by gce e2/o).
    ///
    /// Default ON for the Generational heap since gcg g6/jn (vg5-jn C, 3
    /// interleaved pairs: `GenR4W2ParkAllocProbe 8 20000 64` 82-88 -> 37
    /// minors, `GenR4W4SkewedShareProbe` 116 -> 113, `GenR4W6AllocRateProbe`
    /// wall a tie; `GenR5W3ConcUnloadProbe` and `GenR4W4NativeStringOomProbe
    /// --nojit` correct 5/5), OFF on G1 and ZGC. History: defaults8 (gen r5w4)
    /// turned it ON for the
    /// Generational heap on the evidence below, and the orchestrator REVERTED
    /// that before merging: on the wave-4 build, with the sizer on (and the
    /// tail sink on, its default), `GenR5W3ConcUnloadProbe` (JIT on) reads a
    /// static field it only ever sets to a live object back as NULL, and
    /// `GenR4W4NativeStringOomProbe --nojit` fails its recovery step; both pass
    /// with `=0`. Live data is being handed out as a TLAB
    /// (`docs/feature-designs/generative-gc-architecture.md`).
    /// That page is fixed, and gcg g6/jn flipped `generational` on (park 37 vs
    /// 82-88 minors on vg5, 38 vs 79 on vg6). Evidence for the original flip —
    /// the orchestrator's protocol A of
    /// `docs/feature-designs/generative-gc-architecture.md` on the
    /// wave-3 merge `897210f83`, Linux release, `-XX:+UseGenerationalGC`,
    /// interleaved reps: W1 `GenR4W2ParkAllocProbe 8 20000 64` 610 -> 63
    /// minors; W4 `GenR4W4SkewedShareProbe` 108 -> ~69 minors; W5
    /// `GenR4W6AllocRateProbe` multi and parked phases +60 %, single -9 %;
    /// the sizer with the tail sink (S+K) was the best arm on W1 and W4; every
    /// correctness line unchanged. The epoch it samples at is one per young
    /// pause (`VmHeap::young_pause_epoch`). `=0` restores the ladder on the
    /// Generational heap; `=1` still arms it on G1 and ZGC. gci i48/zl3: G1
    /// takes the Generational default ([`TLAB_SHARE_SIZER_G1`]); `other` is
    /// ZGC's.
    pub const TLAB_SHARE_SIZER: BackendDefault = BackendDefault {
        generational: true,
        other: false,
    };
    /// gci i48/zl3 (`zx48-collector-scope-audit.md` A7) --
    /// `CRATONVM_TLAB_SHARE_SIZER`'s default on the G1 heap
    /// (`BackendDefault::other` stays ZGC's: its VM TLABs are sized by the
    /// arena's own share, `CRATONVM_ZGC_VM_TLAB_SHARE`). Resolved by
    /// [`super::GcFlags::tlab_share_sizer_for_backend`].
    ///
    /// Default ON on G1, the Generational default: HotSpot 25 sizes every
    /// collector's TLABs with `ThreadLocalAllocBuffer::resize` (G1 included).
    /// `=0` restores the ladder on G1 as on the Generational heap; an explicit
    /// value wins on every backend.
    ///
    /// Why the G1 arm cannot hand out live memory (the Generational history:
    /// `gengc-r5w4-orch-share-sizer-hands-out-live-memory-FIXED-20260928.md`,
    /// a young-to-old marking gap the sizer's geometry exposed, not the
    /// sizer): the sizer chooses only the SIZE of a request. A G1 chunk is a
    /// compare-exchange claim of `[top, top + size)` on an Eden region's
    /// monotonic cursor (`G1Collector::carve_tlab_from_eden_slot`), clamped to
    /// half a region (`CRATONVM_G1_TLAB_CLAMP`), so no size can overlap a live
    /// object or another thread's chunk. G1 has no tail sink (the
    /// Generational cursor retraction, `CRATONVM_GEN_TLAB_TAIL_SINK`): a
    /// retired tail stays under its own thread's filler. A refill-waste keep
    /// (`Tlab::keep_on_miss`) sends the one object to G1's per-object Eden
    /// claim (`try_alloc_object`, the same cursor), only while
    /// `young_bump_headroom` (`!needs_gc()`) holds. Windows close on every G1
    /// pause (`VmHeap::young_pause_epoch`), as HotSpot samples at every GC.
    pub const TLAB_SHARE_SIZER_G1: bool = true;
    /// `CRATONVM_GEN_ZERO_ONCE` — a young hand-out carved from the bump tail
    /// is not zeroed again: the young wipe (or a fresh commit) is its one
    /// zeroing point. `=0` restores the refill memset.
    pub const GEN_ZERO_ONCE: bool = true;
    /// `CRATONVM_GEN_HUMONGOUS_ZERO_UNLOCKED` — a humongous PRIMITIVE array's
    /// body is zeroed after the old-generation lock is released. `=0` zeroes
    /// it under the lock, as before.
    pub const GEN_HUMONGOUS_ZERO_UNLOCKED: bool = true;
    /// `CRATONVM_GEN_TLAB_TAIL_SINK` — gen r4w6/tlab6: a retiring TLAB's unused
    /// tail goes back to the generational young from-space (the bump cursor
    /// retracts, or the tail joins the young free list) instead of being
    /// buried under an `int[]` filler the young trigger counts as occupied.
    ///
    /// Default ON since gen r5w2 (orchestrator A/B, 2026-09-27, protocol A of
    /// `docs/feature-designs/generative-gc-architecture.md`, 3 interleaved
    /// reps): `GenR4W2ParkAllocProbe 8 20000 64` 6.4 s / 610 minors -> 3.5 s /
    /// ~40; `GenR4W4SkewedShareProbe` 108 -> ~60 minors; `GenR4W6AllocRateProbe`
    /// multi and parked phases +60 %, single -8 %; every correctness line and
    /// `filler_over_object=0` on every run. `=0` restores the filler.
    pub const GEN_TLAB_TAIL_SINK: bool = true;
    /// `CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED` — gen r5w1/oldgen5: a
    /// humongous REFERENCE array's body is zeroed after the old-generation
    /// lock is released, behind a `long[]` disguise of the same footprint.
    /// Default ON since gcg g5 (vg4 `og4_t6_*`: 8 threads +33 % median
    /// paired, 5/5 pairs; 4 threads no regression); `=0` restores the
    /// locked memset.
    pub const GEN_HUMONGOUS_REF_ZERO_UNLOCKED: bool = true;
    /// `CRATONVM_SOFTREF_HOTSPOT_LRU` — gen r5w1/refs5: the SoftReference LRU
    /// policy takes HotSpot's `LRUMaxHeapPolicy` inputs (the soft clock and the
    /// free heap as of the END of the previous collection). Not an
    /// allocation-path arm; kept here because it is flipped on the same kind
    /// of evidence and resolved the same per-backend way.
    ///
    /// Default ON on the Generational heap since gen r5w4/defaults8, OFF on G1
    /// and ZGC in this constant. The VM builds its processor with the
    /// Generational default on other backends too: on G1 since gci i5g/g1o
    /// (`vm_init::soft_lru_takes_hotspot_inputs_by_default`), and on EVERY
    /// backend since jr20 wave 9 (lane gcE, `vm_init.rs`): G1 never cleared an
    /// idle soft reference and ZGC cleared a hot one on the wall-clock inputs
    /// (vh4-g1 block E: `g14_softlru_g1_hs_*` SAME with `=1`). The `other` arm
    /// is what a processor built with no heap in hand (unit tests) latches.
    /// Evidence (orchestrator, wave-3 merge `897210f83`, Linux
    /// release): `-XX:+UseGenerationalGC -Xmx32m GenR5W2SoftRefIdleYoungProbe`
    /// prints `lost=0` `PASS` with the flag — HotSpot Serial's lines — and
    /// `lost=2` `FAIL` without it. Latched per VM by
    /// `ReferenceProcessor::new_for_heap`. `=0` restores the wall-clock inputs
    /// on the Generational heap; `=1` still selects HotSpot's on G1 and ZGC.
    pub const SOFTREF_HOTSPOT_LRU: BackendDefault = BackendDefault {
        generational: true,
        other: false,
    };
    /// zr z1/rf — `CRATONVM_SOFTREF_HOTSPOT_LRU`'s default on the ZGC heap
    /// (`BackendDefault::other` stays G1's: G1 is out of this lane's scope).
    ///
    /// Default ON on ZGC since zr z1/rf
    /// (`docs/feature-designs/zgc-architecture.md`,
    /// ZGC half). On the wall-clock inputs a ZGC run under discovery at
    /// allocation never advanced `SoftReference.clock` (only the HotSpot mode
    /// commits `soft_gc_clock_ms`, so `publish_soft_reference_clock` gets `0`
    /// and returns), every bytecode `get()` copied the same stale `0`, no row
    /// was ever refreshed, and a soft reference read every 2 ms was condemned
    /// as idle since its construction (`ak1_softlru_zgc`: `hot kept: false`).
    /// ZGC's default collection is the same stop-the-world pre / post
    /// protocol the Generational heap runs (`weakref_null_referents_pre_gc`,
    /// `process_references_after_gc`), where HotSpot's inputs are measured to
    /// match. `CRATONVM_SOFTREF_HOTSPOT_LRU=0` restores the wall-clock inputs
    /// on ZGC (and on the Generational heap); an explicit value wins on every
    /// backend, as before.
    pub const SOFTREF_HOTSPOT_LRU_ZGC: bool = true;
    /// `CRATONVM_GEN_XMS_USABLE_FIRST` — gen r5w3/oldgen7: the `-Xms` startup
    /// commit is split the way HotSpot Serial sizes its initial heap (one
    /// survivor of copy reserve in the to-space, the rest usable). Generational
    /// only, so a plain `bool`.
    ///
    /// Default ON since gen r5w4/defaults8. Evidence (orchestrator, wave-3
    /// merge `897210f83`, Linux release): `-XX:+UseGenerationalGC -Xms64m
    /// -Xmx512m GenR5W3XmsTotalProbe 64` prints `PASS` (= HotSpot Serial) with
    /// the flag and `FAIL` without it. The page's second flip criterion (the
    /// first young pause's time at a `-Xms`-sized startup, within noise of the
    /// even split) was not measured; it is listed as the run to take in
    /// `docs/feature-designs/generative-gc-architecture.md`. `=0`
    /// restores the even split.
    pub const GEN_XMS_USABLE_FIRST: bool = true;
}

/// Parse a switch whose default is a [`alloc_policy_defaults::BackendDefault`]:
/// `None` when unset, not UTF-8, or empty after trimming (the three readings
/// that keep a default, as [`alloc_policy_switch`] keeps it for the empty
/// string), else `Some` of the [`parse::truthy_word`] rule — off for `0` /
/// `false` / `off` / `no`, on for anything else. gen r5w4/defaults8.
fn backend_policy_switch(src: &dyn FlagSource, name: &str) -> Option<bool> {
    parse::utf8(src, name)
        .filter(|v| !v.trim().is_empty())
        .map(|v| word_is_on(&v))
}

/// Parse one allocation-policy switch against its default from
/// [`alloc_policy_defaults`]: unset → `default_on`; set → the
/// [`parse::truthy_word`] reading (off for `0` / `false` / `off` / `no`) when
/// the default is off, the [`parse::truthy_word_default_true`] reading when
/// it is on — so the two spellings agree on every set value except the empty
/// string, which keeps the default.
fn alloc_policy_switch(src: &dyn FlagSource, name: &str, default_on: bool) -> bool {
    if default_on {
        parse::truthy_word_default_true(src, name)
    } else {
        parse::truthy_word(src, name)
    }
}

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
    /// `CRATONVM_GC_CARD_SUMMARY` — gen r4w3/cards3 (2026-09-23): the
    /// generational card table keeps a one-byte-per-64-cards SUMMARY map and
    /// its collector-side scans (`take_dirty_cards`, `clear_all`,
    /// `dirty_card_indices`) read card bytes only inside summary groups that
    /// hold something. Default **ON** ([`parse::on_unless_zero`]); `=0` builds
    /// every card table without a summary, so the scans walk the byte map up
    /// to the scan bound exactly as before. The A/B lever for
    /// `gc/src/card_table.rs::SUMMARY_GROUP`; read once per card table, at
    /// construction.
    pub gc_card_summary: bool,
    /// `CRATONVM_GC_OLD_BOT` — gen r4w3/cards3 (2026-09-23): the generational
    /// old generation keeps a BLOCK-OFFSET TABLE (one anchor per card), and
    /// the young pause's dirty-card object walk starts each dirty range at its
    /// card's anchor instead of decoding every old-gen header from offset 0.
    /// Default **ON** ([`parse::on_unless_zero`]); `=0` restores the linear
    /// walk (`OldGen::walk_objects_in_card_ranges`). Read once per `OldGen`,
    /// at construction.
    pub gc_old_bot: bool,
    /// `CRATONVM_GC_PRECISE_ARRAY_CARDS` — gen r4w3/cards3 (2026-09-23): a
    /// reference store into an old-generation reference array through
    /// `GenerationalHeap::set_array_element` dirties the card of the written
    /// ELEMENT instead of the array's header card, and the dirty-card scan
    /// reads only the elements inside a dirty card of an array whose header
    /// card is clean. Default **ON** ([`parse::on_unless_zero`]); `=0` keeps
    /// the header-card contract (one store into a million-element array
    /// re-scans all million elements). Read once per card table, at
    /// construction.
    pub gc_precise_array_cards: bool,
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
    /// `CRATONVM_G1_SERIAL_EVAC_HOLDER_SCREEN` (default ON) -- the same
    /// word0-is-an-arena-pointer holder refusal on the SERIAL evacuator's
    /// ref scan. Its walk rewrites the cells it visits and is bounded by the
    /// REGION, not by the holder, so a bad header rewrites the objects that
    /// follow it. Set to 0 to ablate.
    pub g1_serial_evac_holder_screen: bool,
    /// `CRATONVM_G1_VERIFY_FORWARDS_RETIRED` -- count objects still FORWARDED
    /// after `retire_forwards`. Diagnostic, opt-in, counts only.
    pub g1_verify_forwards_retired: bool,
    /// `CRATONVM_G1_EVAC_COPY_WATCH` — record every to-space copy's first
    /// header word as it is made, and re-read them at two checkpoints inside
    /// the pause. **OPT-IN** ([`parse::non_empty_non_zero`]): a push per copy
    /// under one lock.
    ///
    /// It answers one question and it is the question
    /// `g1-eight-byte-write-at-a-live-objects-base-20260906` could only infer:
    /// the corrupt holders that walk reports find are to-space copies, and
    /// whether they were sound WHEN COPIED decides whether the writer is inside
    /// the pause. Checkpoint 1 closes the parallel closure, checkpoint 2 the
    /// serial self-forward drain.
    pub g1_evac_copy_watch: bool,
    /// `CRATONVM_G1_REF_WRITE_WATCH` — record every REFERENCE-SLOT WRITE the
    /// collector makes during a pause, keyed by slot address, and name the
    /// writer when a later walk finds that slot corrupt. **OPT-IN**
    /// ([`parse::non_empty_non_zero`]): a hash-map insert per rewritten slot
    /// under one lock, so it is a debug instrument and not a production guard.
    ///
    /// It answers the question
    /// `g1-eight-byte-write-at-a-live-objects-base-20260906` names as its next
    /// step and could not answer from the READ side: the corrupt cells it
    /// measured are an EIGHT-byte quantity sitting where a SIXTEEN-byte cell's
    /// first word belongs, and the two candidate producers (an 8-byte
    /// reference write into a legacy cell, versus a read misaligned by 8 into
    /// a forwarded object's mark word) are distinguished only by whether a
    /// collector write actually landed on that slot. [`CopyWatch`] cannot see
    /// it: it records COPIES, and a write into a resident object's body is not
    /// one.
    pub g1_ref_write_watch: bool,
    /// `CRATONVM_G1_EVAC_SUPPLY_SCREEN` (default **ON**) -- refuse an
    /// evacuation supply address that is not the start of an object, on the
    /// three routes that had no screen of their own: the marking keep-alive
    /// set, the evacuation-failure drain's seeds, and Phase 3.5's finalizer
    /// resurrection.
    ///
    /// See `G1Collector::evacuation_supply_is_an_object` for what an
    /// unscreened route costs: `evacuate_object` sizes, copies and installs a
    /// forwarding mark word against whatever bytes it is handed, so an
    /// INTERIOR address produces both families
    /// `g1-eight-byte-write-at-a-live-objects-base-20260906` measured -- a
    /// to-space copy whose first word is an arena pointer, and an eight-byte
    /// `target | MARK_FORWARDED` written inside a live object's body.
    ///
    /// `=0` restores the unscreened behaviour, which is the same-binary A/B.
    pub g1_evac_supply_screen: bool,
    /// `CRATONVM_G1_EVAC_SHARE_CENSUS` — [`parse::non_empty_non_zero`], default
    /// **OFF**. Lane W7-W (2026-09-21).
    ///
    /// Publish the two DISTRIBUTIONS the parallel evacuator's load-balancing
    /// valve is a function of: how many children one `process_object` pushed,
    /// and how deep the pushing worker's LOCAL stack got. `g1-evac-census`
    /// counts `spills` — how often the valve opened — but nothing counted how
    /// close it came to opening, so a workload that never spills and a
    /// workload that reaches 255 on every push produce identical output, and
    /// the question `w6p-work-sharing-engages-on-child-list-width-not-graph-width.md`
    /// §3 leaves open ("do real Java graphs sit below the 256 line?") had no
    /// instrument that could answer it.
    ///
    /// Opt-in, not default-on. `g1-evac-census` earned its default with an
    /// A/B; this has none, and the rule is that a lever defaults to today's
    /// behaviour until a measurement says otherwise. Armed, it is two `max`
    /// updates and two array increments per scanned object on WORKER-LOCAL
    /// state, flushed to the process-global table once per worker per pause.
    pub g1_evac_share_census: bool,
    /// `CRATONVM_G1_EVAC_CANDIDATE_ARENA_SCREEN` (default **ON**) -- refuse a
    /// reference-slot evacuation candidate whose `class_id`/`shape` dword pair
    /// recombines to a pointer into the collector's own arena.
    ///
    /// Split out of `g1_evac_ref_implausible_refuse`, which stays opt-in. That
    /// flag's opt-in status was decided against the CLASS-ID BAND test -- an
    /// assumption about which ids a loader mints, which had already gone stale
    /// once -- and the arena test was folded into the same flag afterwards.
    /// They are not the same kind of claim: `class_id`(4) + `shape`(4) IS the
    /// header's first eight bytes, so a pair landing inside this arena is a
    /// BODY word read at an address that is not an object start, and no future
    /// class-id space can make it a header. The same test already refuses by
    /// default on the serial evacuator's holders and on the Phase-4 fixup's
    /// (`CRATONVM_G1_SERIAL_EVAC_HOLDER_SCREEN`).
    pub g1_evac_candidate_arena_screen: bool,
    /// `CRATONVM_G1_EVAC_EMPTY_HEADER_GRID_PROOF` (default **ON**) -- when a
    /// candidate's header reads `class_id = 0, num_slots = 0, kind = Object`,
    /// prove it against the region's object grid before following it.
    ///
    /// That sixteen-byte shape is what the PAYLOAD WORD of a null `Value` cell
    /// reads as, and it is a valid zero-field object, so no header screen can
    /// reject it. See `G1Collector::empty_header_is_a_real_object`: following
    /// one installs a forwarding mark word at `addr + 8`, which is the next
    /// cell's discriminant word inside a live object -- the corrupt-cell family
    /// of `g1-eight-byte-write-at-a-live-objects-base-20260906`, bit for bit.
    ///
    /// The proof is an O(region) walk, paid only on that shape. `=0` stands it
    /// down, which is the same-binary A/B.
    pub g1_evac_empty_header_grid_proof: bool,
    /// `CRATONVM_G1_PARALLEL_EVAC_SHARED_DEST` — when the parallel evacuator's
    /// reserved Free pool runs out, place the object in an EXISTING non-CSet
    /// region of the destination type instead of failing the evacuation.
    /// Default **ON** ([`parse::on_unless_zero`]); `=0` restores the pool-only
    /// behaviour.
    ///
    /// The serial evacuator has always done this — `alloc_in_type_locked_scan`
    /// scans every non-CSet region of the target type before claiming a Free
    /// one — so this is a parity fix, not a new policy. MEASURED 2026-09-06:
    /// the parallel arm declared to-space exhaustion for a 48-byte promotion
    /// with 2017 of 2025 non-CSet Old regions still holding room, and every
    /// such object became a self-forward, a kept CSet region and a
    /// `retry_after_evacuation_failure` pass.
    pub g1_parallel_evac_shared_dest: bool,
    /// `CRATONVM_G1_PARALLEL_EVAC_RESUME_DEST` — a promotion TLAB takes up an
    /// OLD region a previous pause left partly filled before it consumes a
    /// fresh Free one. Default **ON** ([`parse::on_unless_zero`]); `=0`
    /// restores the pool-only claim.
    ///
    /// Not a tuning knob: without it the old generation grows by ONE REGION
    /// PER EVACUATION WORKER PER YOUNG PAUSE however little is promoted,
    /// because each worker's Old TLAB took a whole fresh Free region every
    /// pause and abandoned it partly filled. MEASURED 2026-09-06 on the Tomcat
    /// `TestHostConfigAutomaticDeploymentXmlExternalWarXml` class at `-Xmx2g`,
    /// 32 CPUs (23 workers), one binary with the arms interleaved: the modal
    /// per-pause Old growth is +23 on the default arm, +4 under
    /// `CRATONVM_G1_WORKERS=4`, and +1 with `CRATONVM_G1_PARALLEL_EVAC=0` —
    /// the quantum IS the worker count. Old peaked at 2026 of 2048 regions
    /// (OOM) against 297 and 30 (both PASS).
    ///
    /// The serial evacuator never had the defect: `alloc_in_type_locked_scan`
    /// scans every existing non-CSet region of the destination type before
    /// claiming a Free one. This is that behaviour at TLAB granularity, and
    /// is a parity fix rather than a new policy — the same relationship
    /// `g1_parallel_evac_shared_dest` has to the same serial function.
    pub g1_parallel_evac_resume_dest: bool,
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
    /// frees it. **Opt-in** ([`parse::non_empty_non_zero`]); off means the allocator's own
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
    /// maintains. Opt-in ([`parse::non_empty_non_zero`]).
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
    /// `CRATONVM_G1_FULL_COMPACTION` — end `System.gc()` and the G1 last-ditch
    /// and overhead-limit rungs with one full-compaction pause after their
    /// marking cycle: every movable Old region with mark data and garbage
    /// worth taking joins one mixed CSet, uncapped (gcd d10/o). Default **ON**
    /// ([`parse::on_unless_zero`]); `=0` restores the cycle-only collection,
    /// which leaves a region with a few live bytes beside a megabyte of
    /// garbage in place until the mixed phase reaches it.
    pub g1_full_compaction: bool,
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
    /// ([`parse::non_empty_non_zero`]); requires `CRATONVM_G1_RESERVE_HEAP` (the default).
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
    /// # OFF, then ON again — the whole arc, because the middle of it is a
    /// record of the fault this flag exists to make visible
    ///
    /// **2026-09-05, flipped ON** on a 90/0 HotSpot-differential regression
    /// suite plus a sequential-ABBA timing that measured no cost.
    ///
    /// **2026-09-06 morning, reverted to opt-in.** `-XX:+UseGenerationalGC`
    /// over the H2 JDBC corpus SIGSEGV'd in **ten seconds** with it on and
    /// completed with it off. One binary, five arms,
    /// `org.h2.test.jdbc.TestPreparedStatement` alone at `--Xmx 512m`:
    ///
    /// | arm | rc |
    /// |---|---|
    /// | this flag ON | 139, fault addr in a `site=unbumped-middle` span |
    /// | `CRATONVM_GEN_UNCOMMIT=0` | 0 |
    /// | `CRATONVM_GC_RESERVE=0` (nothing decommits) | 0 |
    /// | `--nojit` | 0 |
    ///
    /// The third and fourth rows are the ones that name the defect rather than
    /// the switch: with the reservation kept mapped, the same stale reference
    /// reads stale bytes instead of faulting, and with no JIT there is no
    /// stale reference at all. The flag was behaving exactly as documented.
    ///
    /// **2026-09-06 afternoon, the defect was found and fixed** — the
    /// cross-thread coverage handshake was discharging a peer thread's
    /// compiled frames by PINNING their conservative roots, on a collector
    /// that cannot honour a pin (Cheney reclaims from-space wholesale, so
    /// `gen_heap.rs` has no reader of `pinned_jit_roots_snapshot()` and could
    /// not acquire one). See [`VmHeap::honours_conservative_pins`].
    ///
    /// **2026-09-06, back ON by default**, which is where it is now.
    ///
    /// # Why ON, given the same measurement that argued against it
    ///
    /// Concurrent paired arms put this switch at 0.942 / 1.015 / 1.042 / 1.070
    /// — free to within noise. That was read once as "buys nothing"; it is the
    /// wrong reading, and the flip corrects it. **Returning the memory IS the
    /// benefit.** Being wall-clock-neutral while doing it is the property that
    /// makes the give-back shippable, not an argument against shipping it.
    ///
    /// The number that was missing is what it hands back on a REAL workload
    /// rather than a probe. Measured 2026-09-06 with the default on, from the
    /// shutdown census (`[cratonvm] generational young uncommit: N bytes`):
    ///
    /// | workload | returned |
    /// |---|---|
    /// | `DodH2JdbcSuite`, one class, `--Xmx 512m` | **220 MB** |
    /// | `DodH2JdbcSuite`, full, `--Xmx 1g` | **369 MB** |
    /// | Spring Boot + Tomcat over TLS, `--Xmx 1g` | **136 MB** |
    ///
    /// against the 30 MiB of a 96 MB heap the probe had reported. A JVM that
    /// peaks and then idles held its peak for the life of the process before
    /// this, and the generational collector was the only backend of which that
    /// was true — ZGC gives memory back by default and G1 on request.
    ///
    /// # What is still true, and is the first thing to reach for
    ///
    /// The cost stated at the original flip has not changed: this collector
    /// publishes its young arenas' FULL reserved range into
    /// `JIT_REGION_BOUNDS` and `JIT_READ_BOUNDS`, and a decommitted granule
    /// FAULTS on touch rather than reading as zero. The window is not created
    /// here — the young arenas already commit lazily while the published bound
    /// covers the whole reservation — but it is WIDENED, from "granules never
    /// yet allocated into" to "granules that held objects one collection ago".
    ///
    /// So any stale young reference that still exists anywhere becomes a
    /// SIGSEGV rather than a silent stale read. **`CRATONVM_GEN_UNCOMMIT=0` is
    /// the first thing to set if a compiled frame ever faults on a young
    /// address**, and the crash handler prints the released span, the site and
    /// the code buffer to make that attribution immediate.
    ///
    /// That loudness is a feature and it has already earned its keep once: it
    /// is what turned a silent compiled-frame use-after-free — invisible to a
    /// 90/0 regression suite — into a ten-second reproducer with a five-arm
    /// attribution. Note also that withdrawing `JIT_READ_BOUNDS`, the fix
    /// finding 7 applied to ZGC for the identical shape, does NOT close this
    /// window: `emit_trusted_oop_receiver_check_at` dereferences the receiver
    /// consulting no bounds table at all.
    ///
    /// See the retired cross-collector page's finding 7 for where the give-back
    /// was built, and the retired stale-compiled-frame-reference record for the
    /// defect this exposed and how it was closed.
    pub gen_uncommit: bool,
    /// `CRATONVM_TLAB_FILLER_SKIP_ZERO` — opt-in (gen r4w3/alloc3,
    /// 2026-09-23): a Java thread's TLAB retire writes the `int[]` tail
    /// filler's header but skips the memset of its data area, which the
    /// zeroed-at-carve contract already holds at zero, unless the tail
    /// tripwire read a non-zero word. [`parse::truthy_word`]. Latched per
    /// carved TLAB.
    pub tlab_filler_skip_zero: bool,
    /// `CRATONVM_TLAB_GATE_BUMP_FLOOR` — opt-in (gen r4w3/alloc3,
    /// 2026-09-23): the JIT TLAB-refill gate (`tlab_alloc_shaped_inner`)
    /// probes the young bump tail for the fragmentation floor (at least the
    /// object in hand) instead of the full request, so `refill_tlab`'s
    /// bump-tail mini-TLAB is reachable from the gate. [`parse::truthy_word`].
    pub tlab_gate_bump_floor: bool,
    /// `CRATONVM_TLAB_SHARE_SIZER` — gen r4w4/alloc4: the HotSpot-shaped
    /// per-thread TLAB sizer and refill-waste limit.
    ///
    /// gen r5w4/defaults8: the EXPLICIT setting only — `None` when unset. Its
    /// default is per backend ([`alloc_policy_defaults::TLAB_SHARE_SIZER`]:
    /// ON on the Generational heap since gcg g6/jn flipped `generational` on
    /// (park 37 vs 82-88 minors on vg5, 38 vs 79 on vg6), OFF on ZGC; ON on
    /// G1 since gci i48/zl3, [`alloc_policy_defaults::TLAB_SHARE_SIZER_G1`]),
    /// so read it through
    /// [`GcFlags::tlab_share_sizer_for_backend`] with the VM's heap
    /// (`VmHeap::tlab_share_sizer_enabled`), never as a bare flag. Armed per
    /// carved TLAB by the refill site.
    pub tlab_share_sizer: Option<bool>,
    /// `CRATONVM_GEN_ZERO_ONCE` — gen r4w4/alloc4: a young hand-out carved
    /// from the bump tail is not zeroed again (the young wipe or a fresh
    /// commit already did). Default ON ([`alloc_policy_defaults::GEN_ZERO_ONCE`]);
    /// `=0` restores the refill / slow-path memset.
    pub gen_zero_once: bool,
    /// `CRATONVM_GEN_ZERO_ONCE_FREE_LIST` — gce e1/y: a free-list hand-out
    /// wholly inside a span the last non-moving sweep zeroed
    /// (`Arena::hand_out_is_known_zero`) takes the zero-once skip too, with
    /// the same sample check and repair. Default ON since gcf 2026-10-01
    /// ([`parse::on_unless_zero`]; `=0` restores the memset).
    pub gen_zero_once_free_list: bool,
    /// `CRATONVM_GEN_HUMONGOUS_ZERO_UNLOCKED` — gen r4w4/alloc4: a humongous
    /// primitive array's body is zeroed after the old-generation lock drops.
    /// Default ON ([`alloc_policy_defaults::GEN_HUMONGOUS_ZERO_UNLOCKED`]);
    /// `=0` zeroes under the lock.
    pub gen_humongous_zero_unlocked: bool,
    /// `CRATONVM_GEN_TLAB_TAIL_SINK` — gen r4w6/tlab6: the generational heap
    /// takes a retiring TLAB's unused tail back (cursor retraction, or the
    /// young free list) instead of the retire burying it under a filler.
    /// Default ON since gen r5w2 ([`alloc_policy_defaults::GEN_TLAB_TAIL_SINK`],
    /// `=0` is the opt-out); latched per heap
    /// (`GenerationalHeap::tlab_tail_sink_enabled`).
    pub gen_tlab_tail_sink: bool,
    /// `CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED` — gen r5w1/oldgen5: a
    /// humongous REFERENCE array's body is zeroed after the old-generation
    /// lock drops, behind a same-footprint `long[]` header until it is zero.
    /// Default ON since gcg g5 ([`alloc_policy_defaults::GEN_HUMONGOUS_REF_ZERO_UNLOCKED`]);
    /// `=0` is the opt-out.
    pub gen_humongous_ref_zero_unlocked: bool,
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
    /// **Opt-in** ([`parse::non_empty_non_zero`]), and the measurement below is why.
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
    /// `CRATONVM_G1_RETIRE_FORWARDS_LATE` — on unless `0`. Retire the pause's
    /// from-space forwarding tags between Phase 4 and Phase 5 on the PARALLEL
    /// evacuation drivers, which is where the three serial drivers have always
    /// retired theirs.
    ///
    /// The parallel drivers used to retire at the end of `parallel_evacuate`
    /// instead, and Phase 3.5 (`resurrect_dead_finalizers`) sits between the
    /// two points. Since F-02 the serial `evacuate_object` decides "already
    /// evacuated?" by the forwarding tag alone, so retiring first made Phase
    /// 3.5 copy every already-evacuated object a SECOND time and overwrite its
    /// forwarding-map entry with the duplicate — an identity split, silent
    /// because the duplicate is well-formed. The old placement also left Phase
    /// 3.5's own forwards installed past the end of the pause.
    ///
    /// `=0` restores the old placement, so the defect can be reproduced and the
    /// fix attributed on one binary.
    pub g1_retire_forwards_late: bool,
    /// `CRATONVM_G1_RETIRE_KEPT_ONLY` — on unless `0`. `retire_forwards`
    /// restores only the from-space bodies in a region the pause KEEPS (one
    /// holding a self-forward); every other CSet region is freed in Phase 5,
    /// is never read again, and is zeroed by the allocator on reuse. `=0`
    /// retires every key of the pause's map, as before 2026-10-06.
    pub g1_retire_kept_only: bool,
    /// `CRATONVM_G1_REEVAC_GUARD` — on unless `0`. Let `evacuate_object`
    /// consult the pause's `pointer_map` when the from-space mark word carries
    /// no forwarding tag, and answer from it rather than making a second copy.
    ///
    /// A fail-closed net under `g1_retire_forwards_late`, not a substitute for
    /// it: the counter behind it (`reevacuated_after_retire`) is what turns
    /// "some driver retires too early" from an inference into a number, and it
    /// counts whether or not this switch is on.
    pub g1_reevac_guard: bool,
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
    /// moving one AND to-space can cover from-space at all (a thin-slack cycle
    /// runs BUFFERLESS — one exact span per object off the shared cursor —
    /// rather than declining). Defaulting it off would leave the parallel copy
    /// inert on every workload that has the heap for it, which is the state a
    /// gated feature dies in.
    ///
    /// `=0` is a bisection lever, but NOT a behaviour-neutral one (gen r4w2,
    /// 2026-09-23; this doc used to say "the serial and parallel evacuators
    /// produce the same forwarding map, so a suspected regression is one run
    /// apart"). Both arms reach the same set of survivors, but to-space becomes
    /// the NEXT cycle's from-space and only the parallel arm leaves
    /// per-worker-buffer tail fillers in it, so the next cycle's object-start
    /// walk, anchors and trigger point differ. (gce e2/o removed the second
    /// lever, `CRATONVM_GC_PAR_EVAC_BUFFERLESS`, with the evac7 timing levers
    /// `CRATONVM_GC_PAR_EVAC_TARGETED_WAKE`, `CRATONVM_GC_PAR_EVAC_OLD_PLAB_HINT`
    /// and `CRATONVM_GEN_EVAC_SCAN_PREFETCH`: `e1-p-triage.md` table 2B.)
    pub gc_par_evac: bool,
    /// `CRATONVM_GC_PAR_EVAC_CARD_SEED` — gen r5w3/evac7 (2026-09-26): the
    /// parallel young copy forwards the dirty-card roots on its WORKERS, each
    /// taking an equal slice of them, instead of on the driver before the
    /// pool opens. Default **ON** since gcj j1yg-pause (2026-10-27; `=0`
    /// kills, [`parse::truthy_word_default_true`]): on JIT-warm javac the
    /// driver's serial forward of ~700 K card roots was ~50 ms of every young
    /// pause. Same roots, same
    /// closure, same forwarding map; only which thread copies which object
    /// changes. Ignored under `CRATONVM_GC_FULL_RSET_SCAN` (whose extra roots
    /// can repeat a slot). Proposal W2 of
    /// `docs/feature-designs/generative-gc-architecture.md`.
    pub gc_par_evac_card_seed: bool,
    /// `CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL` — gen r5w3/evac7
    /// (2026-09-26): a pinned in-place young cycle (see
    /// [`Self::gen_pinned_young_copy`], which must also be set) runs on the
    /// PARALLEL evacuator instead of the serial one, when the cycle's young
    /// generation is large enough for the parallel copy at all. Default
    /// **off** ([`parse::truthy_word`]; flipped ON by `WARNING
    /// gci-user-flip-20261013`, see [`GcFlags::from_source`]). Limit L1 of
    /// `docs/feature-designs/generative-gc-architecture.md`,
    /// built to wave 6's design.
    pub gen_pinned_young_copy_parallel: bool,
    /// `CRATONVM_GEN_PINNED_TAIL_WINDOW_LIVE` — gen r5w3/evac7 (2026-09-26):
    /// a pinned in-place young cycle offers survivors a from-space tail window
    /// of `used - free_list_bytes` (everything allocated, so every survivor
    /// fits) instead of `max(used / 2, 1 MiB)`, still capped by the un-bumped
    /// tail. Default **off** ([`parse::truthy_word`]). Commits more of the tail
    /// up front; tenures fewer survivors early. Limit L2 of the page above.
    pub gen_pinned_tail_window_live: bool,
    /// `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG` — let an explicit `System.gc()`
    /// take the generational MOVING (compacting) young cycle when nothing else
    /// about the cycle forces the in-place sweep. Default **off**
    /// ([`parse::truthy_word`]): every `System.gc()` sweeps young in place, the
    /// behaviour since the overlay-precision change.
    ///
    /// Off is not an accident of history. The in-place promise is PUBLISHED TO
    /// THE ROOT GATHERER before the collector decides: while a `System.gc()`
    /// is pending, the VM leaves class mirrors, loader metadata and
    /// collection-overlay contents OUT of the unconditional root set and lets
    /// the non-moving marker reach them by owner. All three readers ask
    /// `gc::gc_quiescence::explicit_full_gc_sweeps_young_in_place`, which reads
    /// this field, so turning it on moves the root gatherer and the collector
    /// together — the cost is that a `System.gc()` roots those side tables
    /// unconditionally (it cannot unload a class or free an overlay-only
    /// collection that cycle), the gain is that it compacts young.
    pub gc_system_gc_moving_young: bool,
    /// `CRATONVM_GC_ADAPTIVE_TENURING` — gen r4w6/young6 (2026-09-24): the
    /// generational young collector recomputes its tenuring threshold after
    /// every moving young cycle from an age table of surviving bytes
    /// (HotSpot's `AgeTable::compute_tenuring_threshold`, with HotSpot's
    /// defaults `MaxTenuringThreshold=15`, `TargetSurvivorRatio=50`), instead
    /// of promoting every survivor on its third survival. Default **on** since
    /// gcf 2026-10-01 (vf3 y3: tenuring items 4-7 = HotSpot; [`parse::on_unless_zero`]):
    /// `=0` restores the fixed age 3 and the promote-on-pressure arm. Any of `-XX:MaxTenuringThreshold`,
    /// `-XX:InitialTenuringThreshold`, `-XX:TargetSurvivorRatio` engages the
    /// same policy per VM without this variable. The probe that would justify
    /// flipping the default is `tools/bench/GenR4W6TenuringProbe.java`. See
    /// `gc/src/gen_heap_tenuring.rs`.
    pub gc_adaptive_tenuring: bool,
    /// `CRATONVM_GEN_PINNED_YOUNG_COPY` — gen r4w5/pinned5 (2026-09-24): a
    /// young cycle that the conservative-JIT-root term
    /// (`nonmoving-unrewritable-conservative-jit-roots`) would send to the
    /// non-moving sweep instead runs a copying cycle that leaves the 4 KiB
    /// pages its unrewritable conservative words land in where they are
    /// (`GenerationalHeap`'s pinned in-place evacuation). Default **on** since
    /// gci i14a (WARNING gci-user-flip-20261013; `=0` / `=false` are off); the
    /// cycle still diverts when the pin ledger is incomplete or the pinned
    /// pages exceed 1/8 of from-space. See
    /// `docs/feature-designs/generative-gc-architecture.md`.
    pub gen_pinned_young_copy: bool,
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
    /// `CRATONVM_DBG_SWEEP_LIVENESS` — gate for the old-gen sweep's
    /// freed-while-referenced assertion. [`parse::truthy_word`] (gci i4/fl;
    /// was presence, so `=0` armed it).
    pub dbg_sweep_liveness: bool,
    /// `CRATONVM_DBG_SWEEP_LIVENESS` — the value, so `=rescue` selects the
    /// retain-instead-of-free differential mode; any other value reports only.
    /// `None` whenever the gate above is off (`0`, `false`, `off`, `no`, empty).
    pub dbg_sweep_liveness_value: Option<String>,
    /// `CRATONVM_DBG_BADREF`
    pub dbg_badref: bool,
    /// `CRATONVM_DBG_DEADREF_STORE` — report a reference STORE whose value
    /// names the inactive young semispace, which holds no live object by
    /// construction. The producer-side half of the `[heap-stale]` verifier:
    /// that one finds a dangling field long after the write, this one names
    /// the write.
    pub dbg_deadref_store: bool,
    /// `CRATONVM_DBG_CELLCORRUPT`
    pub dbg_cellcorrupt: bool,
    /// `CRATONVM_DBG_DESCTRACE`
    pub dbg_desctrace: bool,
    /// `CRATONVM_DBG_FWDGUARD`
    pub dbg_fwdguard: bool,
    /// `CRATONVM_DBG_GCPAUSE`
    pub dbg_gcpause: bool,
    /// `CRATONVM_DBG_GCPAUSE_MIN_US` -- the generational `[gcpause]` line's
    /// report threshold, in microseconds: a collection shorter than this prints
    /// nothing. Default `100000` (the historical 100 ms); `0` prints every
    /// cycle. Without it the phase medians the round-4 sweep proposals ask for
    /// could only be taken on pauses of 100 ms or more
    /// (`docs/feature-designs/generative-gc-architecture.md`).
    /// Only read when `CRATONVM_DBG_GCPAUSE` is on. gen r4w2/obs.
    pub dbg_gcpause_min_us: u64,
    /// `CRATONVM_DBG_GCPHASE`
    pub dbg_gcphase: bool,
    /// `CRATONVM_DBG_GCWRITE`
    pub dbg_gcwrite: bool,
    /// `CRATONVM_DBG_HEAP_TRACE`
    pub dbg_heap_trace: bool,
    /// `CRATONVM_DBG_MIRRORPIN`
    pub dbg_mirrorpin: bool,
    /// `CRATONVM_DBG_OOBFIELD` — gate. [`parse::truthy_word`] (gci i4/fl;
    /// was presence). The value is a class-name substring, so a class filter
    /// spelled `no` or `off` reads as off.
    pub dbg_oobfield: bool,
    /// `CRATONVM_DBG_OOBFIELD` — the value, when it is valid UTF-8 and the
    /// gate above is on (`None` otherwise); the same variable is read both
    /// ways at different sites. [`parse::utf8`].
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
    /// `CRATONVM_GC_OLD_TRIGGER_HYSTERESIS` — default **on** under the
    /// concurrent-first old-gen policy since gen r4w5, off under the legacy one
    /// ([`parse::truthy_word`], so `=0` / `=false` are off). Generational only: once
    /// the old generation is above its 75 % trigger, skip a repeat old-gen
    /// collection until half of what the previous one left free (at least
    /// 1/16 of the generation) has been promoted since, unless an old-gen
    /// allocation has failed in between. Off, the trigger is the bare
    /// occupancy test it has always been, and only its counters are new. See
    /// `gc::old_gen::OldGen::major_trigger_verdict` and
    /// `docs/feature-designs/generative-gc-architecture.md`.
    ///
    /// gen r4w4/concmark4 (2026-09-24): this field is the EXPLICITLY-ON reading
    /// only, and is what the legacy old-gen policy uses. The effective value
    /// under the default concurrent-first policy is
    /// [`Self::old_trigger_hysteresis_explicit`] with a policy default (see
    /// `gc::concurrent_mark::OldGenPolicy::hysteresis`, ON since the gen r4w5
    /// flip; that function records the count-based evidence).
    pub old_trigger_hysteresis: bool,
    /// gen r4w4/concmark4 — `CRATONVM_GC_OLD_TRIGGER_HYSTERESIS` as a
    /// tri-state: `None` unset (the policy's default applies), `Some(false)`
    /// for `""`/`0`/`false`/`off`/`no` (an explicit OFF that survives a
    /// default flip), `Some(true)` otherwise. Same value rule as
    /// [`parse::truthy_word`] (gci i4/fl).
    pub old_trigger_hysteresis_explicit: Option<bool>,
    /// `CRATONVM_GC_NO_CONCURRENT_FIRST` — opt-OUT (default: concurrent-first
    /// ON; [`parse::truthy_word`], so `=0` / `=false` keep it on). Generational
    /// only (gen r4w4/concmark4, 2026-09-24). Set, the old generation runs the
    /// pre-2026-09-24 policy: the concurrent cycle and the stop-the-world
    /// old-gen collection share the one 75 % trigger and the STW collection
    /// never defers to an open cycle. Unset, the concurrent cycle starts at an
    /// adaptive initiating occupancy below the STW floor and the STW collection
    /// defers to an open cycle unless requested, an old-gen allocation failed,
    /// or the generation is 90 % full. See `gc::concurrent_mark::OldGenPolicy`
    /// and `docs/feature-designs/generative-gc-architecture.md`.
    pub old_no_concurrent_first: bool,
    /// `CRATONVM_GC_CONC_START_PERCENT=<n>` — a FIXED initiating occupancy for
    /// the generational concurrent cycle, in percent of the old generation,
    /// clamped to `[20, 75]`; disables the adaptive threshold (HotSpot's
    /// `-XX:-G1UseAdaptiveIHOP -XX:InitiatingHeapOccupancyPercent=<n>`). Unset,
    /// `0` or unparseable: adaptive. Concurrent-first policy only.
    pub conc_start_percent: Option<usize>,
    /// `CRATONVM_GEN_CONC_MARK_NO_LOCAL_STACK` — opt-OUT
    /// ([`parse::truthy_word`]). Generational concurrent marker (gen
    /// r4w4/concmark4): set, every grey object goes through the sharded
    /// `MarkQueue` again (a mutex and two atomic RMWs per push and per pop);
    /// unset, the marker drains through a marker-local LIFO stack and uses
    /// the shared queue only as spill space. The A/B arm for
    /// `gengc-r4-mark-old-gen-concurrent-mark-costs-FIXED-20260924.md` item 4.
    pub gen_conc_mark_no_local_stack: bool,
    /// `CRATONVM_GEN_CONC_NO_FROZEN_EAGER_SCAN` — kill switch
    /// ([`parse::truthy_word`]) over a correctness fix (gen
    /// r4w4/concmark4): set, the generational initial-mark pause no longer
    /// scans the old-gen objects held by threads the xt takeover FROZE, which
    /// reopens `gengc-r4w2-concmark-jit-gate-takeover-window`. Bisection only.
    pub gen_conc_no_frozen_eager_scan: bool,
    /// `CRATONVM_GEN_CONC_SERVICE_THREAD` — opt-in
    /// ([`parse::truthy_word`]). Generational only (gen r4w5/concmark5,
    /// 2026-09-24): the VM starts a dedicated concurrent-cycle SERVICE thread
    /// (`gc::concurrent_mark::ConcurrentGcState::serve`) that runs the
    /// concurrent old-gen cycle — initial-mark and remark pauses, Phase 2 and
    /// the sweep — instead of the mutator whose young-collection epilogue
    /// found it due; a due cycle is handed to it, and direct old-gen
    /// allocations and a periodic check wake it without any young collection.
    /// Unset: no service, the inline driver exactly as before. See
    /// `docs/feature-designs/generative-gc-architecture.md`.
    pub gen_conc_service_thread: bool,
    /// `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK` — default **ON** today
    /// ([`parse::on_unless_zero`]: `=0` turns it off; the "opt-in" wording
    /// below predates that flip, see `from_source`). Generational concurrent marker (gen
    /// r4w5/concmark5): the remark pause calls the reference-processing seam
    /// (`ConcurrentMarker::remark_with_reference_processing`) with a NO-OP
    /// callback that only counts itself — the call site the real remark-time
    /// reference processing will use, exercised with no semantic change. See
    /// `docs/feature-designs/generative-gc-architecture.md`.
    ///
    /// gen r5w1/refs5 (2026-09-26): the VM driver now runs the REAL remark-time
    /// reference processing under this flag — the old-gen `Reference`s'
    /// referent slots hidden from the concurrent trace at initial mark, weak /
    /// soft / phantom references to dead old-gen referents cleared and
    /// enqueued at remark, dead finalizables and cleaner actions kept for
    /// delivery — instead of waiting for a stop-the-world full collection.
    /// Still opt-in; `concdrv_remark_refproc_hook_calls` counts the remarks
    /// that ran it and `concdrv_remark_refproc_retired` what they retired.
    pub gen_conc_remark_refproc_hook: bool,
    /// `CRATONVM_GEN_SATB_OLD_ONLY` — default ON since gcf 2026-10-01
    /// ([`parse::on_unless_zero`]; `=0` logs every value again).
    /// Generational only (gen r5w1/refs5, 2026-09-26): the SATB pre-barrier
    /// logs an overwritten reference only when it lies in the OLD generation
    /// (one subtract-and-compare against the card table's span), because the
    /// generational concurrent marker traces the old generation only and every
    /// consumer drops a young entry. `=0`: every non-null old value is logged,
    /// as before. See
    /// `docs/feature-designs/jit-barrier-lowering.md`.
    pub gen_satb_old_only: bool,
    /// `CRATONVM_GEN_CONC_ALLOC_FAIL_DOOR` — default **on**
    /// ([`parse::on_unless_zero`], so `=0` is the kill switch). Generational
    /// only (gen r5w1/refs5, 2026-09-26; default flipped by gen r5w2/conc6):
    /// the ALLOCATION-FAILURE collection door (`maybe_gc_forced`, which every
    /// JIT allocation helper, `alloc_object_shared` and the TLAB refill wedge
    /// funnel into) asks the concurrent start trigger after a collection it
    /// ran, as the interpreter's `maybe_gc` epilogue does, and runs a due
    /// cycle inline — the Generational twin of G1's
    /// `CRATONVM_G1_ALLOC_MARK_DRIVE` (also default-on). `=0`: that door never
    /// consults the trigger, so a program whose young collections all come
    /// from compiled code opens no concurrent cycle unless the service thread
    /// (`CRATONVM_GEN_CONC_SERVICE_THREAD`) is on — the pre-r5w2 default, and
    /// the A/B arm. With the service attached the door HANDS the due cycle to
    /// it whatever this says. See
    /// `docs/feature-designs/generative-gc-architecture.md`.
    pub gen_conc_alloc_fail_door: bool,
    /// `CRATONVM_GEN_CONC_CLASS_UNLOAD` — default **ON** today
    /// ([`parse::on_unless_zero`]: `=0` turns it off; the "Unset" sentence
    /// below predates that flip and now describes `=0`).
    /// Generational only (gen r5w3/unload7, 2026-09-26): the concurrent
    /// old-generation cycle may UNLOAD classes, as G1's remark does, instead
    /// of leaving every dead class loader for a stop-the-world full
    /// collection. Its initial mark and remark take their roots under
    /// `gc_quiescence::with_class_unload_marking` (user-loader mirrors,
    /// statics and the other loader-conditional roots are deferred to their
    /// loader), the marker follows a snapshot of the `loader_pin` /
    /// `mirror_pin` / `metadata_pin` tables, the young objects' loaders are
    /// roots, and the remark reconciles and unloads the dead loaders' classes
    /// after its final drain. Takes effect only together with
    /// `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK` (the remark-time processing it
    /// runs inside) and with loader unloading on (`CRATONVM_LOADER_UNLOAD`,
    /// the default). Unset: every side-table deferral stays vetoed for the
    /// concurrent mark, exactly as before. See
    /// `docs/feature-designs/generative-gc-architecture.md`.
    pub gen_conc_class_unload: bool,
    /// `CRATONVM_SOFTREF_HOTSPOT_LRU` — every backend (gen r5w1/refs5,
    /// 2026-09-26): the SoftReference LRU policy takes HotSpot's
    /// `LRUMaxHeapPolicy` inputs — the soft clock as of the END of the
    /// previous collection (not the wall clock now) and the free heap AFTER
    /// the previous collection (not the free space of the generation that just
    /// filled) — so a soft reference read since the last collection is never
    /// condemned by the policy and soft caches survive young collections while
    /// the heap has room. See
    /// `docs/feature-designs/generative-gc-architecture.md`.
    ///
    /// gen r5w4/defaults8: the EXPLICIT setting only — `None` when unset (was
    /// an opt-in `bool` over [`parse::non_empty_non_zero`]; `false` / `off` /
    /// `no` now read as off, where they used to switch it ON). Its default is
    /// per backend ([`alloc_policy_defaults::SOFTREF_HOTSPOT_LRU`]: ON on the
    /// Generational heap, OFF on G1; zr z1/rf: ON on ZGC,
    /// [`alloc_policy_defaults::SOFTREF_HOTSPOT_LRU_ZGC`]), so read it through
    /// [`GcFlags::softref_hotspot_lru_for_backend`]; the VM's reference
    /// processor latches it with its heap (`ReferenceProcessor::new_for_backend`).
    pub softref_hotspot_lru: Option<bool>,
    /// `CRATONVM_GC_OLD_GIVE_BACK` — opt-in (default **off**;
    /// [`parse::truthy_word`], so `=0` / `=false` are off). Generational only (gen
    /// r4w3/oldgen3, 2026-09-23): after an IN-PLACE old-gen sweep that leaves
    /// the generation below its 75 % trigger, hand the whole-granule interiors
    /// of free blocks spanning at least four granules back to the OS (at most
    /// once per four old-gen collections). Off, nothing is given back and only
    /// the candidate-bytes sample is new. A policy change (RSS against
    /// re-fault cost), hence opt-in pending the A/B. See
    /// `gc::old_gen::OldGen::after_in_place_sweep`.
    pub old_give_back: bool,
    /// `CRATONVM_GC_OLD_SHRINK` — default **on** ([`parse::on_unless_zero`],
    /// so only `=0` turns it off). Generational only (gen r4w4/oldgen4,
    /// 2026-09-24): after a stop-the-world old-gen collection, DECOMMIT the
    /// whole granules of the old generation's trailing free run above
    /// `max(-Xms prefix, used / 0.3)` (HotSpot's `MaxHeapFreeRatio = 70`),
    /// damped so a collection whose preceding interval grew the committed end
    /// shrinks nothing — so `Runtime.totalMemory()` falls with the live set.
    /// `=0` is the kill switch: a decommitted granule faults on touch, so a
    /// compiled load through a DANGLING old-gen reference into the released
    /// tail faults instead of reading stale bytes (the same trade as
    /// `CRATONVM_GEN_UNCOMMIT`). See
    /// `gc::old_gen::OldGen::resize_after_collection`.
    pub old_shrink: bool,
    /// `CRATONVM_GC_OLD_FRAG_COMPACT` — opt-in (default **off**;
    /// [`parse::truthy_word`], so `=0` / `=false` are off). Generational only (gen
    /// r4w4/oldgen4): the PROACTIVE fragmentation trigger — an old-gen
    /// collection that ends with the largest free block below the largest
    /// request served since the last repair, while the free bytes hold four
    /// times that request, asks the next young cycle for a COMPACTING old-gen
    /// collection before any allocation has failed. Opt-in because it compacts
    /// on a prediction, and compaction itself is opt-in (see
    /// `gen_heap::oldgen_compact_enabled` for the unexplained corruption that
    /// took it off by default). Also enables the refusal-armed request below.
    /// See `gc::old_gen::OldGen::note_fragmentation_after_collection`.
    pub old_frag_compact: bool,
    /// `CRATONVM_GC_OLD_OOM_COMPACT` — default **on**
    /// ([`parse::on_unless_zero`]). Generational only (gen r4w4/oldgen4): when
    /// an old-gen allocation is REFUSED although the free list holds its size
    /// in total (a humongous array, a promotion — "there are bytes but no
    /// room"), the next young cycle runs an old-gen collection, below the
    /// occupancy floor too, and the moving path's collection COMPACTS once;
    /// the allocation-failure ladder can ask the same directly
    /// (`gc::gen_heap::GenerationalHeap::request_old_gen_compaction_if_fragmented_for`).
    /// HotSpot Serial's full GC always compacts; without this a humongous
    /// request on a fragmented old generation throws a false
    /// `OutOfMemoryError`. Exposure to the compactor is limited to a request
    /// that has already failed for want of a contiguous block; `=0` restores
    /// the in-place-only behaviour.
    pub old_oom_compact: bool,
    /// `CRATONVM_GC_OLD_WALK_GAP_RECOVERY` — default **on**
    /// ([`parse::on_unless_zero`]). Generational only (gen r4w4/oldgen4): when
    /// the in-place old-gen sweep's object walk stops at an implausible header,
    /// scan the unwalked bytes conservatively as roots and keep reclaiming
    /// the rest of the generation, instead of freeing nothing on every cycle
    /// until `OutOfMemoryError`. `=0` restores the wave-3 fail-closed sweep.
    /// See `gc::gen_heap::seed_old_gen_walk_gaps`.
    pub old_walk_gap_recovery: bool,
    /// `CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4` — opt-in (default **off**;
    /// [`parse::truthy_word`], so `=0` / `=false` are off). Generational only (gen
    /// r4w5/pinwords5, 2026-09-24): "option B" of
    /// `docs/feature-designs/generative-gc-architecture.md`.
    /// Set, the young collector's conservative-JIT-root divert (term 4,
    /// `nonmoving-unrewritable-conservative-jit-roots`) stands down on a cycle
    /// whose per-pause young pin-word ledger is COMPLETE (every thread inside
    /// compiled code deposited for this pause) and EMPTY (no conservative word
    /// outside a rewritten or provably dead slot lands in a young region).
    /// Unset, the ledger is still built and counted
    /// (`[GC] young_conservative_divert: cjdiv_ledger_*`) but decides nothing.
    /// See `gc::gc_quiescence::young_pin_ledger_clears_term4`.
    pub gen_young_pin_ledger_term4: bool,
    /// `CRATONVM_GEN_PRECISE_ROOT_PROMOTE` -- opt-in (default **off**;
    /// [`parse::truthy_word`], the same rule as the `runtime_flag_on` reader in
    /// `gc::gen_heap_tenuring::precise_root_promote_enabled`). The snapshot copy
    /// is for the young pin ledger's hot deposit paths: when the non-moving
    /// sweep may promote a precise-only root value, the ledger is what proves
    /// no compiled-frame word still names it, so it must be strict (JIT r20
    /// wave 18, lane gcI; see `gc_quiescence::young_pin_ledger_licenses_moves`).
    pub gen_precise_root_promote: bool,
    /// `CRATONVM_GC_OLD_PINNED_COMPACT` — opt-in (default **off**;
    /// [`parse::truthy_word`], so `=0` / `=false` are off). Generational only (gen
    /// r4w5/oldcompact5, 2026-09-24): a FRAGMENTATION compaction request (an
    /// old-gen allocation refused although the free list holds its size in
    /// total — see `CRATONVM_GC_OLD_OOM_COMPACT`) that reaches the NON-MOVING
    /// old-gen collection is answered there by a sliding compaction AROUND
    /// pinned objects: everything a conservative word names (an interior root,
    /// a published JIT word, a word in an unparseable young stretch — and
    /// every root, when an unpublished conservative source ran) keeps its
    /// address, everything else slides. Off, that collection discards the
    /// request and sweeps in place, so a humongous array on a fragmented old
    /// generation in a JIT-warm process (or on the allocation-failure
    /// ladder's forced major) throws a false `OutOfMemoryError`. Opt-in
    /// because it moves old objects on a cycle whose root set is partly
    /// conservative. See `gc::old_gen::OldGen::compact_around_pins`.
    pub old_pinned_compact: bool,
    /// Default-on fragmented non-moving majors through the pinned compactor. A
    /// request is admitted at most once per four old-generation collections;
    /// after pin settlement it is declined when pins reach 10% of marked live
    /// bytes. `CRATONVM_GC_OLD_FRAG_PINNED_COMPACT`.
    pub old_frag_pinned_compact: bool,
    /// `CRATONVM_OLDGEN_COMPACT` — default **on**, `=0` kills.
    /// Generational: every moving
    /// major runs the walked mark-compact (`OldGen::compact_with_walked_objects`)
    /// instead of the in-place sweep. gcf f4/c: was a process-global `OnceLock`
    /// in `gen_heap::oldgen_compact_enabled`; a field now, so a per-thread test
    /// override reaches it.
    pub oldgen_compact: bool,
    /// `CRATONVM_GC_G1_ONLY_JIT_PINS` — opt-in ([`parse::truthy_word`] since
    /// gci i4/fl; `=0` is off). Restores the pre-2026-09-06 gate that withheld a parked
    /// thread's conservative JIT-frame roots from `gc_quiescence`'s pin
    /// registry on every backend but G1; the pinned old-gen compaction then
    /// pins the WHOLE root slice (`OldPinnedCompact::plan_from_sources`). A
    /// typed field since gcd d1/d (2026-09-27): the collector read it with
    /// `runtime_var_os` on every pause that had a fragmentation request
    /// pending (`gengc-r4w6-review6-old-pinned-compaction-residuals`, "Also
    /// noted" 3). The VM's own deposit-side reader
    /// (`gc_and_alloc::g1_only_jit_pins`) still latches it in a `OnceLock`.
    pub g1_only_jit_pins: bool,
    /// `CRATONVM_GC_OVERHEAD_PROGRESS` — default **ON**
    /// ([`parse::on_unless_off_word_cased`]: `0`, `false`, `off` switch it
    /// off). Read by the VM (gen r4w5/thrash5): the mutator-progress half of
    /// the GC-overhead limit and the Generational "full collection, then the
    /// error" exit of the interpreter's allocation ladder
    /// (`vm::runtime::interpreter::gc_and_alloc::gc_cycle_is_unproductive`).
    /// A typed field since gen r4w6/review6: it was a process-wide `OnceLock`
    /// over `runtime_var`, which latched the first reader's value — a test
    /// override included — for the rest of the process.
    pub gc_overhead_progress: bool,
    /// `CRATONVM_MONITOR_ENTER_SPIN` — default **ON**
    /// ([`parse::on_unless_off_word_cased`]). Read by the VM (gen
    /// r4w5/thrash5): the bounded spin a contended `monitorenter` takes before
    /// it parks and retires its TLAB (`vm::threading::monitor::Monitor::spin_try_enter`).
    /// Here rather than in a threads group because its reason is the TLAB and
    /// the young trigger. A typed field since gen r4w6/review6, for the same
    /// reason as [`GcFlags::gc_overhead_progress`].
    pub monitor_enter_spin: bool,
    /// `CRATONVM_GC_FUTILE_YOUNG_BACKOFF` — default **ON**
    /// ([`parse::on_unless_zero`]: only `0` turns it off). Generational only
    /// (gcd d2/j, 2026-09-27): once an object door's forced young cycle leaves
    /// young unable to serve the object that missed (a young generation full
    /// of live objects under a non-moving cycle), the object doors spill to
    /// the old generation first and force the next young cycle only after a
    /// quantum of allocation, instead of one futile collection per object
    /// (`vm::runtime::interpreter::gc_and_alloc::futile_young_backoff_skips`).
    /// A liveness fix: `gcd-d1b-thread-exit-shape-livelocks-on-forced-young-cycles`.
    /// `=0` restores the collection per door entry.
    pub futile_young_backoff: bool,
    /// `CRATONVM_GC_PREALLOCATED_OOME_KINDS` — default **on** since gcf 2026-10-01
    /// ([`parse::on_unless_zero`], so `=0` is off). gcd d2/j (2026-09-27):
    /// `ensure_singleton_oom` also preallocates an `OutOfMemoryError` for
    /// `Requested array size exceeds VM limit` and for `Metaspace`, and an
    /// OOME that cannot be built fresh falls back to the one of its own
    /// message instead of the `Java heap space` singleton
    /// (`vm::vm::realms::heap_realm::PreallocatedOome`).
    pub preallocated_oome_kinds: bool,
    /// `CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT` — opt-in (default **off**;
    /// [`parse::truthy_word`], so `=0` / `=false` are off). Generational only (gen
    /// r4w6/oldpin6, 2026-09-24): after a CONCURRENT old-gen cycle's sweep
    /// ends, the next young pause (stop-the-world, under the old-gen lock)
    /// runs the committed-size resize that `CRATONVM_GC_OLD_SHRINK` otherwise
    /// runs only after a stop-the-world old-gen collection — retried for up to
    /// four young pauses while the shrink is damped. Under the default
    /// concurrent-first policy a workload that peaks and settles may see no
    /// stop-the-world old-gen collection at all, so without this its committed
    /// old generation stays at the peak. Needs `CRATONVM_GC_OLD_SHRINK` (on by
    /// default). Probe: `tools/bench/GenR4W6OldShrinkProbe.java`. See
    /// `gc::old_gen::OldGen::resize_after_concurrent_sweep_if_due`.
    pub old_shrink_after_concurrent: bool,
    /// `CRATONVM_GC_CONC_WALK_GAP_RECOVERY` — opt-in (default **off**;
    /// [`parse::truthy_word`], so `=0` / `=false` are off). Generational only (gen
    /// r4w6/oldpin6, 2026-09-24): a concurrent old-gen cycle whose object
    /// walk desynced (a header the walk cannot size) SWEEPS anyway, as the
    /// stop-the-world sweep has since wave 4 (`CRATONVM_GC_OLD_WALK_GAP_RECOVERY`):
    /// the remark walks afresh, requires the unwalked bytes to account exactly
    /// for the rest of `used`, marks every walked object a word in those bytes
    /// names, rescans every marked object, and frees only walked objects.
    /// Off, such a cycle reclaims nothing (`CONC_MARK_WALK_DESYNC_ABORTS`). See
    /// `gc::concurrent_mark::ConcurrentMarker::remark_walk_gap_plan`.
    pub conc_walk_gap_recovery: bool,
    /// `CRATONVM_GEN_XMS_USABLE_FIRST` — default **on** since gen
    /// r5w4/defaults8 ([`alloc_policy_defaults::GEN_XMS_USABLE_FIRST`], parsed
    /// like the allocation-policy arms: `0` / `false` / `off` / `no` turn it
    /// off; until then an opt-in over [`parse::non_empty_non_zero`]).
    /// Generational only (gen r5w3/oldgen7, 2026-09-26): the `-Xms` startup
    /// commit is split the way HotSpot Serial sizes its initial heap — the
    /// young pair's share of
    /// `-Xms` goes to the from-space less one SURVIVOR (a tenth of that share,
    /// `SurvivorRatio = 8`), the to-space gets only the survivor, and the old
    /// generation the rest — instead of half of the young share in each
    /// semi-space. `Runtime.totalMemory()` of a fresh `-Xms64m -Xmx512m` heap
    /// then reads ~64m (HotSpot ~62m) instead of 32m; the startup commit is
    /// still `-Xms`. The cost is commit faults in the to-space on the first
    /// evacuations. See `gc::gen_heap::GenerationalHeap::commit_initial_heap`.
    pub gen_xms_usable_first: bool,
    /// `CRATONVM_GC_OLD_INTERIOR_DECOMMIT` — opt-in (default **off**;
    /// [`parse::truthy_word`], so `=0` / `=false` are off). Generational only (gen
    /// r5w3/oldgen7, 2026-09-26): at the end of a stop-the-world old-gen
    /// collection (and at the young pause that resizes after a concurrent
    /// cycle, under `CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT`), DECOMMIT the
    /// whole granules of large free blocks BELOW the trailing free run while
    /// the committed generation is above `used / 0.3` (the shrink's
    /// `MaxHeapFreeRatio` target), largest first — so one survivor near the
    /// top no longer pins the whole committed extent. A decommitted interior
    /// granule faults on touch; every raw old-gen reader checks the commit
    /// bitmap for it (`OldGen::contains`, `OldGen::contains_range`). Needs
    /// `CRATONVM_GC_OLD_SHRINK` (on by default). Skipped while a concurrent
    /// cycle is open. See `gc::old_gen::OldGen::decommit_interior_free_runs`.
    pub old_interior_decommit: bool,
    /// `CRATONVM_GC_OLD_BORROW_YOUNG` — default **ON** since gcg g6/yg
    /// ([`parse::non_empty_non_zero_default_true`]; `=0` is off). Generational only, and
    /// only for the DEFAULT young/old split (`-Xmn`, `-XX:NewSize`,
    /// `-XX:MaxNewSize`, `-XX:NewRatio` all unset; gen r5w3/oldgen7,
    /// 2026-09-26): the old generation is RESERVED up to HotSpot Serial's
    /// `MaxHeapSize - MaxNewSize` (`2/3 · -Xmx`) while its capacity starts at
    /// its default share (`-Xmx / 2`), and a stop-the-world old-gen
    /// collection that follows a REFUSED old-gen allocation grows the
    /// capacity in place (HotSpot's `MinHeapFreeRatio = 40`) — the young
    /// trigger is capped so the young generation cedes that budget. A tenured
    /// set between 1/2 and 2/3 of `-Xmx` then fits, as on HotSpot Serial,
    /// instead of throwing `OutOfMemoryError`. See
    /// `gc::old_gen::OldGen::grow_after_refusal`.
    pub old_borrow_young: bool,
    /// `CRATONVM_GC_OLD_HUMONGOUS_TOP` — default **on** since the GC defects
    /// round's triage (2026-09-28; [`parse::on_unless_off_word_cased`], so
    /// `=0` / `false` / `off` restores the best fit; opt-in before).
    /// Generational only (gen r5w3/oldgen7, 2026-09-26): a HUMONGOUS array (one that goes straight to
    /// the old generation) is carved from the END of the highest free block
    /// that can hold it, instead of the best fit that fills from low
    /// addresses, and only the array's own pages are committed. Humongous
    /// arrays then cluster at the top and small survivors at the bottom, so a
    /// dead array's span merges with its neighbours and the free tail instead
    /// of leaving a hole between long-lived small objects. See
    /// `gc::old_gen::OldGen::alloc_from_top`.
    pub old_humongous_top: bool,
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
    /// `CRATONVM_G1_IHOP_COUNTS_REGIONS` — measure old-generation occupancy for
    /// IHOP by counting the regions the old generation has TAKEN instead of
    /// summing the live bytes in them.
    ///
    /// **OPT-IN** ([`parse::non_empty_non_zero`]): unset, empty and `=0` are
    /// off. (This line used to say presence is truth and `=0` turns it ON;
    /// that stopped being true when the `CRATONVM_G1_*` booleans moved to
    /// `non_empty_non_zero` on 2026-09-20 -- corrected by gci i4/fl.)
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
    ///
    /// 2026-09-06, Tomcat `TestHostConfigAutomaticDeploymentXmlExternalWarXml`:
    /// the byte-count reading is wrong there for exactly the reason above --
    /// the old generation held at most 0.64 GB in 2.03 GB of regions -- but
    /// turning this on did not start a cycle in the one run measured either,
    /// because the class finished with Old at 937 of 2048 regions, still under
    /// the 70% threshold. It is the SECOND brake on that workload; the first,
    /// and the one that filled the heap, was `g1_parallel_evac_resume_dest`.
    /// This flag stays un-defaulted on its own H2 evidence, not on that run.
    pub g1_ihop_counts_regions: bool,

    // --- lane W2-C (2026-09-20), the marking cycle's policy and cost ---
    //
    // All five are OPT-IN (`=1`). Each changes a POLICY number or a
    // per-object cost inside the mark cycle, and this round's own rule is that
    // a default moves on a measurement rather than on an argument -- wave 1
    // flipped two defaults back to opt-in for exactly that reason (see
    // `docs/feature-designs/concurrent-gc-maturation.md` §4).
    // The A/B recipe for each is written onto its consumer in `gc/src/g1.rs`.
    /// `CRATONVM_G1_IHOP_GROSS_GROWTH` — [`parse::non_empty_non_zero`].
    ///
    /// Measure the old generation's growth DURING a mark cycle instead of
    /// NET of the reclaim that cycle performed, and derive the allocation
    /// rate in bytes rather than through a double integer truncation.
    ///
    /// Off (today): `note_mark_cycle_end` reads `old_gen_bytes` after
    /// `cleanup` has already applied the cycle's in-place frees and dead
    /// humongous reclaim, so a steady-state cycle that reclaims roughly what
    /// it promoted folds `grew == 0`, `alloc_rate_kib_per_ms` decays to zero,
    /// and `recompute_marking_threshold` silently falls back to the static
    /// IHOP — the adaptive model disengages on exactly the workload it was
    /// built for. `ihop_model_state()`'s fifth element reports that state
    /// either way, which is the half of the fix that is NOT behind this flag.
    pub g1_ihop_gross_growth: bool,
    /// `CRATONVM_G1_IHOP_BACKOFF` — [`parse::non_empty_non_zero`].
    ///
    /// Refuse to start a second concurrent cycle against a heap that has not
    /// grown since the last one reclaimed nothing.
    ///
    /// Off (today): `check_ihop` is a pure level test, so a LIVE old set above
    /// the threshold marks continuously — every cycle reclaims nothing,
    /// republishes the same occupancy, and re-fires the trigger — burning
    /// `concurrent_mark_worker_count()` threads beside the application
    /// precisely when the heap is tightest.
    pub g1_ihop_backoff: bool,
    /// `CRATONVM_G1_MARK_BACKOFF_DEADLINE` — [`parse::non_empty_non_zero`].
    ///
    /// Bound how long [`Self::g1_ihop_backoff`] may defer a cycle: once
    /// `MARK_BACKOFF_DEADLINE_COLLECTIONS` collections have completed since
    /// the last cycle ended, let the next one through even though the old
    /// generation has not grown. Has no effect unless the back-off is on.
    ///
    /// Off (today): the back-off's ONLY re-arm is old-generation GROWTH, and
    /// the width it demands doubles per unproductive cycle. That is blind on
    /// exactly one axis, and it is the axis a concurrent marker exists to
    /// cover — old objects DYING. When a program drops a large old data set
    /// and then allocates only short-lived objects, occupancy does not grow,
    /// the gate never reopens, the dead old set is never discovered, and the
    /// collector finds out by failing to evacuate. `G1OldBurstProbe` is built
    /// to produce exactly that shape; see
    /// `docs/feature-designs/concurrent-gc-maturation.md`.
    pub g1_mark_backoff_deadline: bool,
    /// `CRATONVM_G1_IHOP_POLL_GATE` — [`parse::non_empty_non_zero`].
    ///
    /// Make a DECLINED `check_ihop` poll cost no shared write. Has no effect
    /// unless [`Self::g1_ihop_backoff`] is on, because nothing else declines.
    ///
    /// Off (today): every declined poll performs two read-modify-writes on two
    /// PROCESS-GLOBAL cache lines — `IHOP_POLLS.fetch_add(1)` and
    /// `mark_backoff_suppressions.fetch_add(1)` — and the poll rate is the
    /// ALLOCATION rate, because a suppressed cycle never sets
    /// `g1_is_marking_active()` and so never short-circuits the JIT mark
    /// driver that `jit_new_object` / `jit_newarray` /
    /// `jit_anewarray_object` each call unconditionally. Measured at
    /// `ihop_polls=1 616 077` in wave 3 and `41 641 213` against 130 mark
    /// cycles in wave 5, both on SINGLE-THREADED probes where the two lines
    /// are uncontended. With N allocating mutators they are N-way contention
    /// on two lines at allocation rate.
    ///
    /// On: `check_ihop` publishes the occupancy at which its answer could
    /// change (`backoff_gate_open_at_bytes`) and a subsequent declined poll is
    /// two relaxed loads, a compare, and a thread-local increment flushed to
    /// the globals every `IHOP_POLL_FLUSH_BATCH`. The verdict is unchanged —
    /// the published watermark is the slow path's own decline condition, and
    /// every input to it is written only where the watermark is cleared.
    ///
    /// See `docs/feature-designs/concurrent-gc-maturation.md`.
    pub g1_ihop_poll_gate: bool,
    /// `CRATONVM_G1_ALLOC_MARK_DRIVE` — drive the G1 mark-cycle lifecycle from
    /// the ALLOCATION-FAILURE pause as well as from `maybe_gc`'s. Default
    /// **ON** ([`parse::on_unless_zero`]); `=0` restores the behaviour every
    /// measurement before 2026-09-21 was taken under.
    ///
    /// # What `=0` restores, and why it is a defect rather than a policy
    ///
    /// `marking_complete` is set in exactly one place, the tail of
    /// `G1Collector::cleanup`, and `cleanup` is reached from exactly one VM
    /// function, `interpreter::g1_final_remark_cleanup`. Before this flag its
    /// callers were: `maybe_concurrent_gc` (`maybe_gc`'s epilogue — a pause
    /// `maybe_gc` itself took, plus `System.gc()`'s own call), the opt-in JIT
    /// mark driver, and `g1_force_full_cycle` (`System.gc()` and the pre-OOM
    /// ladder).
    ///
    /// `maybe_gc_forced_at` — the allocation-failure collection, which every
    /// JIT allocation helper, `alloc_object_shared`, `gc_alloc_array` and the
    /// TLAB refill wedge funnel into — was NOT among them. So on a default
    /// build a cycle that opens is never closed: `g1_is_marking_active()`
    /// stays true, which also makes `g1_should_start_marking()`'s
    /// `&& !is_marking_active()` guard refuse every subsequent start, so the
    /// collector runs for the rest of the process with one open cycle,
    /// `marking_complete == false`, `needs_mixed_gc()` false, and no mixed
    /// pause ever. Measured as `remark_pauses=1, cleanup_pauses=0` in
    /// `docs/feature-designs/concurrent-gc-maturation.md` §4, and as
    /// old growing to 245 of 256 regions with 18-54 to-space exhaustions per
    /// run in `g1_drive_concurrent_mark_pub`'s own doc.
    ///
    /// On, the forced pause runs the identical finish-then-start pair, in
    /// `maybe_concurrent_gc`'s order, on a thread that has just completed a
    /// collection and reopened the world — the same state `maybe_gc`'s
    /// epilogue calls it in. It is not new machinery; it is the existing call
    /// on the path that was missing it, and it needs no JIT.
    ///
    /// `=0` is the bisection lever for any G1 change in mark-cycle frequency
    /// dated after 2026-09-21, and the arm under which every earlier marking
    /// measurement in this round was taken.
    ///
    /// See `docs/feature-designs/concurrent-gc-maturation.md`.
    pub g1_alloc_mark_drive: bool,
    /// `CRATONVM_G1_MARK_SIDE_TABLES` — [`parse::non_empty_non_zero`].
    ///
    /// Snapshot the three class-loader side-table KEY SETS once per marking
    /// batch instead of asking each registry about every scanned object.
    ///
    /// Off (today): the tail of `scan_object_refs` calls
    /// `loader_pin::loader_pin_addr`, `mirror_pin::mirrors_for_loader` and
    /// `metadata_pin::roots_for_loader` per object. Each has a `NON_EMPTY`
    /// latch, so this is three relaxed loads on a run with no custom
    /// `ClassLoader` — and a process-global `RwLock` acquisition plus (for two
    /// of them) a heap-allocating `Vec` clone per object per marking worker
    /// the moment one exists, which is every Spring/Tomcat/Hibernate process.
    pub g1_mark_side_tables: bool,
    /// `CRATONVM_G1_CLEANUP_SATB_DISCARD` — [`parse::non_empty_non_zero`].
    ///
    /// End the cycle with `SatbQueue::deactivate_and_discard()` rather than
    /// `deactivate_and_drain()`, whose returned `Vec` `cleanup` drops.
    ///
    /// Off (today): the cleanup pause pays a global SATB registry walk —
    /// `SATB_BUFFER_REGISTRY`'s mutex, every registered mutator's buffer lock,
    /// then every orphaned buffer — to concatenate entries into a `Vec` that
    /// is immediately dropped. The discard form keeps the same barrier and the
    /// same "no entry survives the INACTIVE store" guarantee.
    pub g1_cleanup_satb_discard: bool,
    /// `CRATONVM_G1_CLEANUP_RESET_TAMS` — [`parse::non_empty_non_zero`].
    ///
    /// Clear `mark_start` / `mark_start_snapshot` at the end of `cleanup`, the
    /// way `abort_concurrent_mark` already does.
    ///
    /// Off (today): a completed cycle leaves its TAMS behind, so a second bare
    /// `cleanup()` with no intervening `start_concurrent_mark` runs against the
    /// previous cycle's snapshot. The direction is safe — a region recycled
    /// since the snapshot fails the epoch/type match and is treated as wholly
    /// live — but it is state outliving the cycle that defines it.
    pub g1_cleanup_reset_tams: bool,
    /// `CRATONVM_G1_MARK_CONVERGENCE_EPOCH` — [`parse::non_empty_non_zero`].
    ///
    /// Read the concurrent marker's completion signal as a statement about the
    /// CYCLE — every worker parked at a fixed point, and no work published
    /// since the step that found it — instead of as one shared `AtomicBool`.
    ///
    /// Off (today): `ConcurrentMarkState::quiesced` records whatever the last
    /// worker to write it thought, so a peer returning from a park clears a
    /// convergence that is still true and the coordinator polls again for
    /// nothing. The direction is safe (a spurious `false` only delays
    /// completion), which is why this is a latency and clarity change rather
    /// than a correctness one — and why it moves on a measurement.
    ///
    /// This is the measurable core of proposal P1, not the whole of it: the
    /// `WORKER_POLL_MS` fallback park and the per-step `gray_set_is_empty()`
    /// probe both remain.
    pub g1_mark_convergence_epoch: bool,

    /// `CRATONVM_G1_MIXED_FUTILE_GUARD` — LANE W7-L, default **ON** since
    /// 2026-09-21. `=0` restores the unguarded behaviour.
    ///
    /// Arms the breaker that refuses a mixed phase after
    /// `MIXED_FUTILE_STREAK_TRIP` consecutive mixed pauses reclaimed less
    /// than `MIXED_PRODUCTIVE_RATIO_PCT` of what they copied. With `=0` the
    /// counters behind the breaker are still maintained, but nothing is
    /// refused.
    ///
    /// # Why this defaults ON, and why it is paired with `ALLOC_MARK_DRIVE`
    ///
    /// It is a LIVENESS fix, not a tuning knob: unguarded, the VM either hangs
    /// or dies of `OutOfMemoryError` on a legitimate workload. Measured on
    /// `G1MixedRepeatProbe 12 6 48 24 4096 0` at `-Xmx192m`, IHOP 25, one
    /// binary, interleaved arms, three batches:
    ///
    /// | build | runs that failed |
    /// |---|---|
    /// | `ALLOC_MARK_DRIVE=0` (pre-wave-7) | **6/6** — never marks, always OOMs |
    /// | wave 7, this guard off | **10/22** (4 OOM, 6 hang) |
    /// | wave 7, this guard on | **0/14** |
    ///
    /// The mechanism is not inferred from the completion counts: the breaker
    /// trips in **3 of 6** armed runs (`streak=4`, `refused=` 15/9/11) against
    /// a **3 of 6** hang rate unarmed, so the runs it acts on are the runs
    /// that would have looped. In a hung run every mixed pause reads
    /// `freed/copied ≈ 1.0002` — ~20 MB copied and ~20 MB reclaimed, with
    /// `old_regions` and `free_regions` frozen across hundreds of consecutive
    /// pauses.
    ///
    /// `CRATONVM_G1_ALLOC_MARK_DRIVE` is what makes a mixed pause reachable on
    /// a default build at all, so before it this loop could not occur — and
    /// neither could any reclamation, which is why the pre-wave-7 row is worse
    /// than the unguarded one. The two flags are a PAIR: turning
    /// `ALLOC_MARK_DRIVE` off while leaving this on restores a build that
    /// never marks.
    ///
    /// See `G1Collector::mixed_futile_breaker_allows_within` in
    /// `gc/src/g1.rs` and
    /// `docs/feature-designs/concurrent-gc-maturation.md`.
    pub g1_mixed_futile_guard: bool,
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
    /// `CRATONVM_GC_HANDSHAKE_AT_HELPER_ENTRY` — a JIT helper's entry door
    /// (`vm::jit::helpers::jit_safepoint_flush_satb`, at the allocation and
    /// dispatch helpers) runs the calling thread's queued per-thread
    /// handshakes, so a compiled peer serves a cross-thread frame trace
    /// (`CRATONVM_GC_HANDSHAKE_FRAME_TRACE`) without the frame-trace pause.
    /// **Default ON**; `0` / `false` serve them at `safepoint_check` only
    /// (JIT round 23 wave 6, the proxy-define page).
    /// [`parse::on_unless_zero_or_false`].
    pub handshake_at_helper_entry: bool,
}

/// gen r5w4/defaults8 — the per-backend switches, resolved for one heap.
/// `generational` is the asking VM's `VmHeap::is_generational()`; see
/// [`alloc_policy_defaults::BackendDefault::resolve`].
impl GcFlags {
    /// Is the HotSpot-shaped TLAB share sizer on for a heap of this family?
    /// (`CRATONVM_TLAB_SHARE_SIZER`, default [`alloc_policy_defaults::TLAB_SHARE_SIZER`].)
    #[inline]
    pub fn tlab_share_sizer_for(&self, generational: bool) -> bool {
        alloc_policy_defaults::TLAB_SHARE_SIZER.resolve(self.tlab_share_sizer, generational)
    }

    /// gci i48/zl3: [`Self::tlab_share_sizer_for`] with the G1 heap told apart
    /// from ZGC -- `g1` resolves an unset switch to
    /// [`alloc_policy_defaults::TLAB_SHARE_SIZER_G1`]; otherwise exactly
    /// `tlab_share_sizer_for(generational)` (Generational and ZGC unchanged).
    /// An explicit value wins everywhere (`=0` is the G1 kill switch too).
    #[inline]
    pub fn tlab_share_sizer_for_backend(&self, generational: bool, g1: bool) -> bool {
        match self.tlab_share_sizer {
            Some(on) => on,
            None if g1 => alloc_policy_defaults::TLAB_SHARE_SIZER_G1,
            None => self.tlab_share_sizer_for(generational),
        }
    }

    /// Does the soft-reference LRU policy take HotSpot's inputs on a heap of
    /// this family? (`CRATONVM_SOFTREF_HOTSPOT_LRU`, default
    /// [`alloc_policy_defaults::SOFTREF_HOTSPOT_LRU`].)
    #[inline]
    pub fn softref_hotspot_lru_for(&self, generational: bool) -> bool {
        alloc_policy_defaults::SOFTREF_HOTSPOT_LRU.resolve(self.softref_hotspot_lru, generational)
    }

    /// zr z1/rf: [`Self::softref_hotspot_lru_for`] with the ZGC heap told
    /// apart from G1 — `zgc` resolves an unset switch to
    /// [`alloc_policy_defaults::SOFTREF_HOTSPOT_LRU_ZGC`]; otherwise exactly
    /// `softref_hotspot_lru_for(generational)` (G1 and Generational unchanged).
    /// An explicit value wins everywhere.
    #[inline]
    pub fn softref_hotspot_lru_for_backend(&self, generational: bool, zgc: bool) -> bool {
        match self.softref_hotspot_lru {
            Some(on) => on,
            None if zgc => alloc_policy_defaults::SOFTREF_HOTSPOT_LRU_ZGC,
            None => self.softref_hotspot_lru_for(generational),
        }
    }

    /// gcf f4/r (`gengc-r5w4-defaults8-proposal-print-the-effective-heap-policy`):
    /// the `[GC] policy:` line `VmHeap::print_gc_summary` prints first -- every
    /// allocation / heap policy switch with the value that ran (`on` / `off`)
    /// and where it came from (`set` in the environment, else `default`).
    /// `generational` resolves the per-backend defaults as their consumers do;
    /// `env` is the environment the switches were read from (raw value, `None`
    /// when unset). The three switches read outside `GcFlags` resolve with
    /// `runtime_flag_default_on`'s rule (off only for `0` / `false` / `off` /
    /// `no`). A new `alloc_policy_defaults` constant must be added here: the
    /// unit test `gcf_f4r_policy_line_names_every_switch_and_its_origin` names
    /// them all.
    pub fn policy_line(
        &self,
        backend: &str,
        generational: bool,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> String {
        let default_on = |key: &str| match env(key) {
            Some(v) => {
                let v = v.trim().to_ascii_lowercase();
                !matches!(v.as_str(), "0" | "false" | "off" | "no")
            }
            None => true,
        };
        let arms: [(&str, bool); 18] = [
            // gci i48/zl3: the G1 line resolves the G1 default.
            (
                "CRATONVM_TLAB_SHARE_SIZER",
                self.tlab_share_sizer_for_backend(generational, backend == "g1"),
            ),
            // jr20 wave 9 / gcE: the VM latches the Generational default on
            // every backend (`vm_init.rs`, `ReferenceProcessor::new_for_heap`).
            (
                "CRATONVM_SOFTREF_HOTSPOT_LRU",
                self.softref_hotspot_lru_for_backend(true, backend == "zgc"),
            ),
            ("CRATONVM_TLAB_FILLER_SKIP_ZERO", self.tlab_filler_skip_zero),
            ("CRATONVM_TLAB_GATE_BUMP_FLOOR", self.tlab_gate_bump_floor),
            ("CRATONVM_GEN_XMS_USABLE_FIRST", self.gen_xms_usable_first),
            ("CRATONVM_GEN_ZERO_ONCE", self.gen_zero_once),
            (
                "CRATONVM_GEN_ZERO_ONCE_FREE_LIST",
                self.gen_zero_once_free_list,
            ),
            (
                "CRATONVM_GEN_HUMONGOUS_ZERO_UNLOCKED",
                self.gen_humongous_zero_unlocked,
            ),
            (
                "CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED",
                self.gen_humongous_ref_zero_unlocked,
            ),
            ("CRATONVM_GEN_TLAB_TAIL_SINK", self.gen_tlab_tail_sink),
            ("CRATONVM_GEN_SATB_OLD_ONLY", self.gen_satb_old_only),
            ("CRATONVM_GEN_CONC_CLASS_UNLOAD", self.gen_conc_class_unload),
            (
                "CRATONVM_GEN_CONC_INLINE_START",
                default_on("CRATONVM_GEN_CONC_INLINE_START"),
            ),
            ("CRATONVM_GC_OLD_HUMONGOUS_TOP", self.old_humongous_top),
            (
                "CRATONVM_GC_FULL_GC_TRUE_ROOTS",
                default_on("CRATONVM_GC_FULL_GC_TRUE_ROOTS"),
            ),
            (
                "CRATONVM_GC_MOVING_MAJOR_JIT_GUARD",
                default_on("CRATONVM_GC_MOVING_MAJOR_JIT_GUARD"),
            ),
            (
                "CRATONVM_GC_FUTILE_YOUNG_BACKOFF",
                self.futile_young_backoff,
            ),
            ("CRATONVM_GC_OVERHEAD_PROGRESS", self.gc_overhead_progress),
        ];
        let mut line = format!("[GC] policy: backend={backend}");
        for (key, on) in arms {
            let token = key.trim_start_matches("CRATONVM_").to_ascii_lowercase();
            let state = if on { "on" } else { "off" };
            let origin = if env(key).is_some() { "set" } else { "default" };
            line.push_str(&format!(" {token}={state}({origin})"));
        }
        line
    }
}

impl GcFlags {
    fn from_source(src: &dyn FlagSource) -> Self {
        use parse::*;
        Self {
            handshake_at_helper_entry: on_unless_zero_or_false(
                src,
                "CRATONVM_GC_HANDSHAKE_AT_HELPER_ENTRY",
            ),
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
            // gci i4/fl (2026-10-12): every opt-in boolean below that used to
            // be `present` (set to anything = on, so `=0` ran the treatment
            // arm) is `truthy_word`: unset, empty, `0`, `false`, `off`, `no`
            // are off. No default moves (every one of them is unset by
            // default). `moving_young` above keeps its presence pair on
            // purpose: it is paired with the JIT's publication gate. Pinned
            // by `tests::gci_i4fl_*`.
            moving_young_fallbacks: truthy_word(src, "CRATONVM_MOVING_YOUNG_FALLBACKS"),
            no_gc_promotion_guard: truthy_word(src, "CRATONVM_NO_GC_PROMOTION_GUARD"),
            promotion_oom_guard_broad: truthy_word(src, "CRATONVM_PROMOTION_OOM_GUARD_BROAD"),
            no_selective_promote: truthy_word(src, "CRATONVM_NO_SELECTIVE_PROMOTE"),
            sp_no_coalesce: truthy_word(src, "CRATONVM_SP_NO_COALESCE"),
            no_defrag_promote: truthy_word(src, "CRATONVM_NO_DEFRAG_PROMOTE"),
            card_table_only: truthy_word(src, "CRATONVM_CARD_TABLE_ONLY"),
            full_rset_scan: truthy_word(src, "CRATONVM_GC_FULL_RSET_SCAN"),
            verify_rset: truthy_word(src, "CRATONVM_GC_VERIFY_RSET"),
            gc_sync_young_wipe: truthy_word(src, "CRATONVM_GC_SYNC_YOUNG_WIPE"),
            gc_jit_ref_store_gates: on_unless_zero(src, "CRATONVM_GC_JIT_REF_STORE_GATES"),
            gc_card_summary: on_unless_zero(src, "CRATONVM_GC_CARD_SUMMARY"),
            gc_old_bot: on_unless_zero(src, "CRATONVM_GC_OLD_BOT"),
            gc_precise_array_cards: on_unless_zero(src, "CRATONVM_GC_PRECISE_ARRAY_CARDS"),
            old_sweep_jit: on_unless_zero(src, "CRATONVM_OLD_SWEEP_JIT"),
            g1_parallel_evac: on_unless_zero(src, "CRATONVM_G1_PARALLEL_EVAC"),
            g1_parallel_evac_screen: on_unless_zero(src, "CRATONVM_G1_PARALLEL_EVAC_SCREEN"),
            g1_serial_evac_holder_screen: on_unless_zero(
                src,
                "CRATONVM_G1_SERIAL_EVAC_HOLDER_SCREEN",
            ),
            g1_verify_forwards_retired: non_empty_non_zero(
                src,
                "CRATONVM_G1_VERIFY_FORWARDS_RETIRED",
            ),
            g1_evac_copy_watch: truthy_word(src, "CRATONVM_G1_EVAC_COPY_WATCH"),
            g1_ref_write_watch: truthy_word(src, "CRATONVM_G1_REF_WRITE_WATCH"),
            g1_evac_supply_screen: on_unless_zero(src, "CRATONVM_G1_EVAC_SUPPLY_SCREEN"),
            // Lane W7-W (2026-09-21) — opt-in; see the field doc.
            g1_evac_share_census: truthy_word(src, "CRATONVM_G1_EVAC_SHARE_CENSUS"),
            g1_evac_candidate_arena_screen: on_unless_zero(
                src,
                "CRATONVM_G1_EVAC_CANDIDATE_ARENA_SCREEN",
            ),
            g1_evac_empty_header_grid_proof: on_unless_zero(
                src,
                "CRATONVM_G1_EVAC_EMPTY_HEADER_GRID_PROOF",
            ),
            g1_parallel_evac_shared_dest: on_unless_zero(
                src,
                "CRATONVM_G1_PARALLEL_EVAC_SHARED_DEST",
            ),
            g1_parallel_evac_resume_dest: on_unless_zero(
                src,
                "CRATONVM_G1_PARALLEL_EVAC_RESUME_DEST",
            ),
            g1_evac_ref_implausible_refuse: non_empty_non_zero(
                src,
                "CRATONVM_G1_EVAC_REF_IMPLAUSIBLE_REFUSE",
            ),
            g1_parallel_evac_in_jit: on_unless_zero(src, "CRATONVM_G1_PARALLEL_EVAC_IN_JIT"),
            g1_eager_humongous: on_unless_zero(src, "CRATONVM_G1_EAGER_HUMONGOUS"),
            g1_young_pause_target: on_unless_zero(src, "CRATONVM_G1_YOUNG_PAUSE_TARGET"),
            g1_scrub_free: truthy_word(src, "CRATONVM_G1_SCRUB_FREE"),
            g1_narrow_fixup: on_unless_zero(src, "CRATONVM_G1_NARROW_FIXUP"),
            g1_cleanup_walk: truthy_word(src, "CRATONVM_G1_CLEANUP_WALK"),
            g1_adaptive_ihop: on_unless_zero(src, "CRATONVM_G1_ADAPTIVE_IHOP"),
            g1_full_compaction: on_unless_zero(src, "CRATONVM_G1_FULL_COMPACTION"),
            g1_adaptive_tenuring: on_unless_zero(src, "CRATONVM_G1_ADAPTIVE_TENURING"),
            g1_reserve_heap: on_unless_zero(src, "CRATONVM_G1_RESERVE_HEAP"),
            g1_uncommit: truthy_word(src, "CRATONVM_G1_UNCOMMIT"),
            gen_uncommit: on_unless_zero(src, "CRATONVM_GEN_UNCOMMIT"),
            // gen r4w4/alloc4: every allocation-policy arm parses against its
            // default in `alloc_policy_defaults` — a default flip is one line
            // there, not a parser change here.
            tlab_filler_skip_zero: alloc_policy_switch(
                src,
                "CRATONVM_TLAB_FILLER_SKIP_ZERO",
                alloc_policy_defaults::TLAB_FILLER_SKIP_ZERO,
            ),
            tlab_gate_bump_floor: alloc_policy_switch(
                src,
                "CRATONVM_TLAB_GATE_BUMP_FLOOR",
                alloc_policy_defaults::TLAB_GATE_BUMP_FLOOR,
            ),
            // gen r5w4/defaults8: explicit setting only; the default is per
            // backend (`GcFlags::tlab_share_sizer_for`).
            tlab_share_sizer: backend_policy_switch(src, "CRATONVM_TLAB_SHARE_SIZER"),
            gen_zero_once: alloc_policy_switch(
                src,
                "CRATONVM_GEN_ZERO_ONCE",
                alloc_policy_defaults::GEN_ZERO_ONCE,
            ),
            gen_zero_once_free_list: on_unless_zero(src, "CRATONVM_GEN_ZERO_ONCE_FREE_LIST"),
            gen_humongous_zero_unlocked: alloc_policy_switch(
                src,
                "CRATONVM_GEN_HUMONGOUS_ZERO_UNLOCKED",
                alloc_policy_defaults::GEN_HUMONGOUS_ZERO_UNLOCKED,
            ),
            gen_tlab_tail_sink: alloc_policy_switch(
                src,
                "CRATONVM_GEN_TLAB_TAIL_SINK",
                alloc_policy_defaults::GEN_TLAB_TAIL_SINK,
            ),
            gen_humongous_ref_zero_unlocked: alloc_policy_switch(
                src,
                "CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED",
                alloc_policy_defaults::GEN_HUMONGOUS_REF_ZERO_UNLOCKED,
            ),
            g1_card_rset: on_unless_zero(src, "CRATONVM_G1_CARD_RSET"),
            g1_card_clean: truthy_word(src, "CRATONVM_G1_CARD_CLEAN"),
            g1_card_screen_jit_pinned: on_unless_zero(src, "CRATONVM_G1_CARD_SCREEN_JIT_PINNED"),
            g1_inline_barrier: on_unless_zero(src, "CRATONVM_G1_INLINE_BARRIER"),
            g1_mark_lock_yield: on_unless_zero(src, "CRATONVM_G1_MARK_LOCK_YIELD"),
            g1_shared_alloc: on_unless_zero(src, "CRATONVM_G1_SHARED_ALLOC"),
            g1_eden_stripes: usize_min1(src, "CRATONVM_G1_EDEN_STRIPES"),
            g1_parallel_mark: on_unless_zero(src, "CRATONVM_G1_PARALLEL_MARK"),
            g1_dbg_rset: truthy_word(src, "CRATONVM_G1_DBG_RSET"),
            g1_no_evac_retry: truthy_word(src, "CRATONVM_G1_NO_EVAC_RETRY"),
            g1_retire_forwards_late: on_unless_zero(src, "CRATONVM_G1_RETIRE_FORWARDS_LATE"),
            g1_retire_kept_only: on_unless_zero(src, "CRATONVM_G1_RETIRE_KEPT_ONLY"),
            g1_reevac_guard: on_unless_zero(src, "CRATONVM_G1_REEVAC_GUARD"),
            g1_coverage_pin: truthy_word(src, "CRATONVM_G1_COVERAGE_PIN"),
            g1_pin_empty_publication: truthy_word(src, "CRATONVM_G1_PIN_EMPTY_PUBLICATION"),
            g1_workers: usize_min1(src, "CRATONVM_G1_WORKERS"),
            gc_sweep_anchor_stride: usize_opt(src, "CRATONVM_GC_SWEEP_ANCHOR_STRIDE")
                .filter(|&n| n >= 64)
                .unwrap_or(8 * 1024 * 1024),
            gc_par_threads: usize_opt(src, "CRATONVM_GC_PAR_THREADS"),
            gc_par_min_bytes: usize_opt(src, "CRATONVM_GC_PAR_MIN_BYTES")
                .unwrap_or(16 * 1024 * 1024),
            gc_par_evac: on_unless_zero(src, "CRATONVM_GC_PAR_EVAC"),
            gc_par_evac_card_seed: truthy_word_default_true(src, "CRATONVM_GC_PAR_EVAC_CARD_SEED"),
            // WARNING gci-user-flip-20261013: default flipped ON by the user's decision, pending apps triage (the pinned copy's gated companion).
            gen_pinned_young_copy_parallel: truthy_word_default_true(
                src,
                "CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL",
            ),
            gen_pinned_tail_window_live: truthy_word(src, "CRATONVM_GEN_PINNED_TAIL_WINDOW_LIVE"),
            gc_system_gc_moving_young: truthy_word(src, "CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG"),
            gc_adaptive_tenuring: on_unless_zero(src, "CRATONVM_GC_ADAPTIVE_TENURING"),
            // WARNING gci-user-flip-20261013: default flipped ON by the user's decision, pending apps triage.
            gen_pinned_young_copy: truthy_word_default_true(src, "CRATONVM_GEN_PINNED_YOUNG_COPY"),
            gc_stress_bytes: utf8(src, "CRATONVM_DBG_GC_STRESS")
                .or_else(|| utf8(src, "CRATONVM_GC_STRESS"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|&v| v > 0),
            dbg_force_moving: truthy_word(src, "CRATONVM_DBG_FORCE_MOVING"),
            dbg_no_nonmoving_reclaim: truthy_word(src, "CRATONVM_DBG_NO_NONMOVING_RECLAIM"),
            dbg_compact_legacy: truthy_word(src, "CRATONVM_DBG_COMPACT_LEGACY"),
            dbg_seed_all_old: truthy_word(src, "CRATONVM_DBG_SEED_ALL_OLD"),
            dbg_stale_objref: truthy_word(src, "CRATONVM_DBG_STALE_OBJREF"),
            dbg_stale_objref_cycles: usize_opt_untrimmed(src, "CRATONVM_DBG_STALE_OBJREF_CYCLES")
                .filter(|&n| n >= 1)
                .unwrap_or(1),
            dbg_blocked_access: match utf8(src, "CRATONVM_DBG_BLOCKED_ACCESS") {
                Some(ref v) if v == "warn" => BlockedAccessMode::Warn,
                // gci i4/fl: the value rule (`false` / `off` / `no` were Panic).
                Some(ref v) if word_is_on(v) => BlockedAccessMode::Panic,
                _ => BlockedAccessMode::Off,
            },

            fwd_resolve_strict: truthy_word(src, "CRATONVM_FWD_RESOLVE_STRICT"),
            moving_young_verify: truthy_word(src, "CRATONVM_MOVING_YOUNG_VERIFY"),
            sp_verify: truthy_word(src, "CRATONVM_SP_VERIFY"),
            gc_array_guard_bt: truthy_word(src, "CRATONVM_GC_ARRAY_GUARD_BT"),

            dbg_a2: truthy_word(src, "CRATONVM_DBG_A2"),
            dbg_sweep_liveness: truthy_word(src, "CRATONVM_DBG_SWEEP_LIVENESS"),
            // The value only when the gate is on, so `=0` cannot reach a
            // consumer that tests the value alone.
            dbg_sweep_liveness_value: utf8(src, "CRATONVM_DBG_SWEEP_LIVENESS")
                .filter(|v| word_is_on(v)),
            dbg_badref: truthy_word(src, "CRATONVM_DBG_BADREF"),
            dbg_deadref_store: truthy_word(src, "CRATONVM_DBG_DEADREF_STORE"),
            dbg_cellcorrupt: truthy_word(src, "CRATONVM_DBG_CELLCORRUPT"),
            dbg_desctrace: truthy_word(src, "CRATONVM_DBG_DESCTRACE"),
            dbg_fwdguard: truthy_word(src, "CRATONVM_DBG_FWDGUARD"),
            dbg_gcpause: truthy_word(src, "CRATONVM_DBG_GCPAUSE"),
            dbg_gcpause_min_us: utf8(src, "CRATONVM_DBG_GCPAUSE_MIN_US")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(100_000),
            dbg_gcphase: truthy_word(src, "CRATONVM_DBG_GCPHASE"),
            dbg_gcwrite: truthy_word(src, "CRATONVM_DBG_GCWRITE"),
            dbg_heap_trace: truthy_word(src, "CRATONVM_DBG_HEAP_TRACE"),
            dbg_mirrorpin: truthy_word(src, "CRATONVM_DBG_MIRRORPIN"),
            dbg_oobfield: truthy_word(src, "CRATONVM_DBG_OOBFIELD"),
            // As `dbg_sweep_liveness_value`: `gen_heap`'s write-side guard
            // tests `dbg_oobfield_value.is_some()` without the gate.
            dbg_oobfield_value: utf8(src, "CRATONVM_DBG_OOBFIELD").filter(|v| word_is_on(v)),
            dbg_precise: truthy_word(src, "CRATONVM_DBG_PRECISE"),
            dbg_rset_audit: truthy_word(src, "CRATONVM_DBG_RSET_AUDIT"),
            dbg_rset_audit_young_scan: truthy_word(src, "CRATONVM_DBG_RSET_AUDIT_YOUNG_SCAN"),
            dbg_seedhunt: truthy_word(src, "CRATONVM_DBG_SEEDHUNT"),
            dbg_sweep_census: truthy_word(src, "CRATONVM_DBG_SWEEP_CENSUS"),
            dbg_sweep_edges: truthy_word(src, "CRATONVM_DBG_SWEEP_EDGES"),
            dbg_sweep_zero: truthy_word(src, "CRATONVM_DBG_SWEEP_ZERO"),
            late_resolve_dropped: truthy_word(src, "CRATONVM_GC_LATE_RESOLVE_DROPPED"),
            dbg_watchref: truthy_word(src, "CRATONVM_DBG_WATCHREF"),
            dbg_watch_cell: hex_addr_or_zero(src, "CRATONVM_DBG_WATCH_CELL"),
            dbg_youngstate: truthy_word(src, "CRATONVM_DBG_YOUNGSTATE"),
            dbg_zero_ranges: truthy_word(src, "CRATONVM_DBG_ZERO_RANGES"),
            diag_hib32: truthy_word(src, "CRATONVM_DIAG_HIB32"),
            no_exact_refproc_survival: truthy_word(src, "CRATONVM_NO_EXACT_REFPROC_SURVIVAL"),
            no_referent_identity_screen: truthy_word(src, "CRATONVM_NO_REFERENT_IDENTITY_SCREEN"),
            no_oldgen_coalesce: truthy_word(src, "CRATONVM_NO_OLDGEN_COALESCE"),
            // gci i4/fl: the Generational opt-ins below moved from
            // `non_empty_non_zero` (`=false` / `=off` / `=no` were ON) to the
            // same value rule; unset, empty and `0` read exactly as before.
            old_trigger_hysteresis: truthy_word(src, "CRATONVM_GC_OLD_TRIGGER_HYSTERESIS"),
            old_trigger_hysteresis_explicit: utf8(src, "CRATONVM_GC_OLD_TRIGGER_HYSTERESIS")
                .map(|v| word_is_on(&v)),
            old_no_concurrent_first: truthy_word(src, "CRATONVM_GC_NO_CONCURRENT_FIRST"),
            conc_start_percent: usize_opt(src, "CRATONVM_GC_CONC_START_PERCENT").filter(|&p| p > 0),
            gen_conc_mark_no_local_stack: truthy_word(src, "CRATONVM_GEN_CONC_MARK_NO_LOCAL_STACK"),
            gen_conc_no_frozen_eager_scan: truthy_word(
                src,
                "CRATONVM_GEN_CONC_NO_FROZEN_EAGER_SCAN",
            ),
            gen_conc_service_thread: truthy_word(src, "CRATONVM_GEN_CONC_SERVICE_THREAD"),
            gen_conc_remark_refproc_hook: on_unless_zero(
                src,
                "CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK",
            ),
            gen_satb_old_only: on_unless_zero(src, "CRATONVM_GEN_SATB_OLD_ONLY"),
            gen_conc_alloc_fail_door: on_unless_zero(src, "CRATONVM_GEN_CONC_ALLOC_FAIL_DOOR"),
            gen_conc_class_unload: on_unless_zero(src, "CRATONVM_GEN_CONC_CLASS_UNLOAD"),
            // gen r5w4/defaults8: explicit setting only; the default is per
            // backend (`GcFlags::softref_hotspot_lru_for`).
            softref_hotspot_lru: backend_policy_switch(src, "CRATONVM_SOFTREF_HOTSPOT_LRU"),
            old_give_back: truthy_word(src, "CRATONVM_GC_OLD_GIVE_BACK"),
            old_shrink: on_unless_zero(src, "CRATONVM_GC_OLD_SHRINK"),
            old_frag_compact: truthy_word(src, "CRATONVM_GC_OLD_FRAG_COMPACT"),
            old_oom_compact: on_unless_zero(src, "CRATONVM_GC_OLD_OOM_COMPACT"),
            old_walk_gap_recovery: on_unless_zero(src, "CRATONVM_GC_OLD_WALK_GAP_RECOVERY"),
            gen_young_pin_ledger_term4: truthy_word(src, "CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4"),
            gen_precise_root_promote: truthy_word(src, "CRATONVM_GEN_PRECISE_ROOT_PROMOTE"),
            old_pinned_compact: truthy_word(src, "CRATONVM_GC_OLD_PINNED_COMPACT"),
            old_frag_pinned_compact: on_unless_zero(src, "CRATONVM_GC_OLD_FRAG_PINNED_COMPACT"),
            // WARNING gci-user-flip-20261013: default flipped ON by the user's decision, pending apps triage.
            oldgen_compact: truthy_word_default_true(src, "CRATONVM_OLDGEN_COMPACT"),
            g1_only_jit_pins: truthy_word(src, "CRATONVM_GC_G1_ONLY_JIT_PINS"),
            gc_overhead_progress: on_unless_off_word_cased(src, "CRATONVM_GC_OVERHEAD_PROGRESS"),
            monitor_enter_spin: on_unless_off_word_cased(src, "CRATONVM_MONITOR_ENTER_SPIN"),
            // gcd d2/j: default ON, `=0` off; opt-in. See the field docs.
            futile_young_backoff: on_unless_zero(src, "CRATONVM_GC_FUTILE_YOUNG_BACKOFF"),
            preallocated_oome_kinds: on_unless_zero(src, "CRATONVM_GC_PREALLOCATED_OOME_KINDS"),
            old_shrink_after_concurrent: truthy_word(
                src,
                "CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT",
            ),
            conc_walk_gap_recovery: truthy_word(src, "CRATONVM_GC_CONC_WALK_GAP_RECOVERY"),
            // gen r5w3/oldgen7: opt-in, see the field docs — except the
            // `-Xms` split, default ON since gen r5w4/defaults8 and parsed
            // against its constant like the allocation-policy arms.
            gen_xms_usable_first: alloc_policy_switch(
                src,
                "CRATONVM_GEN_XMS_USABLE_FIRST",
                alloc_policy_defaults::GEN_XMS_USABLE_FIRST,
            ),
            old_interior_decommit: truthy_word(src, "CRATONVM_GC_OLD_INTERIOR_DECOMMIT"),
            old_borrow_young: non_empty_non_zero_default_true(src, "CRATONVM_GC_OLD_BORROW_YOUNG"),
            old_humongous_top: on_unless_off_word_cased(src, "CRATONVM_GC_OLD_HUMONGOUS_TOP"),
            g1_dbg_headers: truthy_word(src, "CRATONVM_G1_DBG_HEADERS"),
            g1_dbg_diag: truthy_word(src, "CRATONVM_DBG_G1DIAG"),
            g1_dbg_accessor: truthy_word(src, "CRATONVM_DBG_G1ACCESSOR"),
            g1_dbg_pins: truthy_word(src, "CRATONVM_G1_DBG_PINS"),
            g1_humongous_marks: truthy_word(src, "CRATONVM_G1_HUMONGOUS_MARKS"),
            g1_ihop_counts_regions: truthy_word(src, "CRATONVM_G1_IHOP_COUNTS_REGIONS"),
            // --- lane W2-C (2026-09-20) --- all five opt-in; see the field docs.
            g1_ihop_gross_growth: truthy_word(src, "CRATONVM_G1_IHOP_GROSS_GROWTH"),
            g1_ihop_backoff: truthy_word(src, "CRATONVM_G1_IHOP_BACKOFF"),
            // --- lane W5-C (2026-09-21), the back-off's counted fail-safe ---
            g1_mark_backoff_deadline: truthy_word(src, "CRATONVM_G1_MARK_BACKOFF_DEADLINE"),
            g1_ihop_poll_gate: truthy_word(src, "CRATONVM_G1_IHOP_POLL_GATE"),
            // --- lane W7-D (2026-09-21), the cycle no pause closed ---
            //
            // `on_unless_zero`, not `non_empty_non_zero`: this is a kill
            // switch over a fix, not an opt-in over a feature. House rule 3
            // ("`non_empty_non_zero`, never `present`") is about a flag whose
            // `=0` must mean OFF; it does here too — `on_unless_zero` treats
            // exactly `"0"` as off, which is the same value rule read from the
            // other default. Same helper as `g1_parallel_evac_in_jit` and
            // `g1_eager_humongous`, the two other G1 defaults that ship on.
            g1_alloc_mark_drive: on_unless_zero(src, "CRATONVM_G1_ALLOC_MARK_DRIVE"),
            g1_mark_side_tables: truthy_word(src, "CRATONVM_G1_MARK_SIDE_TABLES"),
            g1_cleanup_satb_discard: truthy_word(src, "CRATONVM_G1_CLEANUP_SATB_DISCARD"),
            g1_cleanup_reset_tams: truthy_word(src, "CRATONVM_G1_CLEANUP_RESET_TAMS"),
            g1_mark_convergence_epoch: non_empty_non_zero(
                src,
                "CRATONVM_G1_MARK_CONVERGENCE_EPOCH",
            ),
            g1_mixed_futile_guard: on_unless_zero(src, "CRATONVM_G1_MIXED_FUTILE_GUARD"),
            g1_verify_holders: truthy_word(src, "CRATONVM_G1_VERIFY_HOLDERS"),
            g1_dbg_reach: truthy_word(src, "CRATONVM_G1_DBG_REACH"),
            g1_dbg_rootcensus: truthy_word(src, "CRATONVM_G1_DBG_ROOTCENSUS"),
            g1_dbg_zero: truthy_word(src, "CRATONVM_G1_DBG_ZERO"),
            sp_stats: truthy_word(src, "CRATONVM_SP_STATS"),
            sp_trace: truthy_word(src, "CRATONVM_SP_TRACE"),
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
    /// rather than a crate-private cache. Opt-in by value
    /// ([`parse::truthy_word`], gci i4/fl): `=0` is off.
    pub shadow_stack: bool,
    /// `CRATONVM_DBG_JIT_METHOD_STATS=1` — emit the tier/promotion summary
    /// exactly once during controlled VM shutdown. Kept in the shared snapshot
    /// so the supported `CRATONVM_DBG=jit-method-stats` spelling and the legacy
    /// spelling cannot diverge at the launcher.
    pub method_stats: bool,
    /// `CRATONVM_JIT_INLINE_CARD_MARK` — gen r4w4/cards4 (2026-09-24): the x64
    /// JIT emits the generational post barrier inline on old receivers — a
    /// card check before AND after every compiled reference store, and a call
    /// to the collector's barrier only for a clean card — instead of a helper
    /// call per store. Default **OFF** ([`parse::non_empty_non_zero`]) this
    /// wave; the A/B lever for `jit/src/x64/objects.rs::emit_gen_card_barrier`.
    /// Read per compiled store site. Inert under G1 and ZGC, which publish no
    /// generational card view.
    pub inline_card_mark: bool,
    /// `CRATONVM_JIT_OSR_ENTRY_TIERUP` — JIT round 16 (lane coldpath): an
    /// activation that ENTERS an OSR body credits the back edges it interpreted
    /// to its method's tier-up counter, so a method whose work is one hot loop
    /// per call earns its whole-method compile instead of being OSR-entered on
    /// every call until 500 plain invocations. **Default ON**; `0` turns it off
    /// ([`parse::on_unless_zero`]). Read by `vm/src/runtime/interpreter.rs`
    /// (`osr_entry_tierup_armed`) once per OSR entry.
    pub osr_entry_tierup: bool,
    /// `CRATONVM_JIT_IR_DEOPT_NULL_CONST_AS_OBJECT` — JIT round 23 wave 5: a
    /// deopt frame state describes the `Ref` null constant as
    /// `FrameValue::Object(0)` instead of `Int(0)` (which every resume sink
    /// rebuilt as an `int` in a reference slot). Correctness, **default ON**;
    /// `0` restores the old description ([`parse::on_unless_zero`]). Read by
    /// `jit/src/ir_lower.rs` (`deopt_null_const_as_object`) per frame value at
    /// compile time.
    pub deopt_null_const_as_object: bool,

    /// `CRATONVM_JIT_FFM_DIRECT_DOOR_EXACT_ONLY` — JIT round 21, lane native
    /// (FFL16-1 / FUNNEL2-19-4): `jit_invoke_virtual_mic` asks the FFM direct
    /// door (its panic scope and thread-local table probe) only for an
    /// `invokeExact` site, the one kind the door's table is ever filled for.
    /// **Default ON**; `0` asks it for every call as before
    /// ([`parse::on_unless_zero`]). Read once per MIC-helper call.
    pub ffm_direct_door_exact_only: bool,
    /// `CRATONVM_JIT_BYTECODE_ONLY_KEY` — JIT round 21, lane reflect4
    /// (REFLECT4-21-1, page
    /// `jr21-reflect3-bytecode-only-fallback-bodies-always-interpret-20261014.md`):
    /// a reviewed `Intrinsic` that runs its own JDK body through the
    /// bytecode-only doors (`invoke_virtual_bytecode_only`,
    /// `invoke_special_bytecode_only*`) enters a compiled body of it, compiled
    /// and published under a key no ordinary caller looks up, instead of
    /// interpreting the body on every call
    /// (`vm/src/runtime/interpreter/bytecode_only_body.rs`). **Opt-in**
    /// (`CRATONVM_JIT_BYTECODE_ONLY_KEY=1`, [`parse::non_empty_non_zero`]):
    /// dev's re-land of REFLECT4-21-1 (`CRATONVM_JIT_BYTECODE_ONLY_BODY`,
    /// default ON, the method marked re-entry-only) is the default form, and
    /// the two must not both compile one body. Read once per bytecode-only
    /// door call.
    pub bytecode_only_body: bool,
    /// `CRATONVM_DBG_BYTECODE_ONLY_BODY` — the bytecode-only door's per-call
    /// outcome census, printed at exit (REFLECT4-21-1;
    /// [`parse::non_empty_non_zero`]).
    pub dbg_bytecode_only_body: bool,
    /// `CRATONVM_DBG_MH_LINK_CENSUS` — JIT round 21, lane reflect4
    /// (REFLECT4-21-3): the compiled signature-polymorphic link's per-call
    /// outcome census (`vm/src/jit/helpers.rs` `mh_compiled_link_redirect`),
    /// each refusal named by the step that refused, printed at exit
    /// ([`parse::non_empty_non_zero`]).
    pub dbg_mh_link_census: bool,
}

impl JitFlags {
    fn from_source(src: &dyn FlagSource) -> Self {
        Self {
            disable_jit: parse::non_empty_non_zero(src, "CRATONVM_DISABLE_JIT"),
            // gci i4/fl: by value, together with the VM's root-scan twin
            // (`conservative_roots::shadow_stack_enabled`, `runtime_flag_on`):
            // codegen and the scan must agree on publication.
            shadow_stack: parse::truthy_word(src, "CRATONVM_SHADOW_STACK"),
            method_stats: parse::exactly_one(src, "CRATONVM_DBG_JIT_METHOD_STATS"),
            // gcg triage (tg1): default ON, `=0` restores the helper call.
            inline_card_mark: parse::on_unless_zero(src, "CRATONVM_JIT_INLINE_CARD_MARK"),
            osr_entry_tierup: parse::on_unless_zero(src, "CRATONVM_JIT_OSR_ENTRY_TIERUP"),
            deopt_null_const_as_object: parse::on_unless_zero(
                src,
                "CRATONVM_JIT_IR_DEOPT_NULL_CONST_AS_OBJECT",
            ),

            ffm_direct_door_exact_only: parse::on_unless_zero(
                src,
                "CRATONVM_JIT_FFM_DIRECT_DOOR_EXACT_ONLY",
            ),
            bytecode_only_body: parse::non_empty_non_zero(src, "CRATONVM_JIT_BYTECODE_ONLY_KEY"),
            dbg_bytecode_only_body: parse::non_empty_non_zero(
                src,
                "CRATONVM_DBG_BYTECODE_ONLY_BODY",
            ),
            dbg_mh_link_census: parse::non_empty_non_zero(src, "CRATONVM_DBG_MH_LINK_CENSUS"),
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
    /// `CRATONVM_VERIFY_RECEIVER_ASSIGNABLE` — the split verifier pops an
    /// `invokevirtual` / `getfield` / `putfield` receiver as the referenced
    /// class (HotSpot's `pop_stack(ref_class_type)`), refusing an
    /// unassignable one with "Bad type on operand stack". **Default ON**;
    /// `0` / `false` restore the reference-only check (interpreter round i5
    /// wave 101, lane L7). [`parse::on_unless_zero_or_false`].
    pub verify_receiver_assignable: bool,
    /// `CRATONVM_VERIFY_HANDLER_FRAMES` — the split verifier matches the
    /// type state of every instruction inside a handler's range against the
    /// handler's declared frame (HotSpot's
    /// `verify_exception_handler_targets`). **Default ON**; `0` / `false`
    /// restore the entry-only adoption (interpreter round i5 wave 101, lane
    /// L7). [`parse::on_unless_zero_or_false`].
    pub verify_handler_frames: bool,
    /// `CRATONVM_VERIFY_HANDLER_CATCH_MEMO` — the per-instruction handler
    /// frame check ([`Self::verify_handler_frames`]) asks whether a handler's
    /// catch type is assignable to its declared frame's stack slot once per
    /// handler per method, not once per instruction in its range (the
    /// question has the same two operands at every pc). **Default ON**; `0` /
    /// `false` ask it at every instruction again (JIT round 23 wave 2, the
    /// proxy-define page: `RuntimeException`/`Error` -> `Throwable` over every
    /// pc of every proxy method was ~9 % of the row's main-thread CPU).
    /// [`parse::on_unless_zero_or_false`].
    pub verify_handler_catch_memo: bool,
    /// `CRATONVM_VERIFY_INITIALIZERS_FIRST` — the verifier walks a class's
    /// `<init>` methods, then its `<clinit>`, then the rest (each group in
    /// class-file order), as every HotSpot 25 run does (`Method::sort_methods`
    /// puts the two `vmSymbols` names first). Visible as the order of the
    /// link's verifier `loadClass` calls on a user loader and as which
    /// method's `VerifyError` is reported. **Default ON**; `0` / `false` walk
    /// class-file order (verifier-link-loads page, item 2).
    /// [`parse::on_unless_zero_or_false`].
    pub verify_initializers_first: bool,
    /// `CRATONVM_VERIFY_SUBCLASS_MEMO` — the user-loader Pass 3 walk
    /// (`verifier::UndecidedIsAssignable`) memoises its `is_subclass` answers
    /// for the class being walked (each answer is two loader-chain name
    /// resolutions; the class store does not change during the walk).
    /// **Default ON**; `0` / `false` ask the hierarchy every time (JIT round 23
    /// wave 2, the proxy-define page). [`parse::on_unless_zero_or_false`].
    pub verify_subclass_memo: bool,
    /// `CRATONVM_VERIFY_TYPE_MAP_BUFFER_REUSE` — the split verifier's per-method
    /// type-map builder reuses the previous method's emptied row vectors
    /// within one class walk instead of allocating and freeing four vectors
    /// per method. The maps are byte-identical. **Default ON**; `0` / `false`
    /// allocate fresh ones (JIT round 23 wave 5, the proxy-define page).
    /// [`parse::on_unless_zero_or_false`].
    pub verify_type_map_buffer_reuse: bool,
    /// `CRATONVM_LOADER_LINK_VERDICT_MEMO` — per user-defined loader, the VM
    /// keeps two loader-life-invariant verdicts its class resolutions asked on
    /// every name: "isolated URL loader" and "parent chain reaches the
    /// application loader" (`ClassRealm::loader_link_verdicts`). **Default ON**;
    /// `0` / `false` recompute them per resolution (JIT round 23 wave 6, the
    /// proxy-define page). [`parse::on_unless_zero_or_false`].
    pub loader_link_verdict_memo: bool,
    /// `CRATONVM_LOADER_LOAD_CLASS_OVERRIDE_MEMO` — `native-builtins`'
    /// `receiver_overrides_load_class` (does the receiver's class override
    /// `loadClass`?) is kept per (receiver class id, overload) in the VM's
    /// `LoaderIdentityTables`, stamped with the hierarchy-edge, in-place-origin
    /// and redefinition counters and pruned with dead loaders' classes.
    /// **Default ON**; `0` / `false` walk every time (JIT round 23 wave 6, the
    /// proxy-define page). [`parse::on_unless_zero_or_false`].
    pub load_class_override_memo: bool,
    /// `CRATONVM_CLASS_BY_NAME_NO_ALLOC` — `ClassManager::find_class_by_name_for_loader`
    /// tries the same keys in the same order (slash form, then the dot form
    /// when the slash form has a `/`) without allocating them up front.
    /// **Opt-in** (`1` / `true` / `yes`; neutral on the n23-w7 A/B); otherwise the owned key list (JIT round 23
    /// wave 7, the proxy-define page). [`parse::one_true_yes_exact`].
    pub class_by_name_no_alloc: bool,
    /// `CRATONVM_VERIFY_USER_LOADER_CLASSES` — a class a user-defined loader
    /// defines gets the Pass 3 type-state verdict too (HotSpot's
    /// `BytecodeVerificationRemote`, on by default), with an assignability
    /// question this verifier cannot decide (a class not loaded yet) answered
    /// "assignable". **Default ON**; `0` / `false` restore the deferral that
    /// ran only the structural scan and the linkage rules (interpreter round i5
    /// wave 101, lane L7). [`parse::on_unless_zero_or_false`].
    pub verify_user_loader_classes: bool,
    /// `CRATONVM_VERIFY_USER_LOADER_CLASSES_AT_LINK` — the Pass 3 verdict of
    /// [`Self::verify_user_loader_classes`] (and the two linkage rules the
    /// deferral ran) is given when the class is LINKED, as HotSpot verifies in
    /// `InstanceKlass::link_class_impl`, not inside `defineClass`: a bad class
    /// defined and never linked raises nothing. The structural scan stays at
    /// define. Hidden classes and redefinitions keep the define-time verdict.
    /// **Default ON**; `0` / `false` give the verdict at define again
    /// (interpreter round i5 wave 102, lane L7). [`parse::on_unless_zero_or_false`].
    pub verify_user_loader_classes_at_link: bool,
    /// `CRATONVM_VERIFY_CONTROL_RULES` — the split and inference verifiers
    /// apply three HotSpot rules they skipped: a `*return` value must be
    /// assignable to the declared return type and `return` needs a `void`
    /// method ("Bad return type", "Method does not expect a return value",
    /// "Method expects a return value"); an array load / store needs an array
    /// of its kind ("Bad type on operand stack in iaload", ...); control may
    /// not fall through the end of the code ("Control flow falls through code
    /// end"). **Default ON**; `0` / `false` restore the old acceptance
    /// (interpreter round i5 wave 102, lane L7). [`parse::on_unless_zero_or_false`].
    pub verify_control_rules: bool,
    /// `CRATONVM_VERIFY_SUBROUTINES` — a method with `jsr` / `ret` in a class
    /// file below major 51 is type-checked with its subroutines followed per
    /// calling context (libverify's subroutine handling), not accepted on the
    /// structural scan alone. **Default ON**; `0` / `false` restore the
    /// structural-only acceptance (interpreter round i5 wave 103, lane L7).
    /// [`parse::on_unless_zero_or_false`].
    pub verify_subroutines: bool,
    /// `CRATONVM_VERIFY_THIS_UNINIT_FLAGS` — the split verifier keeps HotSpot's
    /// `flagThisUninit` per frame: a constructor's `return` while it is set
    /// ("Constructor must call super() or this() before return", even when
    /// `this` was overwritten), a branch, fall-through or handler whose
    /// declared frame lacks it ("Current frame's flags are not assignable to
    /// stack map frame's."), and an `invokespecial <init>` on
    /// `uninitializedThis` in a `try` range checked against its handlers with
    /// the incoming locals. **Default ON**; `0` / `false` restore the
    /// locals-only check (interpreter round i5 wave 108, lane L7).
    /// [`parse::on_unless_zero_or_false`].
    pub verify_this_uninit_flags: bool,
    /// `CRATONVM_VERIFY_MAJOR50_FAILOVER` — a major-50 class is verified as
    /// HotSpot's `Verifier::verify` does: the split (StackMapTable) rules
    /// first; on a refusal, libverify's rules with the table ignored; the
    /// class loads if either accepts, and a class both refuse reports the
    /// split verifier's message. **Default ON**; `0` / `false` restore the
    /// single relaxed walk (interpreter round i5 wave 112, lane L7).
    /// [`parse::on_unless_zero_or_false`].
    pub verify_major50_failover: bool,
    /// `CRATONVM_HARDEN_MANIFEST_CLASSPATH` — drop manifest `Class-Path`
    /// entries that escape the JAR's directory. [`parse::non_empty_non_zero`].
    pub harden_manifest_classpath: bool,
    /// `CRATONVM_DISABLE_JAR_MMAP` — read JARs with `read()` instead of `mmap`.
    pub disable_jar_mmap: bool,
    /// `CRATONVM_CLASS_PATH_LIVE_JAR_NO_REREAD` — [`parse::truthy_word`] (default off, opt-in,
    /// performance). Interpreter round i6 wave 115, lane L5 (proposal
    /// `i100-L5-proposal-loader-area-directions` item 1): `ClassPath::add_path` does not read a
    /// jar again that the class path already has an entry for (the re-read entry was dropped as a
    /// duplicate of the same file), so a further `URLClassLoader` over a live jar costs no read
    /// and no central-directory parse under the class-manager write lock.
    pub class_path_live_jar_no_reread: bool,
    /// `CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION` — [`parse::truthy_word`] (default off,
    /// opt-in, HotSpot parity under review). Interpreter round i6 wave 118, lane L5 (proposal
    /// `i45-L5-proposal-one-module-access-question-by-identity` item 2, with `i44-L5`):
    /// `access_control::class_export_denial` and `reflective_module_access` ask the one module
    /// question by identity (`module_access`) first and admit on its yes; their name-map arms
    /// answer the rest and spell every refusal. Since wave 122 (lane L5) the same switch also
    /// gates a REFUSAL: the `Module.isExported(pn, other)` / `isOpen(pn, other)` natives answer a
    /// named name-map receiver asked about a user-`ModuleLayer` target by that target's identity
    /// (`native-builtins` `layer_target_by_identity`), so a descriptor's qualified `exports p to m`
    /// no longer reaches a layer module that only shares the name `m`.
    pub module_access_export_one_question: bool,
    /// `CRATONVM_USER_PASS3_RECORDS_LINK_LOADS` — [`parse::on_unless_zero_or_false`] (HotSpot
    /// parity; **default ON** since JIT round 23 wave 10, R23-W10-FLIP; `=0` restores the
    /// two-walk route). Interpreter round i6 wave 119, lane L5 (proposal
    /// `i102-L5-proposal-one-walk-for-user-loader-pass3-and-type-maps`): a user-loader class whose
    /// link owes both the Pass 3 verdict and the type-map walk gets ONE walk (the verdict's,
    /// recording the verifier loads, catch types included, and publishing the maps) instead of two.
    /// A parity fix: the two-walk route under-records the loads HotSpot's verifier makes through
    /// the defining loader (`tools/probes/jit/r23/VLoad.java` on Temurin 25.0.3: `TckLoading`,
    /// `TckLang`, `RealFjp` match HotSpot exactly on one walk and miss loads on two; page
    /// `docs/feature-designs/class-resolution-and-caching.md`).
    /// As a performance switch it lost the n23-w9c A/B 2 of 3 (`Zs9Nd8ProxyCost room 3000`,
    /// µs/define: one walk 2026/2359/1916 vs `=0` 1645/1692/2223); parity overrides that.
    /// Pinned to HotSpot's lists by `classloading/tests/verifier_loads_one_walk_diff.rs`.
    pub user_pass3_records_link_loads: bool,
    /// `CRATONVM_DEFINE_PD_URL_MEMO` — [`parse::truthy_word_default_true`] (default ON since
    /// the round-i7 triage, `=0` off; performance). Interpreter round i6 wave 120, lane L5 (umbrella
    /// `i71-orch-proposal-class-path-jar-loading-is-eight-times-hotspot`): the `defineClass*`
    /// natives decode a `ProtectionDomain`'s code-source URL once per domain and GC address epoch
    /// (`classloader::PdUrlMemo`), not once per class.
    pub define_pd_url_memo: bool,
    /// `CRATONVM_BUILTIN_FOR_NAME_RECORD` — [`parse::truthy_word_default_true`] (default ON
    /// since the round-i7 user-decision flip, WARNING i7-user-flip-20261022, `=0` off;
    /// performance). Interpreter round i6 wave 133, lane L5d (page
    /// `i133-L2-typevariable-getannotations-is-16-to-19x-hotspot-xint`): `Class.forName(name, init,
    /// loader)` with the application or platform loader answers a name that loader already
    /// answered a `forName` for from the VM's per-loader record (HotSpot's `SystemDictionary`
    /// dictionary hit) instead of running the loader's Java `loadClass` again.
    pub builtin_for_name_record: bool,
    /// `CRATONVM_LOOKUP_DEFINE_JDK_BODY` — [`parse::truthy_word`] (default off, opt-in, HotSpot
    /// parity under review). Interpreter round i6 wave 120, lane L5 (proposal
    /// `i44-L5-proposal-lookup-define-runs-the-jdks-own-checks`): the three
    /// `MethodHandles.Lookup` define natives are not registered, so the JDK's own checks run down
    /// to `ClassLoader.defineClass0`, which links loader-faithfully for a lookup define.
    pub lookup_define_jdk_body: bool,
    /// `CRATONVM_JMOD_LAZY_INDEX` — index a JMOD from its ZIP central
    /// directory alone and read a local header only when its entry is served
    /// (zr z8/fm). **Default ON**; `0` / `false` keep `zip::ZipArchive`, whose
    /// open reads every entry's local header (a page fault per entry of a
    /// mapped JMOD). [`parse::on_unless_zero_or_false`].
    pub jmod_lazy_index: bool,
    /// `CRATONVM_JAR_LAZY_INDEX` — open a plain class-path jar from its ZIP
    /// central directory alone, as `CRATONVM_JMOD_LAZY_INDEX` does for a JMOD
    /// (zt/fp footprint round). **Default ON**; `0` / `false` keep
    /// `zip::ZipArchive`, whose open reads every entry's local header.
    /// [`parse::on_unless_zero_or_false`].
    pub jar_lazy_index: bool,
    /// `CRATONVM_CLASS_CODE_COMPACT` — at define, copy every method's
    /// bytecode, stack map and unknown-attribute bytes into one compact
    /// per-class buffer, so the decoded attributes stop pinning the whole
    /// class file (zr z11/md). **Default ON**; `0` / `false` keep the views
    /// into the class file. [`parse::on_unless_zero_or_false`].
    pub class_code_compact: bool,
    /// `CRATONVM_BOOT_CLASS_BYTES_REFETCH` — do not keep the class file of a
    /// class the boot loader defined from its own class path (the runtime
    /// image) in the class-bytes cache; a reader that needs it re-reads it
    /// and checks it against the define-time fingerprint (zr z11/md).
    /// **Default ON**; `0` / `false` cache it as before.
    /// [`parse::on_unless_zero_or_false`].
    pub boot_class_bytes_refetch: bool,
    /// `CRATONVM_BOOT_IMAGE_CLASS_BYTES` — when the boot class path is a
    /// JDK's `jmods/`, serve a boot class's bytes from the same JDK's runtime
    /// image (`lib/modules`, the file HotSpot runs) whenever the image holds
    /// the class, and serve the classes only the image holds (jlink-generated:
    /// `BoundMethodHandle$Species_*`, `SystemModules$*`) at all (zs s4/rp).
    /// Module descriptors and resources stay the JMODs'. **Default ON**;
    /// `0` / `false` serve the JMODs' bytes only, as before.
    /// [`parse::on_unless_zero_or_false`].
    pub boot_image_class_bytes: bool,
    /// `CRATONVM_JIMAGE_PACKAGE_INDEX` — a `lib/modules` jimage on the class
    /// path is indexed by PACKAGE (about 900 packages, each to a module
    /// number) instead of by class (a `String` -> `String` map of every one of
    /// its ~27 000 classes, ~5 MB); a class is then confirmed by the image's
    /// own perfect-hash lookup (zs s6/rs). An image with a package in two
    /// modules keeps the per-class map. **Default ON**; `0` / `false` keep the
    /// per-class map for every image, as before.
    /// [`parse::on_unless_zero_or_false`].
    pub jimage_package_index: bool,
    /// `CRATONVM_JIMAGE_SKIP_DIRECTORY_RECORDS` — a `lib/modules` jimage on the
    /// class path leaves the image's `/packages/<pkg>/<module>` and `/modules`
    /// directory records out of its resource index and its module list (zt
    /// t3/fp): they are libjimage's package table and `jrt:/` directories, not
    /// resources, and `packages` / `modules` are not modules. **Default ON**;
    /// `0` / `false` index them as before.
    /// [`parse::on_unless_zero_or_false`].
    pub jimage_skip_directory_records: bool,
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
    ///
    /// Read by `vm/src/jit/helpers/typecheck.rs` (wired t1 wave 3, lane Y).
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
            verify_receiver_assignable: on_unless_zero_or_false(
                src,
                "CRATONVM_VERIFY_RECEIVER_ASSIGNABLE",
            ),
            verify_handler_frames: on_unless_zero_or_false(src, "CRATONVM_VERIFY_HANDLER_FRAMES"),
            verify_handler_catch_memo: on_unless_zero_or_false(
                src,
                "CRATONVM_VERIFY_HANDLER_CATCH_MEMO",
            ),
            verify_initializers_first: on_unless_zero_or_false(
                src,
                "CRATONVM_VERIFY_INITIALIZERS_FIRST",
            ),
            verify_subclass_memo: on_unless_zero_or_false(src, "CRATONVM_VERIFY_SUBCLASS_MEMO"),
            verify_type_map_buffer_reuse: on_unless_zero_or_false(
                src,
                "CRATONVM_VERIFY_TYPE_MAP_BUFFER_REUSE",
            ),
            loader_link_verdict_memo: on_unless_zero_or_false(
                src,
                "CRATONVM_LOADER_LINK_VERDICT_MEMO",
            ),
            load_class_override_memo: on_unless_zero_or_false(
                src,
                "CRATONVM_LOADER_LOAD_CLASS_OVERRIDE_MEMO",
            ),
            class_by_name_no_alloc: one_true_yes_exact(
                src,
                "CRATONVM_CLASS_BY_NAME_NO_ALLOC",
            ),
            verify_user_loader_classes: on_unless_zero_or_false(
                src,
                "CRATONVM_VERIFY_USER_LOADER_CLASSES",
            ),
            verify_user_loader_classes_at_link: on_unless_zero_or_false(
                src,
                "CRATONVM_VERIFY_USER_LOADER_CLASSES_AT_LINK",
            ),
            verify_control_rules: on_unless_zero_or_false(src, "CRATONVM_VERIFY_CONTROL_RULES"),
            verify_subroutines: on_unless_zero_or_false(src, "CRATONVM_VERIFY_SUBROUTINES"),
            verify_this_uninit_flags: on_unless_zero_or_false(
                src,
                "CRATONVM_VERIFY_THIS_UNINIT_FLAGS",
            ),
            verify_major50_failover: on_unless_zero_or_false(
                src,
                "CRATONVM_VERIFY_MAJOR50_FAILOVER",
            ),
            harden_manifest_classpath: non_empty_non_zero(
                src,
                "CRATONVM_HARDEN_MANIFEST_CLASSPATH",
            ),
            disable_jar_mmap: present(src, "CRATONVM_DISABLE_JAR_MMAP"),
            class_path_live_jar_no_reread: truthy_word(
                src,
                "CRATONVM_CLASS_PATH_LIVE_JAR_NO_REREAD",
            ),
            module_access_export_one_question: truthy_word(
                src,
                "CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION",
            ),
            // R23-W10-FLIP: default ON as a parity fix (`=0` restores the two-walk route).
            user_pass3_records_link_loads: on_unless_zero_or_false(
                src,
                "CRATONVM_USER_PASS3_RECORDS_LINK_LOADS",
            ),
            define_pd_url_memo: truthy_word_default_true(src, "CRATONVM_DEFINE_PD_URL_MEMO"),
            builtin_for_name_record: truthy_word_default_true(src, "CRATONVM_BUILTIN_FOR_NAME_RECORD"),
            lookup_define_jdk_body: truthy_word(src, "CRATONVM_LOOKUP_DEFINE_JDK_BODY"),
            jmod_lazy_index: on_unless_zero_or_false(src, "CRATONVM_JMOD_LAZY_INDEX"),
            jar_lazy_index: on_unless_zero_or_false(src, "CRATONVM_JAR_LAZY_INDEX"),
            class_code_compact: on_unless_zero_or_false(src, "CRATONVM_CLASS_CODE_COMPACT"),
            boot_class_bytes_refetch: on_unless_zero_or_false(
                src,
                "CRATONVM_BOOT_CLASS_BYTES_REFETCH",
            ),
            boot_image_class_bytes: on_unless_zero_or_false(src, "CRATONVM_BOOT_IMAGE_CLASS_BYTES"),
            jimage_package_index: on_unless_zero_or_false(src, "CRATONVM_JIMAGE_PACKAGE_INDEX"),
            jimage_skip_directory_records: on_unless_zero_or_false(
                src,
                "CRATONVM_JIMAGE_SKIP_DIRECTORY_RECORDS",
            ),
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
    /// t1 (2026-10-26): no reader (`real_net_sockets` is the live mirror); see `docs/internal/fixed-bugs/t1-Y-unread-flag-fields-FIXED-20261028.md`.
    pub synthetic_net_sockets_forced: bool,
    /// `CRATONVM_SYNTHETIC_FILEWRITER=1` — opt back into the synthetic (known
    /// lossy) `FileWriter`. Consumers want `!synthetic_filewriter_forced`.
    /// [`parse::exactly_one`].
    pub synthetic_filewriter_forced: bool,
    /// `CRATONVM_JIMAGE_DIRECT_MAP` — `NativeImageBuffer.getNativeMap`
    /// answers a direct `ByteBuffer` over a read-only mapping of the run-time
    /// image, as HotSpot answers its libjimage mapping (zr z8/fm). **Default
    /// ON**; `0` / `false` restore the copy of the whole image into a Java
    /// `byte[]` (~145 MiB of live heap per VM). [`parse::on_unless_zero_or_false`].
    pub jimage_direct_map: bool,
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
            jimage_direct_map: on_unless_zero_or_false(src, "CRATONVM_JIMAGE_DIRECT_MAP"),
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

    /// `CRATONVM_DBG_MH_FINDER_CENSUS`: the `Lookup` finder phase census
    /// (`native-builtins::mh_finder_census`), dumped at exit (interpreter
    /// round i4 wave 89, lane L4).
    pub dbg_mh_finder_census: bool,

    /// `CRATONVM_DBG_ZIP_STREAM_CENSUS`: every `Inflater.inflate*` native call
    /// tallied by stream kind (`ZipFile` entry, GZIP, other) and by the first
    /// frame outside the zip / io layers, with `Resource.getBytes` split out
    /// (`native-builtins::zip_stream_census`), dumped at exit (interpreter round
    /// i6 wave 116, lane L5; proposal
    /// `i64-L5-proposal-serve-zipfile-entry-streams-from-the-vm-archive`).
    pub dbg_zip_stream_census: bool,

    /// `CRATONVM_DBG_MH_FIELD_LINK`: one line per field-handle link made
    /// under `--compatible` (interpreter round i2 wave 61, lane L4; the
    /// positive control of `CRATONVM_MH_FIELD_LINK_COMPATIBLE`).
    pub dbg_mh_field_link: bool,

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

    /// `CRATONVM_DBG_STAMPED` — [`parse::present`]. Interpreter round i7 wave
    /// 137 (lane L1b): read here instead of a `runtime_var_os` on every
    /// `StampedLock` native call.
    pub dbg_stamped: bool,

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

    /// `CRATONVM_REFLECT_NEW_ARRAY_ONE_RESOLVE` — `Array.newArray` (the
    /// native behind `Array.newInstance` / `Arrays.copyOf(.., Class)`) maps
    /// the component mirror to its `ClassId` once and reuses it for the name
    /// and the allocation, and skips the `.`-to-`/` copy of a name that has
    /// no `.`. **Default ON**; `0` / `false` restore the two lookups (JIT
    /// round 23 wave 2, the proxy-define page: ~4 % of the class-file API's
    /// CPU). [`parse::on_unless_zero_or_false`].
    pub reflect_new_array_one_resolve: bool,

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

    /// `CRATONVM_STANDIN_SPUN_IWA_FRAMES` — [`parse::truthy_word`] (default off, opt-in; HotSpot
    /// frames, runs Java inside a `SHOW_HIDDEN_FRAMES` walk). Interpreter round i6 wave 115, lane
    /// L3 (page `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`): a walk across a served
    /// `MethodHandle.invokeWithArguments` of a direct handle asks the JDK for the spread invoker's
    /// and the `asType` adapter's spun `LambdaForm$MH` classes and lists their frames
    /// (`lang_stackwalker::add_spun_invoke_with_arguments_frames`), without running the JDK body
    /// for the call. Needs `CRATONVM_STANDIN_HIDDEN_INVOKE_FRAMES` (default on). Default ON since the
    /// interpreter round i7 triage (WARNING: i7-default-flip), `=0` off.
    pub standin_spun_iwa_frames: bool,

    /// `CRATONVM_STANDIN_SPUN_LINKER_FRAMES` — [`parse::truthy_word`] (default ON since the
    /// interpreter round i7 triage (WARNING: i7-default-flip), `=0` off;
    /// HotSpot frames, runs Java inside a `SHOW_HIDDEN_FRAMES` walk). Interpreter round i6 wave
    /// 121, lane L3 (page `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`, residual 5): a
    /// walk across a served `invokeExact` whose shape `Invokers$Holder` does not pregenerate asks
    /// the JDK for the spun linker (`Invokers.invokeHandleForm(.., LF_EX_LINKER)`) and lists its
    /// `invokeExact_MT` frame (`lang_stackwalker::add_spun_linker_frames`). Needs
    /// `CRATONVM_STANDIN_HIDDEN_INVOKE_FRAMES` (default on).
    pub standin_spun_linker_frames: bool,

    /// `CRATONVM_REFLECT_LAMBDA_PROXY_FACTS` — [`parse::truthy_word`] (default ON since the i6 wave-126 flip, `=0` off;
    /// one rule for HotSpot's facts). Interpreter round i6 wave 118, lane L3 (proposal
    /// `i55-L1-proposal-lambda-proxies-answer-the-tool-interfaces-from-one-record`): core
    /// reflection's `getDeclaredMethods` and `getInterfaces` of a lambda proxy class answer from
    /// the VM's one record (`NativeContext::lambda_proxy_record`), as JVMTI and JNI do, instead
    /// of re-deriving the spun class's members.
    pub reflect_lambda_proxy_facts: bool,

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

    /// `CRATONVM_ES_SHUTDOWN_REAL_POOL` — [`parse::truthy_word_default_true`]
    /// (default ON, `=0` the kill switch; the same truth table as the
    /// `runtime_flag_default_on` read it replaces). Interpreter round i7 wave
    /// 137 (lane L1b): read here instead of on every `ExecutorService`
    /// `shutdown` / `isShutdown` / `isTerminated` / `awaitTermination` call.
    pub es_shutdown_real_pool: bool,

    /// `CRATONVM_FORNAME_TRACE`
    pub forname_trace: bool,

    /// `CRATONVM_GENERIC_INTERFACES_MEMO` — [`parse::on_unless_zero`].
    ///
    /// JIT round 16 (lane coldpath): `Class.getGenericInterfaces()` builds a
    /// class's Signature-attribute `Type[]` once and answers a copy of it, as
    /// HotSpot's per-class `genericInfo` does; `0` rebuilds it on every call.
    pub generic_interfaces_memo: bool,

    /// `CRATONVM_GENERIC_REFLECT_MEMO` — [`parse::on_unless_zero`].
    ///
    /// JIT round 16 (lane coldpath, CP16-1): `Class.getGenericSuperclass()`,
    /// `Method.getGenericReturnType()`, `Method`/`Constructor.
    /// getGenericParameterTypes()` and `Field.getGenericType()` build their
    /// Signature answer once per member and hand back the same `Type`s (a copy
    /// of an array), as HotSpot's cached repositories do; `0` rebuilds per call.
    pub generic_reflect_memo: bool,

    /// `CRATONVM_GENERIC_TYPE_RENDER_PARITY` — [`parse::on_unless_zero`].
    ///
    /// JIT round 16 (lane coldpath): the native `toString`/`getTypeName` of a
    /// parameterized type renders its owner (`Outer<E>$Inner`), and of a
    /// wildcard reads its bounds through the reifying getters and joins all of
    /// them, as HotSpot's `*Impl.toString` do; `0` restores the old renderer.
    pub generic_type_render_parity: bool,

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

    /// `CRATONVM_MH_FOREIGN_HANDLE_RAISES` — [`parse::on_unless_zero`].
    /// A method handle the invoke doors cannot run (none of the synthetic
    /// model's slots, not a delegating handle they follow, not a downcall)
    /// raises an `InternalError` naming its class instead of answering the
    /// call's zero value (interpreter round i1 wave 52).
    pub mh_foreign_handle_raises: bool,

    /// `CRATONVM_MH_FOREIGN_HANDLE_RUNS_VMENTRY` — [`parse::on_unless_zero`].
    /// Before raising for such a handle, the invoke doors run a real JDK
    /// `MethodHandle`'s `form.vmentry` (its `LambdaForm`'s linked static
    /// method) with the handle first, as `invokeBasic` does, when the
    /// arguments already have the handle's types (interpreter round i2
    /// wave 53, lane L4; step 2 of
    /// `interpreter-L4-a-real-jdk-method-handle-answers-a-default-value-FIXED-20261020`).
    pub mh_foreign_handle_runs_vmentry: bool,

    /// `CRATONVM_MH_FOREIGN_DMH_BY_MEMBER` — [`parse::on_unless_zero`].
    /// A real JDK `DirectMethodHandle` whose form is the plain
    /// `invokeStatic` / `invokeVirtual` linker is dispatched by its `member`
    /// (`linkToStatic` / `linkToVirtual` with the member as appendix), the
    /// call that form's `vmentry` makes, without running the linker's
    /// bytecode (interpreter round i2 wave 54, lane L4;
    /// `interpreter-L4-a-real-jdk-method-handle-answers-a-default-value-FIXED-20261020`).
    pub mh_foreign_dmh_by_member: bool,

    /// `CRATONVM_MH_FOREIGN_ADAPTERS_RUN_JDK` — [`parse::on_unless_zero`].
    /// `asType` / `bindTo` of a real JDK `MethodHandle` (none of the
    /// synthetic model's slots) run the JDK's own bodies, a new handle, where
    /// the model's `asType` stamped the new type on the receiver in place and
    /// its `bindTo` read absent slots (interpreter round i2 wave 54, lane L4).
    pub mh_foreign_adapters_run_jdk: bool,

    /// `CRATONVM_MH_FOREIGN_VARARGS_RUN_JDK` — [`parse::on_unless_zero`].
    /// `asVarargsCollector` / `withVarargs` of a real JDK `MethodHandle` run
    /// the JDK's own bodies (a real `MethodHandleImpl$AsVarargsCollector`),
    /// and the invoke doors run a real `AsVarargsCollector` at a collecting
    /// arity through the JDK's own `asType(callSiteType)` (interpreter round
    /// i2 wave 55, lane L4; proposal
    /// `i54-L4-proposal-every-adapter-native-hands-a-real-handle-to-the-jdk`).
    pub mh_foreign_varargs_run_jdk: bool,

    /// `CRATONVM_MH_FOREIGN_COLLECTORS_RUN_JDK` — [`parse::exactly_one`].
    /// `asCollector` / `asSpreader` (both arities) of a real JDK
    /// `MethodHandle` run the JDK's own bodies. Default OFF and kept off:
    /// their answer is a `BoundMethodHandle` over a spun `LambdaForm`, which
    /// the default world cannot run (measured wave 55, lane L4).
    pub mh_foreign_collectors_run_jdk: bool,

    /// `CRATONVM_MH_FOREIGN_VMENTRY_FIRST` — [`parse::on_unless_zero`].
    /// Outside `CRATONVM_PURE_BYTECODE`, a real JDK handle whose form's
    /// `vmentry` is real code runs it (`mh_run_foreign_vmentry`) instead of
    /// the doors' `LambdaForm.interpretWithArguments`, which needs a
    /// `NamedFunction` invoker the held `InvokerBytecodeGenerator` natives
    /// cannot make (`InternalError: ()void : DMH.invokeStatic=...`);
    /// and a real `DirectMethodHandle` whose form's entry those natives
    /// fabricated is dispatched by its `member` (interpreter round i2 wave
    /// 55, lane L4; `interpreter-L4-a-real-jdk-method-handle-answers-a-default-value-FIXED-20261020`).
    pub mh_foreign_vmentry_first: bool,

    /// `CRATONVM_IBG_SPIN_ON_MISS` — [`parse::on_unless_zero`].
    /// The held `InvokerBytecodeGenerator.generateCustomizedCode` native, on
    /// a `lookupPregenerated` miss, runs the JDK's own generator for the form
    /// (a spun `LambdaForm$MH` class, as HotSpot does) instead of fabricating
    /// `LambdaForm.MH`; `generateNamedFunctionInvoker` runs the JDK body
    /// instead of fabricating `LambdaForm.NFI()void`. A generator failure
    /// keeps the fabrication. Interpreter round i2 wave 56, lane L4 (proposal
    /// `interpreter-L4-proposal-spin-the-lambdaforms-real-handles-need-FIXED-20261020`).
    pub ibg_spin_on_miss: bool,

    /// `CRATONVM_MH_ASTYPE_REFUSAL_SITE_TEXT` — [`parse::on_unless_zero`].
    /// The `invoke` / `invokeWithArguments` doors refuse a call of another
    /// arity to a `constant` / `identity` / array-accessor / `throwException`
    /// handle as `asType` does, and a leaf arm's arity refusal is reworded by
    /// the door to name the call site (`cannot convert MethodHandle(..) to
    /// (..)`) (interpreter round i2 wave 54, lane L4; proposal
    /// `interpreter-L4-proposal-one-astype-refusal-record-for-every-invoke-door-FIXED-20261018`).
    pub mh_astype_refusal_site_text: bool,

    /// `CRATONVM_VH_PRIMITIVE_RETURN_ASTYPE` — [`parse::on_unless_zero`].
    /// A `VarHandle` access mode that returns its variable, at a call site
    /// whose return is a primitive the variable's type does not convert to
    /// (`(int) vh.get(o)` over a `long` or a `String` field), is `asType`'s
    /// `WrongMethodTypeException` before the access, as on HotSpot, where it
    /// answered `0` (interpreter round i2 wave 57, lane L4;
    /// `interpreter-L4-a-varhandle-read-into-a-narrower-primitive-site-answers-zero-FIXED-20261021`).
    pub vh_primitive_return_astype: bool,

    /// `CRATONVM_FFM_SEGMENT_VH_LEAN_HEAP` — [`parse::on_unless_zero`].
    /// The lean `SegmentVarHandle` `get` / `set` road (`segment_vh_lean`,
    /// native segments only before) also serves a real HEAP segment
    /// (`MemorySegment.ofArray`) through its backing array, after the JDK's
    /// read-only, bounds, alignment and session checks all pass (any failure
    /// declines to the full road, which raises as before). JIT round 23 wave
    /// 2c, lane L8 (`r23-w2-memorysegment-heap-accessors-10000x-20261021`).
    pub ffm_segment_vh_lean_heap: bool,

    /// `CRATONVM_FFM_SEGMENT_VH_JDK_RAISE` — [`parse::on_unless_zero`].
    /// Correctness: a real `SegmentVarHandle` `get` / `set` whose native road
    /// refuses (alignment, bounds, read-only, closed session) is re-run through
    /// the JDK's own `VarHandleSegmentAs<X>` static body, so the exception
    /// carries HotSpot's frames (JIT round 23 wave 2g, lane L8;
    /// `r23-w2-memorysegment-accessor-exceptions-lose-jdk-frames-20261021`).
    pub ffm_segment_vh_jdk_raise: bool,

    /// `CRATONVM_FFM_SEGMENT_VH_JDK_RAISE_ALL_MODES` — [`parse::on_unless_zero`].
    /// Correctness: [`Self::ffm_segment_vh_jdk_raise`] for the ordered and
    /// read-modify-write access modes too (`getVolatile`, `setRelease`,
    /// `compareAndSet`, `getAndAdd`, ...), through the aligned form class's
    /// static body (`VarHandleSegmentAs<X>Aligned.<mode>`), so their exceptions
    /// carry HotSpot's frames (JIT r23 wave 6, lane L8;
    /// `r23-w2-memorysegment-accessor-exceptions-lose-jdk-frames-20261021`).
    pub ffm_segment_vh_jdk_raise_all_modes: bool,

    /// `CRATONVM_FFM_REAL_MEMO_FIRST` — [`parse::on_unless_zero`]. Perf: the
    /// compiled FFM element door (`jit_ffm_segment_get/set`) asks its per-thread
    /// real-segment memo BEFORE the carrier verdict table, so a hot real
    /// segment skips the carrier probe (9% of a heap `getAtIndex` on n23-w4).
    /// An address the memo holds for the current epoch is a JDK segment, never
    /// a carrier, so the order changes no answer (JIT r23 wave 5, lane L8).
    pub ffm_real_memo_first: bool,

    /// `CRATONVM_FFM_ARENA_JIT_RESOLVE` — [`parse::on_unless_zero`]. Perf: the
    /// compiled FFM element door serves an `Unsafe` arena-handle segment address
    /// through the lock-free JIT arena table (the optimizing tier's scoped-fold
    /// resolve: key, length, pointer, key again) instead of the locked
    /// `ArenaStore::copy_out` / `copy_in` tree walk (31% of a native
    /// `getAtIndex` on n23-w4); a miss keeps the locked copy (JIT r23 wave 5,
    /// lane L8).
    pub ffm_arena_jit_resolve: bool,

    /// `CRATONVM_MH_INVOKE_RETURN_UNBOX` — [`parse::on_unless_zero`].
    /// `MethodHandle.invoke` of a handle whose declared return is a reference,
    /// at a call site whose return is a primitive, and `invoke` /
    /// `invokeExact` of an `asType` copy of a direct handle from a reference
    /// leaf return to a primitive one, unbox the result as `asType` does at
    /// run time (`ValueConversions.unbox*`): `null` is a
    /// `NullPointerException`, a wrapper that does not widen (or a `Number`
    /// that is no wrapper) `Class.cast`'s `ClassCastException`, anything else
    /// the `(Number)` cast's, where the return coercion answered `0`
    /// (interpreter round i2 wave 58, lane L4;
    /// `interpreter-L4-a-method-handle-invoke-unboxes-a-null-or-foreign-box-to-zero-FIXED-20261022`).
    pub mh_invoke_return_unbox: bool,

    /// `CRATONVM_MH_GUARD_TYPE_IS_TARGETS` — [`parse::on_unless_zero`].
    /// `MethodHandles.guardWithTest`'s adapter answers its TARGET's own
    /// `MethodType` as its `type` when the target's return was retyped by an
    /// `asType` (its `MH_DESC` keeps the leaf's return for the boxing), as
    /// on HotSpot; it answered the leaf's return, so a
    /// `MutableCallSite.setTarget` of such a guard refused with
    /// `WrongMethodTypeException` (interpreter round i2 wave 58, lane L4;
    /// `L4W58MixedWidthArguments`).
    pub mh_guard_type_is_targets: bool,

    /// `CRATONVM_MH_ADAPTER_STAMP_CONVERTS` — [`parse::on_unless_zero`].
    /// ONE `asType` / `explicitCastArguments` stamp over an ADAPTER handle
    /// (`filterArguments`, `dropArguments`, `insertArguments`, ...) is
    /// judged like a cast chain: the arguments are converted from the stamp's
    /// type into the adapter's own before its arm runs, and the result back
    /// out, where they reached the adapter's targets as passed (Groovy 5's
    /// `long a = 5; a += 1` answered `5`; interpreter round i2 wave 58, lane
    /// L4; `L4W58MixedWidthArguments`).
    pub mh_adapter_stamp_converts: bool,

    /// `CRATONVM_MH_RETURN_PLAN` — [`parse::on_unless_zero`] (`=0` re-derives per call).
    /// A method handle's declared descriptor and parsed cast chain, which the
    /// doors and `mh_dispatch` re-derived on every call (the chain text read
    /// and parsed per dispatch on Groovy's hot path since wave 58), are
    /// memoised per handle within a collection epoch (interpreter round i2
    /// wave 59, lane L4; proposal
    /// `i58-L4-proposal-one-return-plan-for-every-method-handle-door`).
    pub mh_return_plan: bool,

    /// `CRATONVM_MH_VOID_DECLARED_DISCARDS` — [`parse::on_unless_zero`].
    /// A method handle whose declared `type` returns `void` over a leaf that
    /// returns a value (`MethodHandles.dropReturn`, `asType((..)void)`)
    /// discards the value, as `asType` does: `(Object) dropReturn(h).invoke(x)`
    /// is `null`, where the door boxed the leaf's value (interpreter round i2
    /// wave 59, lane L4; `L4W59CombinatorCallSiteTypes` row `dropReturn`).
    pub mh_void_declared_discards: bool,

    /// `CRATONVM_MH_DYNAMIC_INVOKER_SITE_TYPE` — [`parse::on_unless_zero`].
    /// `CallSite.dynamicInvoker()`'s handle is typed by the call site's
    /// target's `type()`, not its leaf's `MH_DESC` (an `asType` copy of an
    /// adapter answered the leaf's `(int,int)int`, so the invoker's `asType`
    /// raised; interpreter round i2 wave 59, lane L4,
    /// `L4W59CombinatorCallSiteTypes`).
    pub mh_dynamic_invoker_site_type: bool,

    /// `CRATONVM_MH_COMPATIBLE_RECORD_OWNER` — [`parse::on_unless_zero`] (`=0` resolves by name).
    /// Under `--compatible` a static method / field handle and a static
    /// `VarHandle` of a class a BUILT-IN loader defined record the owner's
    /// `ClassId` at mint, as `--jdk-only` always does, instead of resolving
    /// the class by name on every call (loader-blind; a class-manager read
    /// per access). Interpreter round i2 wave 60, lane L4; proposal
    /// `interpreter-L4-proposal-compatible-handles-record-their-owner-FIXED-20261024`.
    pub mh_compatible_record_owner: bool,

    /// `CRATONVM_MH_LAMBDA_PROXY_MEMBERS` — [`parse::on_unless_zero`] (a
    /// correctness fix, both modes). A `Lookup` finder (`findVirtual`,
    /// `findSpecial`, `findStatic`, `unreflect`, `unreflectSpecial`) over a
    /// member of one of this VM's lambda proxy classes resolves the member
    /// among the proxy's own methods and answers a handle typed
    /// `(Proxy, ..)R`, a `linkTo*` over a proxy's `MemberName` runs the
    /// receiver's member, and a serializable proxy's `writeReplace()` answers
    /// its `SerializedLambda` on every virtual call. `=0` restores the
    /// receiver-less `(..)R` handle (interpreter round i2 wave 61, lane L4;
    /// `L4W61LambdaProxyMemberHandles`). Since wave 62 (lane L4) it also has
    /// `Reflection.getClassAccessFlags` answer a proxy's `0x1030` instead of
    /// the `PUBLIC` fallback (`L4W61LambdaProxyHandleGaps` row `public-lookup`).
    pub mh_lambda_proxy_members: bool,

    /// `CRATONVM_MH_FIELD_LINK_COMPATIBLE` — [`parse::on_unless_zero`] (`=0` links under `--jdk-only` only).
    /// The field-accessor link (`CRATONVM_MH_FIELD_LINK`) is made under
    /// `--compatible` too, not only `--jdk-only` (interpreter round i2 wave
    /// 61, lane L4; proposal `interpreter-L4-proposal-link-field-handles-under-compatible-too-FIXED-20261025`).
    pub mh_field_link_compatible: bool,

    /// `CRATONVM_MH_INSTANCE_OWNER_ID` — [`parse::on_unless_zero`] (default on since the i63 take-over triage: `L4W62LambdaProxyOwnerCast` matches HotSpot in both modes, no other L4 probe moves).
    /// An unbound instance method handle (`findVirtual`, `unreflect`, and a
    /// lambda proxy's method and field handles) records its member's owner
    /// `ClassId` at mint, and the door's receiver cast and `bindTo`'s cast
    /// answer from it: an exact or subclass receiver is admitted by id, and a
    /// lambda proxy's handle (minted over the class `""`, which cast nothing)
    /// refuses a receiver of another class with HotSpot's
    /// `ClassCastException`. Interpreter round i2 wave 62, lane L4; proposal
    /// `i61-L4-proposal-method-handles-name-their-owner-by-class-id`.
    pub mh_instance_owner_id: bool,

    /// `CRATONVM_MH_OWNER_ID_AUTHORITY` — [`parse::on_unless_zero`] (default on since the wave-63 host run: `L4W63ForeignOwnerCast --compatible` matches HotSpot, the W55-W63 battery does not move, `L4W63OwnerIdBench` within noise).
    /// Stage 2 of `i61-L4-proposal-method-handles-name-their-owner-by-class-id`
    /// (interpreter round i2 wave 63, lane L4), on top of
    /// `CRATONVM_MH_INSTANCE_OWNER_ID`: an unbound instance FIELD handle
    /// (`findGetter`, `findSetter`, `unreflectGetter`, `unreflectSetter`)
    /// records its owner's `ClassId` too, the door's instance field arms and
    /// the field-accessor link resolve the slot by that id instead of
    /// re-resolving `MH_CLASS` by name, and the recorded owner refuses a
    /// receiver of ANOTHER LOADER's class of the same name (or a subclass of
    /// one) with HotSpot's `ClassCastException` -- the leaf cast, the direct
    /// lane and `bindTo`, where the by-name cast admitted it.
    pub mh_owner_id_authority: bool,

    /// `CRATONVM_MH_FIELD_SLOT_FROM_MINT` — [`parse::truthy_word`] (opt-in).
    /// Interpreter round i2 wave 64, lane L4 (proposal
    /// `i63-L4-proposal-field-handles-carry-their-slot-from-mint`), on top of
    /// `CRATONVM_MH_OWNER_ID_AUTHORITY`: `findGetter`, `findSetter`,
    /// `unreflectGetter` and `unreflectSetter` of an instance field resolve the
    /// field's slot once, in the recorded owner, and keep it on the handle; the
    /// door's instance `GETTER` / `SETTER` arms and the field-accessor link
    /// read it instead of resolving the slot per call. A pure performance
    /// change: the slot is the one the per-call lookup answered.
    pub mh_field_slot_from_mint: bool,

    /// `CRATONVM_MH_SPECIAL_BY_OWNER_ID` — [`parse::on_unless_zero`] (default on since the wave-64 host run: `L4W64SpecialByOwnerId --compatible` matches HotSpot; the W55-W64 battery does not regress).
    /// Interpreter round i2 wave 64, lane L4 (first stage of proposal
    /// `i63-L4-proposal-virtual-door-dispatches-by-owner-id`): `findSpecial`
    /// and `unreflectSpecial` record their dispatch class's `ClassId`, and the
    /// door's `SPECIAL` arm calls `invoke_special_by_class_id` with it instead
    /// of re-resolving `MH_CLASS` by name per call -- which also stops a
    /// `super.m()` handle of one loader's class running another loader's
    /// same-named class's body.
    pub mh_special_by_owner_id: bool,

    /// `CRATONVM_MH_CTOR_REGISTERS_FINALIZABLE` — [`parse::on_unless_zero`]
    /// (correctness, default on; `=0` reproduces the defect). JIT round 21
    /// wave 8, lane reflect (page
    /// `docs/feature-designs/jdk-only-mode.md`):
    /// `linkToSpecial` of a `<init>` member -- the constructor call of a
    /// `newInvokeSpecial` form (every constructor `MethodHandle`, every JDK 25
    /// `Constructor.newInstance` through `DirectConstructorHandleAccessor`) --
    /// registers a finalizable receiver, as HotSpot's `Object.<init>` return
    /// does (`RegisterFinalizersAtInit`). The receiver was allocated by
    /// `Unsafe.allocateInstance`, which (rightly) does not register, so no
    /// reflectively constructed instance was ever finalized.
    pub mh_ctor_registers_finalizable: bool,

    /// `CRATONVM_MH_LINKER_HIT_SKIPS_ROOT_REFRESH` — [`parse::on_unless_zero`]
    /// (perf, default on since its landing; `=0` restores the refresh).
    /// JIT round 20 wave 17, lane pRefl (`R20PAllocEntryCheck`): the
    /// signature-polymorphic linker door (`lang_invoke::mh_linked_invoker`,
    /// every `invokeExact` / `invoke` of a real JDK handle, among them each
    /// `DirectMethodHandleAccessor.invokeImpl` call `Method.invoke` makes)
    /// skips its full root-snapshot deposit before the linker call when the
    /// site's linker pair came from the VM's table: nothing the door pinned is
    /// read after that call.
    pub mh_linker_hit_skips_root_refresh: bool,

    /// `CRATONVM_JIT_MH_COMPILED_LINK` — [`parse::on_unless_zero`] (perf,
    /// default on since its landing; `=0` keeps every compiled
    /// signature-polymorphic site on its native door). JIT round 20 wave 19,
    /// lane pRefl: a compiled `invokeExact` site whose linker pair is in the
    /// VM's table, a compiled `invokeBasic` on a real handle and a compiled
    /// `linkToStatic` / `linkToVirtual` / `linkToInterface` call their Java
    /// target as an ordinary compiled call (`vm/src/jit/helpers.rs`
    /// `mh_compiled_link_redirect`).
    pub jit_mh_compiled_link: bool,

    /// `CRATONVM_JIT_MH_COMPILED_LINK_SPECIAL` — [`parse::truthy_word_default_true`] (default ON since the
    /// w109 A/B on Windows: private-findspecial 0.05x, private-unreflect 0.16x, super-findspecial/ctor ~0.7x; `=0` off; was default off, opt-in, performance).
    /// Interpreter round i5 wave 109, lane L2: with `CRATONVM_JIT_MH_COMPILED_LINK`, a compiled
    /// `linkToSpecial` calls its `MemberName`'s method as a compiled `invokespecial`, and a constant
    /// `invokeExact` site binds a real `$Special` handle (or a plain one over a private method) to that
    /// call (`lang_invoke::mh_compiled_bind_special_target`).
    pub jit_mh_compiled_link_special: bool,

    /// `CRATONVM_JIT_MH_COMPILED_LINK_BY_ID` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// Interpreter round i6 wave 116, lane L2 (page
    /// `i115-L2-proposal-compiled-invoke-basic-linked-by-class-id`): with `CRATONVM_JIT_MH_COMPILED_LINK`,
    /// a compiled `invokeBasic` whose `form.vmentry` lives in a HIDDEN (spun) `LambdaForm` class, or in
    /// a class whose name is not unique, calls that entry's already-published compiled body, found by
    /// class id, instead of the native door and its `interpreter::execute` re-entry. First slice: only a
    /// body with no deopt points whose method has no exception table.
    pub jit_mh_compiled_link_by_id: bool,

    /// `CRATONVM_JIT_MH_COMPILED_LINK_LINKER_BY_ID` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// Interpreter round i6 wave 122, lane L2 (page `i100-L4-proposal-method-handle-warm-call-directions`,
    /// item 2): with `CRATONVM_JIT_MH_COMPILED_LINK`, a compiled `invokeExact` site whose linker pair the
    /// VM holds but whose linker `MemberName` names a HIDDEN (spun) `LambdaForm` class calls that linker's
    /// already-published compiled body, found by class id, instead of the native door. The
    /// `CRATONVM_JIT_MH_COMPILED_LINK_BY_ID` admission, applied to the linker shape.
    pub jit_mh_compiled_link_linker_by_id: bool,

    /// `CRATONVM_JIT_MH_COMPILED_LINK_USER_SITE` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// Interpreter round i5 wave 103, lane L4 (page
    /// `i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call`): with
    /// `CRATONVM_JIT_MH_COMPILED_LINK`, a compiled `invokeExact` site whose descriptor names an
    /// application class (no `linker:` pair row) is bound to the linker from the door's
    /// basic-member row and the caller's site-type row (`lang_invoke::mh_compiled_linker_user_site`),
    /// so it calls the linker form directly instead of the door on every call.
    pub jit_mh_compiled_link_user_site: bool,

    /// `CRATONVM_JIT_REFLECT_ACCESSOR_LINK` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// JIT round 21 wave 1, lane reflect: the compiled `invokeExact` site of
    /// `DirectMethodHandleAccessor.invokeImpl` calls the reflected method as an ordinary compiled
    /// call, unboxing the arguments and boxing the result in the helper, instead of the linker
    /// chain and its spun adapter form (`vm/src/jit/helpers.rs` `reflect_accessor_link_redirect`).
    pub jit_reflect_accessor_link: bool,

    /// `CRATONVM_REFLECT_SIG_POLY_UOE` — [`parse::on_unless_zero`] (a correctness fix toward HotSpot;
    /// `=0` is the kill switch). JIT round 21 wave 1, lane reflect: a reflective call of
    /// `MethodHandle.invoke` / `invokeExact` or of a `VarHandle` access-mode method (the
    /// signature-polymorphic natives JDK 25 serves through `NativeAccessor.invoke0`) raises HotSpot's
    /// `UnsupportedOperationException` ("... cannot be invoked reflectively") wrapped in
    /// `InvocationTargetException`, after the receiver / arity / argument checks
    /// (`lang_class::native_method_invoke_body`), instead of dispatching the handle.
    pub reflect_sig_poly_uoe: bool,

    /// `CRATONVM_REFLECT_INVOKE0_VM_TEXTS` — [`parse::on_unless_zero`] (a correctness fix toward
    /// HotSpot; `=0` is the kill switch). JIT round 21 wave 1, lane reflect: `NativeAccessor.invoke0`
    /// raises `JVM_InvokeMethod`'s own check texts -- a message-less NPE for a null receiver,
    /// "object is not an instance of declaring class", "wrong number of arguments" -- instead of the
    /// Java accessor's (`lang_class::native_method_invoke_body`, `gate_access == false` only).
    pub reflect_invoke0_vm_texts: bool,

    /// `CRATONVM_REFLECT_DOOR_NATIVE_ACCESSOR_TEXTS` — [`parse::on_unless_zero`] (a correctness fix
    /// toward HotSpot; `=0` is the kill switch). JIT round 21 wave 1, lane reflect (review item 3): the
    /// `Method.invoke` door gives `JVM_InvokeMethod`'s texts (as `CRATONVM_REFLECT_INVOKE0_VM_TEXTS`)
    /// for the methods JDK 25's `MethodHandleAccessorFactory.useNativeAccessor` sends to `NativeAccessor`
    /// (`lang_class::method_takes_native_accessor`).
    pub reflect_door_native_accessor_texts: bool,

    /// `CRATONVM_REFLECT_NATIVE_ACCESSOR_ARG_TEXTS` — [`parse::on_unless_zero`] (a correctness fix
    /// toward HotSpot; `=0` is the kill switch). JIT round 21 wave 8, lane reflect2:
    /// `NativeAccessor.invoke0` / `newInstance0` raise `Reflection::invoke`'s argument refusals -- a
    /// message-less `IllegalArgumentException` for a null to a primitive parameter, a cause-less
    /// "argument type mismatch" -- and `newInstance0` its "wrong number of arguments", instead of the
    /// Java accessors' texts (`lang_class::native_accessor_argument_refusal`).
    pub reflect_native_accessor_arg_texts: bool,

    /// `CRATONVM_REFLECT_DOOR_NATIVE_ACCESSOR_ONLY` — [`parse::on_unless_zero`] (a correctness fix
    /// toward HotSpot; `=0` is the kill switch). JIT round 21 wave 8, lane reflect2: the
    /// `Method.invoke` / `Constructor.newInstance` doors honour `-Djdk.reflect.useNativeAccessorOnly=true`
    /// (JDK 25 then picks `NativeAccessor` for every member) and give the native accessor's texts for
    /// every member (`lang_class::door_honours_native_accessor_only`, read on a refusal only).
    pub reflect_door_native_accessor_only: bool,

    /// `CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// JIT round 21 wave 1, lane reflect: with `CRATONVM_JIT_MH_COMPILED_LINK`, a compiled
    /// `invokeBasic` / linked `invokeExact` / `linkToStatic` whose target is a static member of an
    /// initialized HIDDEN class (a spun `LambdaForm$MH` / `$DMH` form) calls it by its owner's id
    /// (`interpreter::execute`, the door's own re-entry, frames real) instead of the native door
    /// (`vm/src/jit/helpers.rs` `mh_link_leaf_static` / `mh_hidden_static_call`).
    pub jit_mh_compiled_link_hidden: bool,

    /// `CRATONVM_CLASS_GET_NAME_MIRROR_MEMO` — [`parse::truthy_word_default_true`] (default ON since the r22 A/B, R22-AB-FLIP; `=0` off; performance).
    /// JIT round 21 wave 1, lane reflect: the leaf `Class.getName()` answers first from a per-thread,
    /// per-VM memo keyed by (collection address epoch, mirror address) with the mirror's `name` slot as
    /// its guard, lock-free, for every mirror -- arrays and primitives included, which the class-id memo
    /// (`CRATONVM_CLASS_GET_NAME_MEMO`) never serves (`lang_class::class_get_name_mirror_memo_hit`).
    pub class_get_name_mirror_memo: bool,

    /// `CRATONVM_JIT_REFLECT_CTOR_LINK` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// JIT round 21 wave 1, lane reflect (REFLECT21-3): the compiled `invokeExact` site of
    /// `DirectConstructorHandleAccessor.invokeImpl` allocates the instance and calls its `<init>` as an
    /// ordinary compiled call (`vm/src/jit/helpers.rs` `reflect_ctor_link_redirect`).
    pub jit_reflect_ctor_link: bool,

    /// `CRATONVM_JIT_MH_COMPILED_LINK_PAIR_FIRST` — [`parse::on_unless_zero`]
    /// (perf, default on, `=0` off). JIT round 21 lane reflect (page
    /// `jr21-reflective-invoke-and-mcs-flip-handoff-rows-20261014.md`): the
    /// compiled `invokeExact` link (`lang_invoke::mh_compiled_linker_pair`)
    /// asks the VM's linker table first and takes a row's existence as the
    /// proof that the site's descriptor is shareable (the door files a row only
    /// after that check), instead of re-parsing the descriptor and looking up
    /// each of its classes on every call (`Method.invoke`'s `invokeExact`).
    pub jit_mh_compiled_link_pair_first: bool,

    /// `CRATONVM_JIT_MH_COMPILED_LINK_LEAN` — [`parse::on_unless_zero`] (perf,
    /// default on, `=0` off). JIT round 21 lane reflect2 (page
    /// `jr21-reflect2-method-invoke-per-call-link-work-FIXED-20261015.md`): the
    /// compiled signature-polymorphic link (`vm/src/jit/helpers.rs`
    /// `mh_compiled_link_redirect`) checks the site / member basic-type
    /// agreement without allocating, takes a `MemberName` decode this epoch's
    /// memo already holds without the class-name check, and builds the
    /// linker-table key of a compiled `invokeExact` in one `String`
    /// (`lang_invoke::mh_compiled_linker_pair`), on every `Method.invoke`.
    pub jit_mh_compiled_link_lean: bool,

    /// `CRATONVM_JIT_MH_COMPILED_LINK_AGREE_MEMO` — [`parse::truthy_word`]
    /// (perf, OPT-IN since the round-21 w3i A/B: R21ReflectInvoke reflect-static-obj
    /// 3229 ns with both memos vs 36 without, reflect-virtual 12500 vs 1330). JIT round 21 lane reflect3 (page
    /// `jr21-reflect3-method-invoke-steady-state-string-work-20261014.md`):
    /// the compiled signature-polymorphic link's basic-type agreement
    /// (`vm/src/jit/helpers.rs` `mh_link_basic_agree`, two descriptor parses
    /// per shape per `Method.invoke`) is memoised on the member's per-epoch
    /// decode record, keyed by the site descriptor's interned text.
    pub jit_mh_compiled_link_agree_memo: bool,

    /// `CRATONVM_JIT_MH_COMPILED_LINK_PAIR_MEMO` — [`parse::truthy_word`]
    /// (perf, OPT-IN since the round-21 w3i A/B, see the agree memo above). JIT round 21 lane reflect3 (REFLECT2-21-4
    /// / REFLECT21-3): a compiled `invokeExact` link's linker pair
    /// (`lang_invoke::mh_compiled_linker_pair`: a key `String` and a locked
    /// table probe per `Method.invoke`) is memoised on the handle's per-epoch
    /// record, keyed by the site descriptor's interned text and caller class.
    pub jit_mh_compiled_link_pair_memo: bool,

    /// `CRATONVM_FORNAME_BUILTIN_LOADED_FIRST` — [`parse::on_unless_zero`]
    /// (perf, default on, `=0` off). JIT round 21 lane reflect3: under
    /// `--jdk-only`, `Class.forName` with the application or platform loader
    /// asks that loader's `findLoadedClass0` native first and takes a hit as
    /// the answer, as HotSpot's `SystemDictionary` takes its dictionary hit,
    /// instead of running `loadClass` -> `loadClassOrNull` -> `findLoadedClass`
    /// as interpreted bytecode for the same answer on every call.
    pub forname_builtin_loaded_first: bool,

    /// `CRATONVM_MH_BOUND_FIELD_OWNER` — [`parse::on_unless_zero`] (a correctness fix toward HotSpot; `=0` is the kill switch).
    /// Interpreter round i2 wave 65, lane L4 (proposal
    /// `i64-L4-proposal-bound-field-handles-keep-owner-and-slot`): `bindTo` of
    /// an instance field getter / setter that recorded its owner's `ClassId`
    /// (`CRATONVM_MH_OWNER_ID_AUTHORITY`) carries the owner -- and the slot
    /// minted under `CRATONVM_MH_FIELD_SLOT_FROM_MINT` -- to the bound copy, so
    /// the door's bound `GETTER` / `SETTER` arm resolves the slot in the
    /// owner's layout by id instead of re-resolving `MH_CLASS` by NAME per
    /// call (two loaders' classes of one name with different layouts read the
    /// wrong slot of the bound receiver).
    pub mh_bound_field_owner: bool,

    /// `CRATONVM_MH_FIELD_LINK_OWNER_ADMITS` — [`parse::on_unless_zero`] (default on since the wave-65 triage: polymorphic getter -62%, `=0` turns it off).
    /// Interpreter round i2 wave 65, lane L4 (proposal
    /// `i64-L4-proposal-field-link-admits-the-owners-subclasses`): an instance
    /// field-accessor link of a handle with a recorded owner and the owner's
    /// slot admits any receiver whose class is the owner or a subclass of it
    /// (by id, one remembered subclass), instead of exactly the one receiver
    /// class it was linked for -- a polymorphic receiver no longer relinks the
    /// handle on every alternation, nor loses the link after seven.
    pub mh_field_link_owner_admits: bool,

    /// `CRATONVM_MH_FIELD_LINK_BOUND` — [`parse::on_unless_zero`] (default on since the wave-66 triage, together with its sibling: bound field calls -62%, `bindTo` -47%; `=0` off).
    /// Interpreter round i2 wave 66, lane L4 (proposal
    /// `i65-L4-proposal-bound-field-handles-link-too`): the field-accessor link
    /// also serves `invokeExact` on a BOUND instance field getter / setter
    /// (`getter.bindTo(obj)`) whose recorded owner admits the bound receiver by
    /// id and whose slot is the owner's (minted, or found in the owner by id);
    /// `bindTo` then mints such a copy as wide as an unbound handle so the link
    /// slots exist. A linked call re-checks the bound receiver's exact class.
    pub mh_field_link_bound: bool,

    /// `CRATONVM_MH_BINDTO_FAST_MINT_FIELDS` — [`parse::on_unless_zero`] (default on since the wave-66 triage, together with its sibling: bound field calls -62%, `bindTo` -47%; `=0` off).
    /// Interpreter round i2 wave 66, lane L4 (proposal
    /// `i65-L4-proposal-bindto-fast-mint-covers-field-handles`): with
    /// `CRATONVM_MH_BINDTO_FAST_MINT`, a repeat `bindTo` of an unbound instance
    /// field getter / setter takes the memoised mint too, carrying the
    /// source's recorded owner and minted slot as the slow path does.
    pub mh_bindto_fast_mint_fields: bool,

    /// `CRATONVM_MH_BINDTO_PRELINK_FIELD` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// Interpreter round i2 wave 67, lane L4 (proposal
    /// `i66-L4-proposal-bindto-pre-links-a-bound-field-copy`): with
    /// `CRATONVM_MH_FIELD_LINK_BOUND`, `bindTo` writes the bound field link on
    /// the copy it mints (a getter, or a setter whose value needs no check),
    /// so the copy's first `invokeExact` is already served by the link.
    pub mh_bindto_prelink_field: bool,

    /// `CRATONVM_MH_FIELD_LINK_SERVES_INVOKE` — [`parse::on_unless_zero`] (default on since the wave-67 triage: field-handle `invoke` -60..-70%; `=0` off).
    /// Interpreter round i2 wave 67, lane L4 (proposal
    /// `i66-L4-proposal-field-link-serves-invoke-at-the-declared-type`): the
    /// `invoke` door serves a LINKED field getter / setter through the link
    /// when the call site is exactly the link's declared type (`invoke` at the
    /// declared type is `invokeExact`); any other site keeps the door.
    pub mh_field_link_serves_invoke: bool,

    /// `CRATONVM_MH_FIELD_LINK_SITE_BY_ARC` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// Interpreter round i4 wave 91, lane L4 (proposal
    /// `i71-L4-proposal-linked-calls-judge-the-site-by-its-shared-text`): a
    /// linked field call compares its call site with the link's declared
    /// descriptor through a per-thread copy of the interned text's `Arc`, not
    /// under the process-wide field-link sites `RwLock`.
    pub mh_field_link_site_by_arc: bool,

    /// `CRATONVM_MH_FIELD_LINK_FIRST` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// Interpreter round i4 wave 94, lane L4 (proposal
    /// `i71-L4-proposal-a-linked-field-accessor-is-served-before-the-real-handle-lanes`):
    /// the `invokeExact` / `invoke` natives serve a LINKED synthetic field getter / setter
    /// at its declared site right after taking the call site, ahead of the real-handle
    /// lanes that all decline for it, keeping the refusals that can fire for it (the site's
    /// access refusal; for `invoke`, the loader cast and the return cast) in their order.
    pub mh_field_link_first: bool,

    /// `CRATONVM_MH_LINKER_BASIC_MEMBER` — [`parse::on_unless_zero`] (default on since the round i4 wave-97 triage, `=0` off; performance).
    /// Interpreter round i4 wave 95, lane L4: the JDK invoker linker of a REAL handle at a
    /// call site naming a class outside `java/` (which no caller-keyed pair row may hold)
    /// reuses the linker `MemberName` filed per (caller, name, basic type) -- what
    /// `Invokers.methodHandleInvokeLinkerMethod` answers is a function of the site's basic
    /// type -- with the site's `MethodType` from the site-type table as the appendix,
    /// instead of running the linker method and three full root refreshes per call.
    pub mh_linker_basic_member: bool,

    /// `CRATONVM_MH_REAL_FIELD_BEFORE_LINKER` — [`parse::on_unless_zero`] (default on since the round i4 wave-97 triage, `=0` off; performance).
    /// Interpreter round i4 wave 95, lane L4: a REAL field accessor (`DirectMethodHandle$Accessor`
    /// / `$StaticAccessor`) called by `invokeExact` / `invoke` at a site whose `MethodType`
    /// (the site-type table's row) IS the handle's type is served from its record ahead of
    /// the JDK invoker linker, when nothing the linker's `LambdaForm` chain would run can
    /// be seen: a non-null base, an initialized holder, a value the setter's cast admits.
    pub mh_real_field_before_linker: bool,

    /// `CRATONVM_MH_REAL_FIELD_SITE_MEMO` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// Interpreter round i5 wave 100, lane L4 (page
    /// `i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call`): the field-before-linker
    /// lane remembers, per thread, that a (handle, caller, site) passed its site checks (the
    /// site-type row IS the handle's `type`, no site access refusal) within one collection
    /// epoch, so a warm call skips the by-name `type` read, the site-type key build and table
    /// probe, and the access classification.
    pub mh_real_field_site_memo: bool,

    /// `CRATONVM_MH_ARMED_CALLER` — [`parse::truthy_word_default_true`] (default ON since the
    /// wave-107 A/B on Windows: warm field-handle rows 0.64-0.77x, `jdk-site` 0.80x; `=0` restores the walk).
    /// Interpreter round i5 wave 107, lane L4 (page
    /// `i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call`): the before-linker lanes
    /// (field, bound field, direct) probe the site memo first with the caller class the
    /// arming door published (`poly_call_site::arm_with_caller`: a compiled site's
    /// `JitInvokeInfo::declaring_class_id`), so a warm call skips the caller walk
    /// (`frame_class_ids_tail(1)`, a compiled-frame walk under the JIT). A miss walks as before.
    pub mh_armed_caller: bool,

    /// `CRATONVM_MH_REAL_FIELD_RAW_RETURN` — [`parse::truthy_word_default_true`] (default ON since the
    /// w109 A/B on Windows: getters 0.68-0.91x in both modes, setters flat; `=0` off; was default off, opt-in, performance).
    /// Interpreter round i5 wave 108, lane L4 (page
    /// `i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call`): the before-linker
    /// field lanes (unbound and bound) hand a primitive GETTER value back raw when the site
    /// returns exactly that primitive, as the direct lane does
    /// (`CRATONVM_MH_DIRECT_LANE_RAW_RETURN`), instead of boxing it for the caller to unbox.
    pub mh_real_field_raw_return: bool,

    /// `CRATONVM_MH_REAL_BOUND_FIELD_BEFORE_LINKER` — `truthy_word_default_true` (default ON since its triage, `=0` off, performance).
    /// Interpreter round i5 wave 101, lane L4 (item 1 of
    /// `i100-L4-proposal-method-handle-warm-call-directions`): a `BoundMethodHandle$Species_LL`
    /// proven, on its first call, to be the JDK's own `accessor.bindTo(base)` of a real
    /// instance field accessor (same species, `form`, `type`, `argL0`, `argL1`) is recorded
    /// per thread and collection epoch, and then served like the unbound accessor called with
    /// the base, ahead of the JDK invoker linker. Needs `CRATONVM_MH_REAL_FIELD_BEFORE_LINKER`.
    pub mh_real_bound_field_before_linker: bool,

    /// `CRATONVM_MH_INTERP_EXACT_BOUND_SPECIES_PER_HANDLE` — [`parse::truthy_word_default_true`]
    /// (default on since 2026-10-24, `=0` off; performance; `--nojit` only, read under
    /// `CRATONVM_MH_INTERP_BASIC_FRAME_PUSH`). WARNING: i8-default-flip (page i147-orch).
    /// Interpreter round i8 (page `i147-orch`): the interpreted `invokeExact` linker push asks
    /// whether the bound-field lane can serve a `BoundMethodHandle$Species_LL` per handle (its
    /// `argL0` is a `DirectMethodHandle$Accessor`) instead of by class, so the `asType` adapter
    /// of a typed reflective constructor or method pushes the linker's frames instead of taking
    /// the door's native re-entry on every call.
    pub mh_interp_exact_bound_species_per_handle: bool,

    /// `CRATONVM_MH_REAL_DIRECT_BEFORE_LINKER` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// Interpreter round i5 wave 102, lane L4 (page
    /// `i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call`): a REAL plain
    /// `DirectMethodHandle` of a static or virtual method called by `invokeExact` / `invoke` at a
    /// site whose `MethodType` IS the handle's type is called directly (the real direct lane)
    /// ahead of the JDK invoker linker, when nothing the linker's `LambdaForm` chain would run
    /// can be seen: a non-null receiver, an initialized holder, stack headroom.
    pub mh_real_direct_before_linker: bool,

    /// `CRATONVM_MHN_CALLER_SENSITIVE_FLAG` — [`parse::on_unless_zero`] (default on, `=0` off; HotSpot parity).
    /// Interpreter round i4 wave 95, lane L4: `MethodHandleNatives.resolve` / `init` set
    /// `MN_CALLER_SENSITIVE` on a method member that is `@CallerSensitive` in a boot- or
    /// platform-loader class (HotSpot's `init_method_MemberName`), so `Lookup.findVirtual` /
    /// `findStatic` / `unreflect` bind it to the lookup class's injected invoker
    /// (`MethodHandleImpl.BindCaller`) and refuse a restricted lookup, as HotSpot does.
    pub mhn_caller_sensitive_flag: bool,

    /// `CRATONVM_MHN_RESOLVE_NAMED_METHODS` — [`parse::truthy_word_default_true`] (default ON since the i5 triage, `=0` off, performance).
    /// Interpreter round i5 wave 100, lane L4 (page `i89-L4-method-finder-per-class-and-per-handle-cost`):
    /// `MethodHandleNatives.resolve` of a method member asks the VM for the declared methods
    /// of the member's NAME only (`NativeClassAccess::declared_methods_named`) on each class
    /// of its resolution path, instead of copying every declared method's metadata (name,
    /// descriptor, decoded `Exceptions` and `Signature`) about three times per class.
    pub mhn_resolve_named_methods: bool,

    /// `CRATONVM_MHN_RESOLVE_NAMED_FIELDS` — [`parse::truthy_word_default_true`] (default ON since the i5 triage, `=0` off, performance).
    /// Interpreter round i5 wave 102, lane L4 (page `i89-L4-method-finder-per-class-and-per-handle-cost`):
    /// the field half of `CRATONVM_MHN_RESOLVE_NAMED_METHODS`. `MethodHandleNatives.resolve` of a
    /// field member, the declared-field access flags and the `MemberName` / `VarHandle` layout
    /// tests ask the VM for the declared fields of one NAME only
    /// (`NativeClassAccess::declared_fields_named`) instead of copying every declared field.
    pub mhn_resolve_named_fields: bool,

    /// `CRATONVM_MHN_RESOLVE_NAMED_SUPERINTERFACES` — [`parse::truthy_word_default_true`] (default ON since the w104 triage, `=0` off, performance).
    /// Interpreter round i5 wave 103, lane L4 (page `i89-L4-method-finder-per-class-and-per-handle-cost`):
    /// the superinterface step of method resolution (`lk_superinterface_declares_instance_method`,
    /// asked by `MethodHandleNatives.resolve` for an inherited default method, by the
    /// `--compatible` finders and by the lambda-proxy access flags) asks the VM for the declared
    /// methods of one NAME per superinterface (`NativeClassAccess::declared_methods_named`) instead
    /// of copying each interface's whole method table.
    pub mhn_resolve_named_superinterfaces: bool,

    /// `CRATONVM_MH_INVOKE_LINKS_FIELD` — [`parse::on_unless_zero`] (default on since the wave-68 triage, performance).
    /// Interpreter round i2 wave 68, lane L4 (proposal
    /// `i67-L4-proposal-invoke-links-a-field-accessor-at-the-declared-type`):
    /// with `CRATONVM_MH_FIELD_LINK_SERVES_INVOKE`, an `invoke` whose call
    /// site is exactly a field getter / setter's declared type LINKS the
    /// handle (as `invokeExact` does), so a handle only ever called through
    /// `invoke` is served by the link from its second call on.
    pub mh_invoke_links_field: bool,

    /// `CRATONVM_MH_BINDTO_FAST_MINT_PRELINK` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// Interpreter round i2 wave 68, lane L4 (proposal
    /// `i67-L4-proposal-fast-mint-line-carries-the-pre-link`): with
    /// `CRATONVM_MH_BINDTO_PRELINK_FIELD`, a `bindTo` fast-mint line remembers
    /// the bound field link the slow path's pre-link wrote, and a hit whose
    /// bound receiver is of exactly the remembered class writes those link
    /// words instead of re-deriving them.
    pub mh_bindto_fast_mint_prelink: bool,

    /// `CRATONVM_MH_LINKING_CALL_SERVED` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// Interpreter round i2 wave 69, lane L4 (proposal
    /// `i68-L4-proposal-a-linking-call-is-served-by-its-link`): the
    /// `invokeExact` (and, with `CRATONVM_MH_INVOKE_LINKS_FIELD`, `invoke`)
    /// call that writes a field getter / setter link is served by that link
    /// when its call site is exactly the declared type, instead of the door.
    pub mh_linking_call_served: bool,

    /// `CRATONVM_MH_BINDTO_FAST_MINT_TWO_CLASSES` — [`parse::on_unless_zero`] (default on since the wave-69 triage, performance).
    /// Interpreter round i2 wave 69, lane L4 (proposal
    /// `i68-L4-proposal-fast-mint-line-keeps-a-second-receiver-class`): a
    /// `bindTo` fast-mint line refilled for the same source key with a second
    /// admitted receiver class keeps the first one (and its remembered
    /// pre-link) too, so a source bound alternately to two classes hits.
    pub mh_bindto_fast_mint_two_classes: bool,

    /// `CRATONVM_MH_INVOKE_LINK_DECLARED_ONCE` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// Interpreter round i2 wave 70, lane L4 (proposal
    /// `i69-L4-proposal-invoke-link-renders-the-declared-type-once`): building
    /// on `CRATONVM_MH_INVOKE_LINKS_FIELD`, `invoke`'s link of a field getter /
    /// setter hands the declared type it already compared (and the candidate
    /// screen it already ran) to the link, instead of looking both up again.
    /// A pure refactor: every answer is the same.
    pub mh_invoke_link_declared_once: bool,

    /// `CRATONVM_MH_FIELD_LINK_DERIVATION_MEMO` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// Interpreter round i2 wave 70, lane L4 (proposal
    /// `i69-L4-proposal-field-link-derivation-is-memoised-per-source-shape`):
    /// a per-thread memo of the receiver-independent words of an instance
    /// field-accessor link, keyed by the handle's member slots, `type` object
    /// and prior link word in one collection epoch; a linking call of the same
    /// shape whose receiver (and checked value) class is the remembered one
    /// writes those words instead of re-deriving them.
    pub mh_field_link_derivation_memo: bool,

    /// `CRATONVM_MH_FINDER_SHARED_MEMBER_WORDS` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// Interpreter round i3 wave 71, lane L4 (proposal
    /// `i70-L4-proposal-finder-handles-share-their-member-words`): a field
    /// getter / setter handle minted by a finder takes its `MH_CLASS` /
    /// `MH_NAME` / `MH_DESC` strings from the intern pool and its `type` from
    /// the shared bound-`MethodType` table, so two finder handles of one field
    /// share every word a field-link derivation memo key reads.
    pub mh_finder_shared_member_words: bool,

    /// `CRATONVM_MH_LINKING_CALL_DIRECT_SERVE` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// Interpreter round i3 wave 71, lane L4 (proposals
    /// `i70-L4-proposal-a-memo-hit-serves-the-linking-call`,
    /// `i68-L4-proposal-a-linking-call-is-served-by-its-link`): building on
    /// `CRATONVM_MH_LINKING_CALL_SERVED`, the linking call is served from what
    /// the link attempt (a derivation or a derivation-memo hit) admitted, with
    /// no re-read of the words it just wrote.
    pub mh_linking_call_direct_serve: bool,

    /// `CRATONVM_MH_SITE_TYPE_TABLE` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// Interpreter round i3 wave 72, lane L4 (proposal
    /// `i72-L4-proposal-jdk-only-field-handle-mint-and-converting-invoke-cost`):
    /// `invoke` on a real JDK method handle at a call site of another type
    /// takes the site's `MethodType` from this VM's bound-`MethodType` table
    /// (keyed by the descriptor and the caller's built-in loader) instead of
    /// running `MethodType.fromMethodDescriptorString` on every call.
    pub mh_site_type_table: bool,

    /// `CRATONVM_MH_SITE_TYPE_BOOT_LOADER` — [`parse::truthy_word_default_true`] (default ON
    /// since 2026-10-11, `0` / `false` / `off` / `no` turn it off; correctness). Interpreter round i3 wave 79, lane L4 (page
    /// `i72-L4-a-bootstrap-callers-converting-invoke-resolves-its-site-through-the-system-loader`):
    /// `invoke` on a real JDK method handle at a converting call site whose
    /// caller is a BOOT-loader class resolves the site's `MethodType` with the
    /// boot loader (`MethodType.fromDescriptor(desc, null)`), as HotSpot links
    /// an `invokehandle` site with the caller's loader, instead of
    /// `fromMethodDescriptorString(desc, null)`, whose `null` is the SYSTEM
    /// loader. Only when the caller frame is exact (an interpreter frame, or a
    /// compiled one whose reported class is the running code's): otherwise the
    /// system loader, which sees every boot class too, keeps the site resolving.
    pub mh_site_type_boot_loader: bool,

    /// `CRATONVM_MHN_RESOLVE_MEMO` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// Interpreter round i3 wave 73, lane L4 (proposal
    /// `i72-L4-proposal-mhn-resolve-memoises-its-member-answers`): the
    /// `MethodHandleNatives` natives a field finder calls under `--jdk-only`
    /// (`resolve` of a field `MemberName`, `objectFieldOffset`,
    /// `staticFieldOffset`, `staticFieldBase`) answer from a per-thread,
    /// VM-keyed memo valid for one collection epoch, keyed by the member's
    /// `clazz` / `name` / `type` objects and requested flags; only a FOUND
    /// member is remembered, and every write the natives make stays per call.
    pub mhn_resolve_memo: bool,

    /// `CRATONVM_MHN_RESOLVE_METHOD_MEMO` — [`parse::truthy_word`] (default off, opt-in,
    /// performance). Interpreter round i3 wave 81, lane L4 (proposal
    /// `i73-L4-proposal-method-member-resolve-memo-and-mirror-judged-load`):
    /// `MethodHandleNatives.resolve` of a METHOD `MemberName` (ref kinds 5-9)
    /// on the JDK's own layout answers a member it found before, in one
    /// collection epoch on one thread, from a memo keyed by the member's
    /// `clazz` / `name` / `type` objects, the requested flags, the caller
    /// class, the lookup mode and the speculative bit; only a FOUND member is
    /// remembered, and its `flags` write is replayed per call.
    pub mhn_resolve_method_memo: bool,

    /// `CRATONVM_MHN_RESOLVE_DIRECT_CALL` — [`parse::truthy_word_default_true`] (default ON,
    /// kill switch `0` / `false` / `off` / `no`; correctness). Interpreter round i5 wave 108,
    /// lane L3 (probe `L3W108FinalVirtualHandleKind`): `MethodHandleNatives.resolve` settles a
    /// virtual (or interface-to-`Object`) method HotSpot binds statically -- private, final,
    /// or of a final class -- as `REF_invokeSpecial`, as `init_method_MemberName`'s
    /// `direct_call`, so the JDK links its handle to the `invokeSpecial` form.
    pub mhn_resolve_direct_call: bool,

    /// `CRATONVM_MH_COMPAT_FINDER_NEST_ACCESS` — [`parse::truthy_word_default_true`] (default
    /// ON, kill switch `0` / `false` / `off` / `no`; correctness). Interpreter round i3 wave 81,
    /// lane L4: the `--compatible` `Lookup` method finders (`findVirtual`, `findStatic`,
    /// `findSpecial`, `findConstructor`) refuse a PRIVATE method or constructor of a class
    /// that is not the lookup class or its nestmate, as HotSpot's member resolution does
    /// (`IllegalAccessException: no such method|constructor: ...`), even through a lookup
    /// with `PRIVATE` mode.
    pub mh_compat_finder_nest_access: bool,

    /// `CRATONVM_MH_COMPAT_FINDER_LOOKUP_CLASS_ACCESS` — [`parse::truthy_word_default_true`]
    /// (default ON, kill switch `0` / `false` / `off` / `no`; correctness). Interpreter round i4
    /// wave 92, lane L4 (proposal `i40-L4-proposal-lookup-finders-ask-the-lookup-class`): a
    /// `--compatible` `Lookup` holding `PRIVATE` asks its lookup class, as HotSpot's
    /// `VerifyAccess.isMemberAccessible` does. The field finders (`findGetter`, `findSetter`,
    /// `findStaticGetter`, `findStaticSetter`, `findVarHandle`, `findStaticVarHandle`) refuse
    /// another nest's private field, another run-time package's package-private field and a
    /// non-subclass's protected field (`IllegalAccessException: member is ...`); the method
    /// finders refuse another package's package-private method (`no such method`, as
    /// resolution does), beside `mh_compat_finder_nest_access`'s private rule.
    pub mh_compat_finder_lookup_class_access: bool,

    /// `CRATONVM_MH_UNNAMEABLE_MIRROR_OWN_NAME` — [`parse::truthy_word_default_true`] (default
    /// ON, kill switch `0` / `false` / `off` / `no`; correctness, both modes). Interpreter round
    /// i4 wave 92, lane L4 (proposal
    /// `i62-L4-proposal-render-an-unnameable-mirror-as-itself-not-object`): a lambda proxy's
    /// mirror (no class-store name, no name slot) renders in a `MethodType` descriptor as the
    /// proxy's own internal name `L<host>$$Lambda/0x<id>;`, not `Ljava/lang/Object;`
    /// (`mirror_to_descriptor`) or "unrenderable" (`link_to_type_descriptor`), and that name
    /// resolves back to the proxy (`mirror_for_descriptor_name_resolving`) and casts only the
    /// proxy's own instances (`reference_arg_admitted`).
    pub mh_unnameable_mirror_own_name: bool,

    /// `CRATONVM_MH_REAL_FIELD_RECORD` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// Interpreter round i3 wave 73, lane L4 (proposal
    /// `i72-L4-proposal-a-real-field-accessor-is-served-from-its-handle-record`):
    /// `invokeExact` at its declared type, and `invokeBasic`, on a REAL
    /// `DirectMethodHandle$Accessor` / `$StaticAccessor` read the field shape
    /// (reference kind, slot or static target, field descriptor) from the
    /// handle's memoised record instead of re-deriving it on every call.
    pub mh_real_field_record: bool,

    /// `CRATONVM_MH_REAL_CONSTRUCTOR_LANE` — [`parse::truthy_word`] (default off, opt-in,
    /// performance). Interpreter round i4 wave 88, lane L3: `invokeExact` at its declared
    /// type on a REAL nullary `DirectMethodHandle$Constructor` whose instance class is
    /// initialized (what `Constructor.newInstance()`'s `DirectConstructorHandleAccessor`
    /// calls on every use) allocates the instance and runs the `<init>` directly instead of
    /// running the handle's `LambdaForm`.
    pub mh_real_constructor_lane: bool,

    /// `CRATONVM_MH_REAL_CTOR_CAST_ADAPTER_LANE` — [`parse::truthy_word_default_true`] (default on
    /// since 2026-10-24, `=0` off; performance; `--nojit` only). WARNING: i9-default-flip (page
    /// i147-orch). Interpreter round i9 (page `i147-orch`): `invokeExact` at `(Object)Object`
    /// on the `asType` adapter `MethodHandleAccessorFactory.makeConstructorHandle` builds for a
    /// one-argument constructor with a typed reference parameter (a `BoundMethodHandle$Species_LL`
    /// over the `DirectMethodHandle$Constructor` and a bound `Class.cast`) checks the cast and
    /// runs the allocation and `<init>` directly, as `CRATONVM_MH_REAL_CONSTRUCTOR_LANE` does for
    /// a nullary constructor. An argument the cast would reject keeps the JDK chain.
    pub mh_real_ctor_cast_adapter_lane: bool,

    /// `CRATONVM_MH_REAL_CTOR_CAST_ADAPTER_LANE_UNDER_JIT` — [`parse::truthy_word`] (default
    /// off, opt-in; performance A/B lever, t1 wave 2 lane M, page
    /// `t1-H-proposal-jdk-only-jit-reflective-ctor-slower-than-nojit`): lifts the `--nojit`-only
    /// gate of [`Self::mh_real_ctor_cast_adapter_lane`] (both its door decline and the
    /// interpreter push's `mh_real_constructor_lane_may_serve` answer), so the lane also serves
    /// a VM that runs a JIT. The Linux "no gain" measurement that put the gate in predates the
    /// current JIT; flip only after an interleaved A/B on `T1HCtorCastAdapter bench`.
    pub mh_real_ctor_cast_adapter_lane_under_jit: bool,

    /// `CRATONVM_MH_REAL_CONSTRUCTOR_LANE_MEMO` — [`parse::truthy_word`] (default off, opt-in,
    /// performance). Interpreter round i4 wave 89, lane L3: the constructor lane above keeps
    /// its decode of a handle (instance class, `<init>` owner, the type-matches-site answer)
    /// in a per-thread memo keyed by (VM, collection epoch, handle address), as the
    /// real-handle record memo does, instead of four by-name reads and a rendered method
    /// descriptor per call.
    pub mh_real_constructor_lane_memo: bool,

    /// `CRATONVM_MH_LANES_KEEP_CUSTOMIZATION` — [`parse::truthy_word_default_true`] (default on,
    /// HotSpot parity, `=0` the old answer). JIT round 22 wave 3, lane runtime (page
    /// `docs/feature-designs/class-resolution-and-caching.md`):
    /// the native lanes that serve a real handle's `invokeExact` / `invoke` ahead of the JDK
    /// invoker linker (constructor, direct, field, bound-field, the JIT's warm field lane) run
    /// `Invokers.checkCustomized`'s count as the skipped `invokeExact_MT` does, and decline the
    /// call at `CUSTOMIZE_THRESHOLD` so the linker customizes the handle exactly when HotSpot
    /// does; the constructor lane also declines once hidden frames were observed, so a
    /// `SHOW_HIDDEN_FRAMES` walk sees the customized form's real frame.
    pub mh_lanes_keep_customization: bool,

    /// `CRATONVM_MH_REAL_FIELD_CONVERTING_SERVE` — [`parse::truthy_word_default_true`] (default on since the wave-80 triage,
    /// performance). Interpreter round i3 wave 80, lane L4 (proposal
    /// `i73-L4-proposal-converting-invoke-of-a-real-field-accessor-is-served-from-its-record`):
    /// `invoke` on a REAL field getter at a site that differs from its type only
    /// by `asType`'s boxing / widening / receiver cast is served from the
    /// handle's record once the handle's `asTypeCache` holds that site's adapter.
    pub mh_real_field_converting_serve: bool,

    /// `CRATONVM_MH_METHOD_TYPE_INTERN_MEMO` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// Interpreter round i3 wave 74, lane L4 (proposal
    /// `i74-L4-proposal-the-jdk-only-finder-mint-cost-model`): a reviewed
    /// `Intrinsic` over `MethodType.makeImpl` (registered only while this is
    /// on) answers a type this thread interned before, in the same VM and
    /// collection epoch, from a per-thread memo keyed by the return and
    /// parameter `Class` objects; anything else runs the JDK's own body.
    /// Wave 75: registered only in a VM without a JIT (the JIT arm measured
    /// worse on every finder row), and the memo is two-way and grows on
    /// thrash (the miss penalty of wave 74's 128-line direct-mapped memo).
    pub mh_method_type_intern_memo: bool,

    /// `CRATONVM_OBJECT_CLONE_CLASS_FACTS` — [`parse::truthy_word`] (default off, opt-in, performance).
    /// Interpreter round i3 wave 75, lane L4 (proposal
    /// `i74-L4-proposal-object-clone-answers-its-class-questions-once`): a
    /// plain-object `Object.clone` answers "is the class `Cloneable`" and
    /// "which side-table overlay does it carry" from a per-thread memo keyed by
    /// (VM, collection epoch, `ClassId`) instead of walking the hierarchy (a
    /// by-name lookup, interface worklists, a `String` per superclass) per call.
    pub object_clone_class_facts: bool,

    /// `CRATONVM_MH_COMPAT_METHOD_TYPE_BYTECODE` — [`parse::truthy_word`] (default off, opt-in).
    /// Interpreter round i3 wave 76, lane L4 (proposal
    /// `i75-L4-proposal-compatible-method-type-factories-yield-to-bytecode`):
    /// over a real JDK the four `MethodType.methodType` `Bridge`s are not
    /// registered, so `--compatible` runs the JDK's own factories (one
    /// `makeImpl` per call, its exceptions thrown from Java with the JDK's
    /// frames) instead of a native crossing plus a three-call chain into
    /// `genericMethodType` / `insertParameterTypes` / `changeReturnType`.
    /// A synthetic JDK keeps the bridges.
    pub mh_compat_method_type_bytecode: bool,

    /// `CRATONVM_MH_FINDER_MINT_MEMO` — [`parse::truthy_word_default_true`] (default on since
    /// interpreter round i3 wave 78; `0` / `false` / `off` / `no` turn it off; performance).
    /// Interpreter round i3 wave 77, lane L4 (proposal
    /// `i74-L4-proposal-a-repeat-field-finder-is-minted-from-its-first-answer`):
    /// under `--jdk-only` over a real JDK in a VM without a JIT, reviewed
    /// `Intrinsic`s over `Lookup.findGetter` / `findSetter` /
    /// `findStaticGetter` / `findStaticSetter` run the JDK's own body once per
    /// (lookup class, previous lookup class, modes, refc, name, type) in a
    /// collection epoch and mint a repeat call's answer (a fresh
    /// `DirectMethodHandle$Accessor` / `$StaticAccessor` and a fresh resolved
    /// `MemberName`, word for word the first answer's) from the filed one.
    pub mh_finder_mint_memo: bool,

    /// `CRATONVM_MH_METHOD_FINDER_MINT_MEMO` — [`parse::truthy_word`] (default off, opt-in,
    /// performance). Interpreter round i3 wave 82, lane L4 (proposal
    /// `i81-L4-proposal-a-repeat-method-finder-is-minted-from-its-first-answer`):
    /// under `--jdk-only` over a real JDK, without a JIT or under the
    /// `method-finder-mint` row of `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS`,
    /// reviewed `Intrinsic`s over `Lookup.findVirtual` / `findStatic` /
    /// `findSpecial` / `findConstructor` / `unreflect` run the JDK's own body
    /// once per (lookup class, previous lookup class, modes, arguments) in a
    /// collection epoch and mint a repeat call's answer (a fresh direct handle
    /// of the filed class and a fresh resolved `MemberName`, word for word the
    /// first answer's) when that answer is exactly a `DirectMethodHandle`,
    /// `$Special`, `$Interface` or `$Constructor`.
    pub mh_method_finder_mint_memo: bool,

    /// `CRATONVM_MH_METHOD_FINDER_MINT_INIT_REFILE` — [`parse::truthy_word_default_true`]
    /// (default on since the wave-90 triage, `=0` off; performance). Interpreter round i4 wave 90, lane L4: a method finder mint
    /// line filed for a STATIC method while its holder was uninitialized is refused once
    /// the holder is initialized (the JDK's answer then carries the barrier-free form).
    /// With the switch, such a line is re-filed and served instead when the filed
    /// handle's own `form` already IS that answer: the filed form is the
    /// `DIRECT_INVOKE_STATIC_INIT` one, and the handle's current form (written by the
    /// JDK's `ensureInitialized` -> `updateForm` on its first call) is an uncustomized
    /// `DIRECT_INVOKE_STATIC` form.
    pub mh_method_finder_mint_init_refile: bool,

    /// `CRATONVM_MH_METHOD_FINDER_SHAPE_MINT` — [`parse::truthy_word_default_true`] (default ON
    /// since the w108 A/B on Windows: `L4W90FinderScopeBench` rows 0.15-0.50x under `--jdk-only`,
    /// 0.66-0.98x under `--compatible`; `=0` off). Interpreter round i5 wave 108, lane L4 (page
    /// `i89-L4-method-finder-per-class-and-per-handle-cost`): a `findStatic` /
    /// `findConstructor` of a class no finder answered before is minted from the answer
    /// filed for ANOTHER class of the same shape (same `Lookup` words, name and type
    /// objects; same defining loader, package, class access flags and own `InnerClasses`
    /// flags; the member declared by each class itself with the same access flags, neither
    /// private nor protected; for a static method the same initialization state), so
    /// every Java-visible verdict of the JDK body is the filed one. Needs
    /// `CRATONVM_MH_METHOD_FINDER_MINT_MEMO`.
    pub mh_method_finder_shape_mint: bool,

    /// `CRATONVM_MH_COMPAT_FINDER_MINT` — [`parse::truthy_word_default_true`] (default ON since the
    /// w109 A/B on Windows: --compatible finder rows 0.12-0.42x; `=0` off; was default off, opt-in,
    /// performance). Interpreter round i5 wave 109, lane L4 (page
    /// `i89-L4-method-finder-per-class-and-per-handle-cost`): the `Lookup` finder mint memos
    /// (`CRATONVM_MH_FINDER_MINT_MEMO`, `CRATONVM_MH_METHOD_FINDER_MINT_MEMO`, and with the
    /// latter `CRATONVM_MH_METHOD_FINDER_SHAPE_MINT`) also register in a `--compatible` VM
    /// over a real JDK whose `JdkInvoke` alignment row is in force, where the finders run the
    /// JDK's own bodies as under `--jdk-only`. Without it `--compatible` runs every finder
    /// body (w108: `findStatic-warm-class` 43-47 us against 14 under `--jdk-only`).
    pub mh_compat_finder_mint: bool,

    /// `CRATONVM_MH_IWA_JDK_BODY` — [`parse::truthy_word`] (default off, opt-in; HotSpot frames
    /// against performance). Interpreter round i5 wave 109, lane L4 (page
    /// `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`): `MethodHandle.invokeWithArguments`
    /// on a real handle (not a variable-arity collector, not a downcall carrier) runs the JDK's
    /// own body instead of the served door, so its spread invoker's and `asType` adapter's spun
    /// `LambdaForm$MH` frames exist, as HotSpot's do, for a `SHOW_HIDDEN_FRAMES` walk.
    pub mh_iwa_jdk_body: bool,

    /// `CRATONVM_MH_FOREIGN_AS_TYPE_CACHE_READ` — [`parse::truthy_word`] (default off, opt-in,
    /// performance). Interpreter round i5 wave 114, lane L4 (page
    /// `i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call`): the `MethodHandle.asType`
    /// `Intrinsic` answers a real JDK handle's `asTypeCache` itself when that adapter's `type` is
    /// the requested `MethodType` (the JDK's `asTypeCached` first test, pure reads), instead of
    /// re-entering the JDK's `asType` bytecode, which the method's own registered native keeps
    /// interpreted for good (`native_skip`). Every other case still runs the JDK's body.
    pub mh_foreign_as_type_cache_read: bool,

    /// `CRATONVM_MH_VOID_TAIL_UNMOUNT` — [`parse::truthy_word`] (default off, opt-in; HotSpot's
    /// unmount against the pin). Interpreter round i6 wave 115, lane L4 (proposal
    /// `i26-L4-proposal-let-a-virtual-thread-unmount-under-the-method-handle-natives`, the
    /// `V -> reference` half): a `void` leaf reached in tail position through a method-handle
    /// door whose caller receives a reference (`invoke` at an `Object` site,
    /// `invokeWithArguments`, `Method.invoke` under `--jdk-only`) declares the door transparent,
    /// so a virtual thread that parks in it unmounts as on HotSpot, and the remounted frame's
    /// `void` return pushes the door's `null` (`interpreter::push_void_return_across_continuation_boundary`).
    pub mh_void_tail_unmount: bool,

    /// `CRATONVM_MH_IWA_NATIVE_STEPS` — [`parse::truthy_word`] (default off, opt-in,
    /// performance; read only under `CRATONVM_MH_IWA_JDK_BODY`). Interpreter round i6 wave 116,
    /// lane L4 (page `i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call`): the array
    /// `invokeWithArguments` body's steps (`asType(genericMethodType(n))`, the spread invoker,
    /// the JDK linker's `invokeExact`) run from the `Intrinsic` instead of re-entering the body's
    /// bytecode, which the method's own native keeps interpreted for good; the
    /// `invokeWithArguments` frame is the served door's reflective mark.
    pub mh_iwa_native_steps: bool,

    /// `CRATONVM_MH_IWA_SPREADER_MEMO` — [`parse::truthy_word`] (default off, opt-in,
    /// performance; read under `CRATONVM_MH_IWA_NATIVE_STEPS`). Interpreter round i6 wave 116,
    /// lane L4: JDK 25's `Invokers.spreadInvoker(0)` builds a fresh spreader per call
    /// (`asSpreader`); its first answer per `n` is filed in the VM's rooted bound-type table and
    /// reused, with its `customizationCount` set back to `0` before each use (invisible to Java).
    pub mh_iwa_spreader_memo: bool,

    /// `CRATONVM_MH_IWA_LINK_MEMO` — [`parse::truthy_word`] (default off, opt-in, performance;
    /// read under `CRATONVM_MH_IWA_NATIVE_STEPS` with `CRATONVM_MH_IWA_SPREADER_MEMO`).
    /// Interpreter round i6 wave 130, lane L4 (page
    /// `i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call`, "Wave 130"): the native
    /// steps' per-call spreader-row and linker-row lookups (key strings, table probes, the site
    /// descriptor's shareability parse, the `fromMethodDescriptorString` scan) are answered from
    /// one per-thread line per (VM, address epoch, caller class, arity), and the linker is
    /// called as the door's pair hit calls it.
    pub mh_iwa_link_memo: bool,

    /// `CRATONVM_MH_IWA_SERVED_CUSTOMIZATION` — [`parse::on_unless_zero`] (default ON, HotSpot
    /// parity; `=0` restores the old served call). Interpreter round i6 wave 132, lane L4 (page
    /// `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`, residual 2): the served
    /// `invokeWithArguments` of a real handle counts its `asType(genericMethodType(n))` adapter
    /// as the JDK spread invoker's `Invokers.checkCustomized` does once per call, and customizes
    /// it at `CUSTOMIZE_THRESHOLD`, so `form.customized` is set where HotSpot sets it.
    pub mh_iwa_served_customization: bool,

    /// `CRATONVM_MH_INVOKER_CHECKS_INTRINSIC` — [`parse::truthy_word`] (default off, opt-in,
    /// performance; read at registration, and the rows register only in a VM without a JIT).
    /// Interpreter round i6 wave 132, lane L4 (page
    /// `i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call`, "Wave 132"):
    /// `Invokers.checkCustomized(MethodHandle)` and `Invokers.checkExactType(MethodHandle,
    /// MethodType)`, the per-call checks every interpreted `invokeExact_MT` / spread-invoker form
    /// runs, are reviewed `Intrinsic`s; anything but the plain answer runs the JDK's own body.
    pub mh_invoker_checks_intrinsic: bool,

    /// `CRATONVM_MH_LF_LEAF_INTRINSICS` — [`parse::truthy_word`] (default off, opt-in,
    /// performance; read at registration, and the rows register only in a VM without a JIT).
    /// Interpreter round i7 wave 137, lane L4 (page
    /// `i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call`, "Wave 137"): two
    /// frameless `LambdaForm` leaf helpers, `MethodHandleImpl.checkSpreadArgument(Object, int)`
    /// (every spread form) and `DirectMethodHandle.internalMemberName(Object)` (every interpreted
    /// `DirectMethodHandle$Holder` form), are reviewed `Intrinsic`s; anything but the plain answer
    /// runs the JDK's own body.
    pub mh_lf_leaf_intrinsics: bool,

    /// `CRATONVM_MH_REAL_DIRECT_HIDDEN_LATCH` — [`parse::truthy_word`] (default off, opt-in,
    /// performance with HotSpot's frames kept). Interpreter round i6 wave 117, lane L4 (proposal
    /// `i100-L4-proposal-method-handle-warm-call-directions`, item 3): the wave-102
    /// direct-method lane before the linker (`CRATONVM_MH_REAL_DIRECT_BEFORE_LINKER`) also runs
    /// while the VM's "hidden frames never observed" latch holds (no `SHOW_HIDDEN_FRAMES` walk
    /// yet, no `-XX:+ShowHiddenFrames`, agent, debugger or JVMTI listener), and keeps the skipped
    /// linker's `checkCustomized` count, so the handle customizes when it would have.
    pub mh_real_direct_hidden_latch: bool,

    /// `CRATONVM_MH_VIRTUAL_BY_OWNER_ID` — [`parse::on_unless_zero`] (default ON since wave 134
    /// by user decision, `=0` off; performance, flat A/B, correctness SAME). Interpreter round
    /// i6 wave 118, lane L4 (the `VIRTUAL` half of
    /// `i63-L4-proposal-virtual-door-dispatches-by-owner-id`): a `VIRTUAL` method handle whose
    /// declaring class id is known (the carrier's `MH_STATIC_OWNER` under
    /// `CRATONVM_MH_INSTANCE_OWNER_ID`, a real handle's `MemberName.clazz`) calls
    /// `NativeContext::invoke_virtual_declared_by_class_id`, so the VM's §5.4.6 selection finds
    /// the declared class on the receiver's chain by id instead of comparing names per step.
    pub mh_virtual_by_owner_id: bool,

    /// `CRATONVM_MH_HIDDEN_CLASS_RUNTIME_PACKAGE` — [`parse::on_unless_zero`] (default ON,
    /// `=0` off; parity). Interpreter round i6 wave 121, lane L4
    /// (`i121-L4-jdk-string-concat-factory-hidden-class-constructor-is-refused`): the native
    /// member-access check (`lang_reflect::caller_may_access_member`, used by
    /// `MethodHandleNatives.resolve` and reflection) puts a hidden class in the runtime package
    /// of its class-file name, not of `<name>/0x<hex>` read up to the last `/`. Off: the old
    /// reading, which refused the JDK `StringConcatFactory`'s package-private hidden-class
    /// constructor.
    pub mh_hidden_class_runtime_package: bool,

    /// `CRATONVM_MHN_INIT_CONSTRUCTOR_JDK_TYPE` — [`parse::on_unless_zero`] (default ON, `=0`
    /// off; parity). Interpreter round i6 wave 124, lane L4 (`i88-L3`): `MethodHandleNatives.init`
    /// of a REAL `MemberName` from a `Constructor` leaves `type` unset, as HotSpot's
    /// `init_method_MemberName` does, so the JDK's `MemberName(Constructor)` stores
    /// `{void.class, ptypes}` and `getMethodType()` interns it. Off: the native mints a
    /// non-interned `(ptypes)void` with a private `MethodTypeForm` claiming to be its own basic
    /// and erased type (`revealDirect(unreflectConstructor(..)).getMethodType().erase()` kept
    /// reference parameters; identity against `MethodType.methodType` was false).
    pub mhn_init_constructor_jdk_type: bool,

    /// `CRATONVM_RECORD_LAYOUT_TABLE` — [`parse::truthy_word`] (default off, opt-in,
    /// performance). Interpreter round i6 wave 122, lane L4
    /// (`i39-L4-proposal-one-record-layout-table-per-class`, items 1-2): the record
    /// `hashCode`/`equals` intrinsic (`native-builtins/src/intrinsics/record.rs`) reads a
    /// class's `object_method_fast_path` answer from the VM's per-class record-layout table
    /// (the wave-117 per-VM `boolean` memo, one row per class) after the first call, instead
    /// of taking the class-manager lock per record and per nested reference component.
    pub record_layout_table: bool,

    /// `CRATONVM_RECORD_BOXED_COMPONENTS` — [`parse::truthy_word_default_true`] (default ON since interpreter round i7 wave 144, A/B w142lto; `=0` off;
    /// performance). Interpreter round i7 wave 135, lane L3
    /// (`i134-R2-record-layout-lookup-is-35x-hotspot`): the record `hashCode`/`equals`
    /// intrinsic (`native-builtins/src/intrinsics/record.rs`) hashes and compares a boxed
    /// primitive component (`Integer`, `Long`, `Short`, `Byte`, `Character`, `Boolean`,
    /// `Float`, `Double`; all final, their `hashCode`/`equals` fixed by the JDK) from its
    /// `value` field, instead of a name-based `invoke_virtual` back into Java per component.
    pub record_boxed_components: bool,

    /// `CRATONVM_RECORD_OBJECTS_FRAMES` — [`parse::on_unless_zero`] (default ON, `=0` off;
    /// correctness). Tier-1 round wave 9, lane STRREC: the record `hashCode`/`equals`
    /// intrinsic (`native-builtins/src/intrinsics/record.rs`) reaches a reference component
    /// that needs Java code through `java.util.Objects.hashCode` / `Objects.equals`, as
    /// HotSpot's `ObjectMethods` handle chain does, so the component's stack (an exception,
    /// `StackWalker`) shows the `Objects` frame and every nested record's own frame. Only
    /// components answered with no Java code at all (String, boxed, enum, and records /
    /// `ArrayList`s made only of those) stay native. `=0`: the earlier direct virtual
    /// `hashCode` / `equals` dispatch.
    pub record_objects_frames: bool,

    /// `CRATONVM_MH_REAL_AS_TYPE_MEMO` — [`parse::truthy_word_default_true`] (default ON since the
    /// interpreter round i4 wave-86 triage, `=0` off;
    /// performance). Interpreter round i3 wave 82, lane L4 (proposal
    /// `i72-L4-proposal-jdk-only-field-handle-mint-and-converting-invoke-cost`, item 4):
    /// under `--jdk-only`, `MethodHandle.asTypeUncached` on a direct or bound
    /// receiver answers a converting adapter minted from a filed one when two
    /// earlier answers for the same (receiver class, `type`, `form`, new type)
    /// in one collection epoch agreed word for word: the receiver (or its bound
    /// values) where the JDK put them, the shared conversion functions and
    /// form elsewhere. `asType` still writes the receiver's cache itself.
    pub mh_real_as_type_memo: bool,

    /// `CRATONVM_MH_REAL_CTOR_WIDENING_SERVE` — [`parse::truthy_word`] (default off, opt-in,
    /// performance). Interpreter round i4 wave 88, lane L4: `invoke()` of a REAL
    /// nullary `DirectMethodHandle$Constructor` at an `()Object` call site (the
    /// widening `asType` view the JDK's generic invoker builds) allocates the
    /// already-initialized instance class and runs its `<init>` directly, as the
    /// handle's form does, instead of the linker, `asType` and the form.
    pub mh_real_ctor_widening_serve: bool,

    /// `CRATONVM_MH_REAL_UNSPELLABLE_TYPE_ADAPTS` — [`parse::on_unless_zero`]
    /// (a correctness fix). `invoke` on a REAL JDK method handle whose `type()`
    /// this model cannot spell as a descriptor (a lambda proxy's mirror names
    /// no class) runs the JDK's `asType(callSiteType)` instead of calling the
    /// member unadapted: `findVirtual(P, "run", ..).invoke((Object) "x")` ran
    /// on the `String` under `--jdk-only` where HotSpot raises
    /// `ClassCastException` (interpreter round i2 wave 62, lane L4;
    /// `L4W62LambdaProxyOwnerCast`).
    pub mh_real_unspellable_type_adapts: bool,

    /// `CRATONVM_MH_REAL_STATIC_FIELD_INITIALIZES` — [`parse::on_unless_zero`]
    /// (a correctness fix). A REAL JDK static field handle
    /// (`DirectMethodHandle$StaticAccessor`, `--jdk-only`) initializes its
    /// field's class before the access, as HotSpot's `LF_*_CINIT` forms do:
    /// the door read `0` from an uninitialized class and lost a setter's
    /// store to the `<clinit>` that ran later (interpreter round i2 wave 62,
    /// lane L4; `L4W62InheritedStaticFieldHandles`).
    pub mh_real_static_field_initializes: bool,

    /// `CRATONVM_MH_IWA_VARARGS_DIRECT` — [`parse::on_unless_zero`]
    /// (performance). `invokeWithArguments` on a real JDK
    /// `AsVarargsCollector` over a direct method handle with a reference
    /// varargs component collects the trailing arguments and calls the member
    /// in place instead of running `asType(genericMethodType(n))`'s spun
    /// collector forms (JIT round 21, lane native; page
    /// `jr21-iwa-small-varargs-call-runs-the-collector-lambda-forms-FIXED-20261015`).
    pub mh_iwa_varargs_direct: bool,

    /// `CRATONVM_VH_SYNTHETIC_MH_TABLE` — [`parse::on_unless_zero`].
    /// `VarHandle.getMethodHandleUncached(I)` is a reviewed `Intrinsic`: for
    /// one of this VM's synthetic `VarHandle`s (no `vform`) it answers this
    /// model's invoker of the mode, which the JDK's own `getMethodHandle`
    /// caches; any other receiver runs the JDK body. With it on, wave 56's
    /// call-side screens in `mh_run_foreign_vmentry` are skipped (interpreter
    /// round i2 wave 57, lane L4; proposal
    /// `interpreter-L4-proposal-give-synthetic-varhandles-the-jdks-method-handle-table-FIXED-20261021`).
    pub vh_synthetic_mh_table: bool,

    /// `CRATONVM_VH_REAL_VARFORM` — [`parse::truthy_word`] (default off, opt-in; HotSpot
    /// parity, `--compatible` only). Interpreter round i6 wave 123, lane L4 (proposal
    /// `i57-L4-proposal-mint-varhandles-with-a-real-varform`, stages 1 and 2): a plain
    /// field `VarHandle` this VM mints (`findVarHandle`, `findStaticVarHandle`,
    /// `unreflectVarHandle`) carries a real `VarForm` whose `implClass` is an embedded
    /// `cratonvm/internal/VhForm*` class, and `isAccessModeSupported` / `toMethodHandle`
    /// run the JDK's own bodies over it.
    pub vh_real_varform: bool,

    /// `CRATONVM_MH_LEAF_DESC` — [`parse::truthy_word`] (default off, opt-in; HotSpot parity).
    /// Interpreter round i6 wave 123, lane L4 (proposal
    /// `i42-L4-proposal-native-adapters-carry-hotspots-type`): a handle of this model wide
    /// enough for `asType` to copy is judged by its own `type` alone. `MH_DESC` is then only
    /// the leaf / dispatch descriptor (the page's `MH_LEAF_DESC`), and the raw-descriptor
    /// escapes of `asType` and `explicitCastArguments` (an accept when `MH_DESC` converts
    /// though `type` does not) are not taken for such a handle. `CRATONVM_DBG_MH_DISPATCH=1`
    /// prints `[MH_LEAF_DESC]` for each refusal the escape would have turned into an accept.
    pub mh_leaf_desc: bool,

    /// `CRATONVM_MH_SITE_LINK_MEMO` — [`parse::truthy_word`] (default off, opt-in,
    /// performance). Interpreter round i6 wave 123, lane L4 (proposal
    /// `i100-L4-proposal-method-handle-warm-call-directions`, item 4): an interpreted
    /// `MethodHandle.invokeExact` / `invoke` site's link (`invoke.rs` `mh_site_link_checked`)
    /// that succeeded is remembered per thread under `(VM, caller class, door, descriptor)`,
    /// as HotSpot resolves an `invokehandle` site once, so a site the route cache never
    /// records (a megamorphic receiver) stops paying a class-manager read per name per call.
    /// A failed link is never remembered (it fails again on every execution, as on HotSpot).
    pub mh_site_link_memo: bool,

    /// `CRATONVM_VH_TO_METHOD_HANDLE_BOUND` — [`parse::on_unless_zero`] (default on, `=0` off; HotSpot parity).
    /// Interpreter round i5 wave 100, lane L4: `VarHandle.toMethodHandle(GET | SET)` of a
    /// field handle answers the mode's invoker bound to the handle, as JDK 25 does
    /// (`getMethodHandle(mode).bindTo(asDirect())`), so `Lookup.revealDirect` refuses it
    /// (`not a direct method handle`); `=0` restores the direct field accessor. Read only where
    /// the native runs: the synthetic JDK, or `--compatible` with `CRATONVM_COMPAT_ALIGN=-jdk-invoke`
    /// (a real-JDK boot's `JdkInvoke` row leaves `toMethodHandle` to the JDK's bytecode).
    pub vh_to_method_handle_bound: bool,

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

    /// `CRATONVM_NETTY_QUEUE_BRIDGE` — [`parse::exactly_one`]. Default OFF: the side-store `offer`/`poll`/`size`
    /// shadows were inert for the first loader of a Netty jar and live for every later one (a Quarkus dev-mode
    /// restart reloads Netty), where `offer` went to the side store while `poll` read the JCTools array and the
    /// acceptor-install task of the second server was lost. The JCTools bytecode is the authority.
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
    /// t1 (2026-10-26): no reader (the real lifecycle is the default; `synthetic_quarkus_start` is the live key); see `docs/internal/fixed-bugs/t1-Y-unread-flag-fields-FIXED-20261028.md`.
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

    /// `CRATONVM_STACKWALK_CALLER_CLASS_TAIL` — [`parse::truthy_word`]. JIT round 21, lane walk (WALK21-9): read here, per
    /// [`flags()`] snapshot (thread overrides included), instead of a process
    /// `OnceLock` in `native-builtins`.
    pub stackwalk_caller_class_tail: bool,

    /// `CRATONVM_STACKWALK_DEEP_WALK_LINES` — [`parse::truthy_word_default_true`] (default ON since the r22 A/B, R22-AB-FLIP). JIT round 21, lane walk (WALK21-1): read here, per
    /// [`flags()`] snapshot (thread overrides included), instead of a process
    /// `OnceLock` in `native-builtins`.
    pub stackwalk_deep_walk_lines: bool,

    /// `CRATONVM_STACKWALK_DEFERRED_LINES` — [`parse::truthy_word`]. JIT round 21, lane walk (WALK21-4): read here, per
    /// [`flags()`] snapshot (thread overrides included), instead of a process
    /// `OnceLock` in `native-builtins`.
    pub stackwalk_deferred_lines: bool,

    /// `CRATONVM_STACKWALK_OPTION_ORDINALS_ONCE` — [`parse::truthy_word_default_true`] (default ON since the r22 A/B, R22-AB-FLIP). JIT round 21, lane walk (WALK21-5): read here, per
    /// [`flags()`] snapshot (thread overrides included), instead of a process
    /// `OnceLock` in `native-builtins`.
    pub stackwalk_option_ordinals_once: bool,

    /// `CRATONVM_STACKWALK_PIPELINE_INVOKE_BY_ID` — [`parse::truthy_word`]. JIT round 21, lane walk (WALK21-6): read here, per
    /// [`flags()`] snapshot (thread overrides included), instead of a process
    /// `OnceLock` in `native-builtins`.
    pub stackwalk_pipeline_invoke_by_id: bool,

    /// `CRATONVM_STACKWALK_PIPELINE_LAYOUT_ONCE` — [`parse::truthy_word`]. JIT round 21, lane walk (WALK21-2): read here, per
    /// [`flags()`] snapshot (thread overrides included), instead of a process
    /// `OnceLock` in `native-builtins`.
    pub stackwalk_pipeline_layout_once: bool,

    /// `CRATONVM_STACKWALK_REFLECTION_SCREEN_EXACT` — [`parse::truthy_word_default_true`]. JIT round 21, lane walk (WALK21-3): read here, per
    /// [`flags()`] snapshot (thread overrides included), instead of a process
    /// `OnceLock` in `native-builtins`.
    pub stackwalk_reflection_screen_exact: bool,

    /// `CRATONVM_STACKWALK_WINDOW_CAPTURE` — [`parse::truthy_word`]. JIT round 21, lane walk (WALK21-10): read here, per
    /// [`flags()`] snapshot (thread overrides included), instead of a process
    /// `OnceLock` in `native-builtins`.
    pub stackwalk_window_capture: bool,

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
    /// t1 (2026-10-26): no reader (`real_forkjoinpool` is the live mirror); see `docs/internal/fixed-bugs/t1-Y-unread-flag-fields-FIXED-20261028.md`.
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

    /// `CRATONVM_TPE_DOORS_SELF_FIRST` — [`parse::truthy_word_default_true`]
    /// (default ON, `=0` the kill switch). Interpreter round i7 wave 137 (lane
    /// L1b): read here instead of a `runtime_flag_default_on` on every
    /// executor door whose receiver has a non-null `e` (every
    /// `Executors.newSingleThreadExecutor()` `execute` / `submit`).
    pub tpe_doors_self_first: bool,

    /// `CRATONVM_TPE_SHUTDOWN_CAS_ADVANCE` — [`parse::truthy_word_default_true`]
    /// (default ON). Wave 137 L1b: from the snapshot, as above.
    pub tpe_shutdown_cas_advance: bool,

    /// `CRATONVM_TPE_SHUTDOWN_REAL_BODY` — [`parse::truthy_word_default_true`]
    /// (default ON). Wave 137 L1b: from the snapshot, as above.
    pub tpe_shutdown_real_body: bool,

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
            dbg_lambda_generic: present_utf8(src, "CRATONVM_DBG_LAMBDA_GENERIC"),
            dbg_linker: present(src, "CRATONVM_DBG_LINKER"),
            dbg_loader_trace: present(src, "CRATONVM_DBG_LOADER_TRACE"),
            dbg_logprov: present(src, "CRATONVM_DBG_LOGPROV"),
            dbg_lookup: present_utf8(src, "CRATONVM_DBG_LOOKUP"),
            dbg_mcl: present(src, "CRATONVM_DBG_MCL"),
            dbg_method_invoke_box: present(src, "CRATONVM_DBG_METHOD_INVOKE_BOX"),
            dbg_mh_dispatch: present(src, "CRATONVM_DBG_MH_DISPATCH"),
            dbg_mh_finder_census: present_utf8(src, "CRATONVM_DBG_MH_FINDER_CENSUS"),
            dbg_zip_stream_census: present_utf8(src, "CRATONVM_DBG_ZIP_STREAM_CENSUS"),
            dbg_mh_field_link: present(src, "CRATONVM_DBG_MH_FIELD_LINK"),
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
            dbg_stamped: present(src, "CRATONVM_DBG_STAMPED"),
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
            reflect_new_array_one_resolve: on_unless_zero_or_false(
                src,
                "CRATONVM_REFLECT_NEW_ARRAY_ONE_RESOLVE",
            ),
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
            standin_spun_iwa_frames: truthy_word_default_true(src, "CRATONVM_STANDIN_SPUN_IWA_FRAMES"),
            standin_spun_linker_frames: truthy_word_default_true(src, "CRATONVM_STANDIN_SPUN_LINKER_FRAMES"),
            reflect_lambda_proxy_facts: on_unless_zero(src, "CRATONVM_REFLECT_LAMBDA_PROXY_FACTS"),
            diag_jboss_services: utf8(src, "CRATONVM_DIAG_JBOSS_SERVICES"),
            diag_jca: present(src, "CRATONVM_DIAG_JCA"),
            diag_method_invoke_null: present(src, "CRATONVM_DIAG_METHOD_INVOKE_NULL"),
            diag_properties: one_true_yes_exact(src, "CRATONVM_DIAG_PROPERTIES"),
            diag_serviceloader: one_true_yes_exact(src, "CRATONVM_DIAG_SERVICELOADER"),
            enable_assertions: present(src, "CRATONVM_ENABLE_ASSERTIONS"),
            eqe_sync_execute: present(src, "CRATONVM_EQE_SYNC_EXECUTE"),
            es_shutdown_real_pool: truthy_word_default_true(src, "CRATONVM_ES_SHUTDOWN_REAL_POOL"),
            forname_trace: present(src, "CRATONVM_FORNAME_TRACE"),
            generic_interfaces_memo: on_unless_zero(src, "CRATONVM_GENERIC_INTERFACES_MEMO"),
            generic_reflect_memo: on_unless_zero(src, "CRATONVM_GENERIC_REFLECT_MEMO"),
            generic_type_render_parity: on_unless_zero(src, "CRATONVM_GENERIC_TYPE_RENDER_PARITY"),
            http_max_body: usize_positive(src, "CRATONVM_HTTP_MAX_BODY"),
            iae_trace: present(src, "CRATONVM_IAE_TRACE"),
            iae_trace_ok: present_utf8(src, "CRATONVM_IAE_TRACE"),
            iae_trace2: present_utf8(src, "CRATONVM_IAE_TRACE2"),
            inherit_thread_ccl: on_unless_zero_or_false(src, "CRATONVM_INHERIT_THREAD_CCL"),
            inherit_tl_workaround: on_unless_zero_or_false(src, "CRATONVM_INHERIT_TL_WORKAROUND"),
            jboss_brute_force_jars: exactly_one(src, "CRATONVM_JBOSS_BRUTE_FORCE_JARS"),
            jboss_logger_base_emit: exactly_one(src, "CRATONVM_JBOSS_LOGGER_BASE_EMIT"),
            loader_unload: on_unless_zero(src, "CRATONVM_LOADER_UNLOAD"),
            max_inflated_bytes: utf8(src, "CRATONVM_MAX_INFLATED_BYTES"),
            mh_foreign_handle_raises: on_unless_zero(src, "CRATONVM_MH_FOREIGN_HANDLE_RAISES"),
            mh_foreign_handle_runs_vmentry: on_unless_zero(
                src,
                "CRATONVM_MH_FOREIGN_HANDLE_RUNS_VMENTRY",
            ),
            mh_foreign_dmh_by_member: on_unless_zero(src, "CRATONVM_MH_FOREIGN_DMH_BY_MEMBER"),
            mh_foreign_adapters_run_jdk: on_unless_zero(
                src,
                "CRATONVM_MH_FOREIGN_ADAPTERS_RUN_JDK",
            ),
            mh_foreign_varargs_run_jdk: on_unless_zero(src, "CRATONVM_MH_FOREIGN_VARARGS_RUN_JDK"),
            mh_foreign_collectors_run_jdk: exactly_one(
                src,
                "CRATONVM_MH_FOREIGN_COLLECTORS_RUN_JDK",
            ),
            mh_foreign_vmentry_first: on_unless_zero(src, "CRATONVM_MH_FOREIGN_VMENTRY_FIRST"),
            ibg_spin_on_miss: on_unless_zero(src, "CRATONVM_IBG_SPIN_ON_MISS"),
            mh_astype_refusal_site_text: on_unless_zero(
                src,
                "CRATONVM_MH_ASTYPE_REFUSAL_SITE_TEXT",
            ),
            vh_primitive_return_astype: on_unless_zero(src, "CRATONVM_VH_PRIMITIVE_RETURN_ASTYPE"),
            ffm_segment_vh_lean_heap: on_unless_zero(src, "CRATONVM_FFM_SEGMENT_VH_LEAN_HEAP"),
            ffm_segment_vh_jdk_raise: on_unless_zero(src, "CRATONVM_FFM_SEGMENT_VH_JDK_RAISE"),
            ffm_segment_vh_jdk_raise_all_modes: on_unless_zero(
                src,
                "CRATONVM_FFM_SEGMENT_VH_JDK_RAISE_ALL_MODES",
            ),
            ffm_real_memo_first: on_unless_zero(src, "CRATONVM_FFM_REAL_MEMO_FIRST"),
            ffm_arena_jit_resolve: on_unless_zero(src, "CRATONVM_FFM_ARENA_JIT_RESOLVE"),
            mh_invoke_return_unbox: on_unless_zero(src, "CRATONVM_MH_INVOKE_RETURN_UNBOX"),
            mh_guard_type_is_targets: on_unless_zero(src, "CRATONVM_MH_GUARD_TYPE_IS_TARGETS"),
            mh_adapter_stamp_converts: on_unless_zero(src, "CRATONVM_MH_ADAPTER_STAMP_CONVERTS"),
            mh_return_plan: on_unless_zero(src, "CRATONVM_MH_RETURN_PLAN"),
            mh_void_declared_discards: on_unless_zero(src, "CRATONVM_MH_VOID_DECLARED_DISCARDS"),
            mh_dynamic_invoker_site_type: on_unless_zero(
                src,
                "CRATONVM_MH_DYNAMIC_INVOKER_SITE_TYPE",
            ),
            mh_compatible_record_owner: on_unless_zero(src, "CRATONVM_MH_COMPATIBLE_RECORD_OWNER"),
            mh_lambda_proxy_members: on_unless_zero(src, "CRATONVM_MH_LAMBDA_PROXY_MEMBERS"),
            mh_field_link_compatible: on_unless_zero(src, "CRATONVM_MH_FIELD_LINK_COMPATIBLE"),
            mh_instance_owner_id: on_unless_zero(src, "CRATONVM_MH_INSTANCE_OWNER_ID"),
            mh_owner_id_authority: on_unless_zero(src, "CRATONVM_MH_OWNER_ID_AUTHORITY"),
            mh_field_slot_from_mint: truthy_word(src, "CRATONVM_MH_FIELD_SLOT_FROM_MINT"),
            mh_special_by_owner_id: on_unless_zero(src, "CRATONVM_MH_SPECIAL_BY_OWNER_ID"),
            mh_ctor_registers_finalizable: on_unless_zero(
                src,
                "CRATONVM_MH_CTOR_REGISTERS_FINALIZABLE",
            ),
            mh_linker_hit_skips_root_refresh: on_unless_zero(
                src,
                "CRATONVM_MH_LINKER_HIT_SKIPS_ROOT_REFRESH",
            ),
            jit_mh_compiled_link: on_unless_zero(src, "CRATONVM_JIT_MH_COMPILED_LINK"),
            jit_mh_compiled_link_special: truthy_word_default_true(src, "CRATONVM_JIT_MH_COMPILED_LINK_SPECIAL"),
            jit_mh_compiled_link_by_id: truthy_word(src, "CRATONVM_JIT_MH_COMPILED_LINK_BY_ID"),
            jit_mh_compiled_link_linker_by_id: truthy_word(
                src,
                "CRATONVM_JIT_MH_COMPILED_LINK_LINKER_BY_ID",
            ),
            jit_mh_compiled_link_user_site: truthy_word(src, "CRATONVM_JIT_MH_COMPILED_LINK_USER_SITE"),
            jit_reflect_accessor_link: truthy_word(src, "CRATONVM_JIT_REFLECT_ACCESSOR_LINK"),
            reflect_sig_poly_uoe: on_unless_zero(src, "CRATONVM_REFLECT_SIG_POLY_UOE"),
            reflect_invoke0_vm_texts: on_unless_zero(src, "CRATONVM_REFLECT_INVOKE0_VM_TEXTS"),
            reflect_door_native_accessor_texts: on_unless_zero(src, "CRATONVM_REFLECT_DOOR_NATIVE_ACCESSOR_TEXTS"),
            reflect_native_accessor_arg_texts: on_unless_zero(src, "CRATONVM_REFLECT_NATIVE_ACCESSOR_ARG_TEXTS"),
            reflect_door_native_accessor_only: on_unless_zero(src, "CRATONVM_REFLECT_DOOR_NATIVE_ACCESSOR_ONLY"),
            jit_mh_compiled_link_hidden: truthy_word(src, "CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN"),
            // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md),
            // after the r21 review's #13 concurrent-mark row matched HotSpot on G1, Generational and ZGC; `=0` is the kill switch.
            class_get_name_mirror_memo: truthy_word_default_true(src, "CRATONVM_CLASS_GET_NAME_MIRROR_MEMO"),
            jit_reflect_ctor_link: truthy_word(src, "CRATONVM_JIT_REFLECT_CTOR_LINK"),

            jit_mh_compiled_link_pair_first: on_unless_zero(
                src,
                "CRATONVM_JIT_MH_COMPILED_LINK_PAIR_FIRST",
            ),
            jit_mh_compiled_link_lean: on_unless_zero(src, "CRATONVM_JIT_MH_COMPILED_LINK_LEAN"),
            jit_mh_compiled_link_agree_memo: truthy_word(
                src,
                "CRATONVM_JIT_MH_COMPILED_LINK_AGREE_MEMO",
            ),
            jit_mh_compiled_link_pair_memo: truthy_word(
                src,
                "CRATONVM_JIT_MH_COMPILED_LINK_PAIR_MEMO",
            ),
            forname_builtin_loaded_first: on_unless_zero(
                src,
                "CRATONVM_FORNAME_BUILTIN_LOADED_FIRST",
            ),
            mh_bound_field_owner: on_unless_zero(src, "CRATONVM_MH_BOUND_FIELD_OWNER"),
            mh_field_link_owner_admits: on_unless_zero(src, "CRATONVM_MH_FIELD_LINK_OWNER_ADMITS"),
            mh_field_link_bound: on_unless_zero(src, "CRATONVM_MH_FIELD_LINK_BOUND"),
            mh_bindto_fast_mint_fields: on_unless_zero(src, "CRATONVM_MH_BINDTO_FAST_MINT_FIELDS"),
            mh_bindto_prelink_field: truthy_word(src, "CRATONVM_MH_BINDTO_PRELINK_FIELD"),
            mh_field_link_serves_invoke: on_unless_zero(
                src,
                "CRATONVM_MH_FIELD_LINK_SERVES_INVOKE",
            ),
            mh_field_link_site_by_arc: truthy_word(src, "CRATONVM_MH_FIELD_LINK_SITE_BY_ARC"),
            mh_field_link_first: truthy_word(src, "CRATONVM_MH_FIELD_LINK_FIRST"),
            mh_linker_basic_member: on_unless_zero(src, "CRATONVM_MH_LINKER_BASIC_MEMBER"),
            mh_real_field_before_linker: on_unless_zero(src, "CRATONVM_MH_REAL_FIELD_BEFORE_LINKER"),
            mh_real_field_site_memo: truthy_word_default_true(src, "CRATONVM_MH_REAL_FIELD_SITE_MEMO"),
            mh_armed_caller: truthy_word_default_true(src, "CRATONVM_MH_ARMED_CALLER"),
            mh_real_field_raw_return: truthy_word_default_true(src, "CRATONVM_MH_REAL_FIELD_RAW_RETURN"),
            mh_real_bound_field_before_linker: truthy_word_default_true(
                src,
                "CRATONVM_MH_REAL_BOUND_FIELD_BEFORE_LINKER",
            ),
            // WARNING: i8-default-flip (page i147-orch): default on, `=0` off.
            mh_interp_exact_bound_species_per_handle: truthy_word_default_true(
                src,
                "CRATONVM_MH_INTERP_EXACT_BOUND_SPECIES_PER_HANDLE",
            ),
            mh_real_direct_before_linker: truthy_word_default_true(src, "CRATONVM_MH_REAL_DIRECT_BEFORE_LINKER"),
            mhn_caller_sensitive_flag: on_unless_zero(src, "CRATONVM_MHN_CALLER_SENSITIVE_FLAG"),
            mhn_resolve_named_methods: truthy_word_default_true(src, "CRATONVM_MHN_RESOLVE_NAMED_METHODS"),
            mhn_resolve_named_fields: truthy_word_default_true(src, "CRATONVM_MHN_RESOLVE_NAMED_FIELDS"),
            // Default ON since the interpreter round i5 w104 triage (JIT 0.71-0.89x,
            // --nojit 0.65-1.00x); `=0` off.
            mhn_resolve_named_superinterfaces: truthy_word_default_true(
                src,
                "CRATONVM_MHN_RESOLVE_NAMED_SUPERINTERFACES",
            ),
            mh_invoke_links_field: on_unless_zero(src, "CRATONVM_MH_INVOKE_LINKS_FIELD"),
            mh_bindto_fast_mint_prelink: truthy_word(src, "CRATONVM_MH_BINDTO_FAST_MINT_PRELINK"),
            mh_linking_call_served: on_unless_zero(src, "CRATONVM_MH_LINKING_CALL_SERVED"),
            mh_bindto_fast_mint_two_classes: on_unless_zero(
                src,
                "CRATONVM_MH_BINDTO_FAST_MINT_TWO_CLASSES",
            ),
            mh_invoke_link_declared_once: truthy_word(src, "CRATONVM_MH_INVOKE_LINK_DECLARED_ONCE"),
            mh_field_link_derivation_memo: on_unless_zero(
                src,
                "CRATONVM_MH_FIELD_LINK_DERIVATION_MEMO",
            ),
            mh_finder_shared_member_words: on_unless_zero(
                src,
                "CRATONVM_MH_FINDER_SHARED_MEMBER_WORDS",
            ),
            mh_linking_call_direct_serve: on_unless_zero(
                src,
                "CRATONVM_MH_LINKING_CALL_DIRECT_SERVE",
            ),
            mh_site_type_table: truthy_word_default_true(src, "CRATONVM_MH_SITE_TYPE_TABLE"),
            mh_site_type_boot_loader: truthy_word_default_true(
                src,
                "CRATONVM_MH_SITE_TYPE_BOOT_LOADER",
            ),
            mhn_resolve_memo: truthy_word(src, "CRATONVM_MHN_RESOLVE_MEMO"),
            mhn_resolve_method_memo: truthy_word_default_true(
                src,
                "CRATONVM_MHN_RESOLVE_METHOD_MEMO",
            ),
            mhn_resolve_direct_call: truthy_word_default_true(src, "CRATONVM_MHN_RESOLVE_DIRECT_CALL"),
            mh_compat_finder_nest_access: truthy_word_default_true(
                src,
                "CRATONVM_MH_COMPAT_FINDER_NEST_ACCESS",
            ),
            mh_compat_finder_lookup_class_access: truthy_word_default_true(
                src,
                "CRATONVM_MH_COMPAT_FINDER_LOOKUP_CLASS_ACCESS",
            ),
            mh_unnameable_mirror_own_name: truthy_word_default_true(
                src,
                "CRATONVM_MH_UNNAMEABLE_MIRROR_OWN_NAME",
            ),
            mh_real_field_record: truthy_word_default_true(src, "CRATONVM_MH_REAL_FIELD_RECORD"),
            mh_real_constructor_lane: truthy_word_default_true(src, "CRATONVM_MH_REAL_CONSTRUCTOR_LANE"),
            // WARNING: i9-default-flip (page i147-orch): default on, =0 off.
            mh_real_ctor_cast_adapter_lane: truthy_word_default_true(
                src,
                "CRATONVM_MH_REAL_CTOR_CAST_ADAPTER_LANE",
            ),
            // t1 wave 2, lane M: opt-in A/B lever, default off.
            mh_real_ctor_cast_adapter_lane_under_jit: truthy_word(
                src,
                "CRATONVM_MH_REAL_CTOR_CAST_ADAPTER_LANE_UNDER_JIT",
            ),
            mh_real_constructor_lane_memo: truthy_word(
                src,
                "CRATONVM_MH_REAL_CONSTRUCTOR_LANE_MEMO",
            ),
            mh_lanes_keep_customization: truthy_word_default_true(
                src,
                "CRATONVM_MH_LANES_KEEP_CUSTOMIZATION",
            ),
            mh_real_field_converting_serve: truthy_word_default_true(
                src,
                "CRATONVM_MH_REAL_FIELD_CONVERTING_SERVE",
            ),
            mh_method_type_intern_memo: truthy_word_default_true(
                src,
                "CRATONVM_MH_METHOD_TYPE_INTERN_MEMO",
            ),
            object_clone_class_facts: truthy_word_default_true(
                src,
                "CRATONVM_OBJECT_CLONE_CLASS_FACTS",
            ),
            mh_compat_method_type_bytecode: truthy_word_default_true(
                src,
                "CRATONVM_MH_COMPAT_METHOD_TYPE_BYTECODE",
            ),
            mh_finder_mint_memo: truthy_word_default_true(src, "CRATONVM_MH_FINDER_MINT_MEMO"),
            mh_method_finder_mint_memo: truthy_word_default_true(
                src,
                "CRATONVM_MH_METHOD_FINDER_MINT_MEMO",
            ),
            mh_method_finder_mint_init_refile: truthy_word_default_true(
                src,
                "CRATONVM_MH_METHOD_FINDER_MINT_INIT_REFILE",
            ),
            mh_method_finder_shape_mint: truthy_word_default_true(src, "CRATONVM_MH_METHOD_FINDER_SHAPE_MINT"),
            mh_compat_finder_mint: truthy_word_default_true(src, "CRATONVM_MH_COMPAT_FINDER_MINT"),
            mh_iwa_jdk_body: truthy_word(src, "CRATONVM_MH_IWA_JDK_BODY"),
            mh_foreign_as_type_cache_read: truthy_word(
                src,
                "CRATONVM_MH_FOREIGN_AS_TYPE_CACHE_READ",
            ),
            mh_void_tail_unmount: truthy_word(src, "CRATONVM_MH_VOID_TAIL_UNMOUNT"),
            mh_iwa_native_steps: truthy_word(src, "CRATONVM_MH_IWA_NATIVE_STEPS"),
            mh_iwa_spreader_memo: truthy_word(src, "CRATONVM_MH_IWA_SPREADER_MEMO"),
            mh_iwa_link_memo: truthy_word(src, "CRATONVM_MH_IWA_LINK_MEMO"),
            mh_iwa_served_customization: on_unless_zero(
                src,
                "CRATONVM_MH_IWA_SERVED_CUSTOMIZATION",
            ),
            mh_invoker_checks_intrinsic: truthy_word(src, "CRATONVM_MH_INVOKER_CHECKS_INTRINSIC"),
            mh_lf_leaf_intrinsics: truthy_word(src, "CRATONVM_MH_LF_LEAF_INTRINSICS"),
            mh_real_direct_hidden_latch: truthy_word(src, "CRATONVM_MH_REAL_DIRECT_HIDDEN_LATCH"),
            // WARNING(i6-user-flip): CRATONVM_MH_VIRTUAL_BY_OWNER_ID default ON by user decision 2026-10-19; =0 restores the old path
            mh_virtual_by_owner_id: on_unless_zero(src, "CRATONVM_MH_VIRTUAL_BY_OWNER_ID"),
            mh_hidden_class_runtime_package: on_unless_zero(
                src,
                "CRATONVM_MH_HIDDEN_CLASS_RUNTIME_PACKAGE",
            ),
            mhn_init_constructor_jdk_type: on_unless_zero(
                src,
                "CRATONVM_MHN_INIT_CONSTRUCTOR_JDK_TYPE",
            ),
            record_layout_table: truthy_word(src, "CRATONVM_RECORD_LAYOUT_TABLE"),
            record_boxed_components: truthy_word_default_true(src, "CRATONVM_RECORD_BOXED_COMPONENTS"),
            record_objects_frames: on_unless_zero(src, "CRATONVM_RECORD_OBJECTS_FRAMES"),
            mh_real_as_type_memo: truthy_word_default_true(src, "CRATONVM_MH_REAL_AS_TYPE_MEMO"),
            mh_real_ctor_widening_serve: truthy_word_default_true(src, "CRATONVM_MH_REAL_CTOR_WIDENING_SERVE"),
            mh_real_unspellable_type_adapts: on_unless_zero(
                src,
                "CRATONVM_MH_REAL_UNSPELLABLE_TYPE_ADAPTS",
            ),
            mh_real_static_field_initializes: on_unless_zero(
                src,
                "CRATONVM_MH_REAL_STATIC_FIELD_INITIALIZES",
            ),
            mh_iwa_varargs_direct: on_unless_zero(src, "CRATONVM_MH_IWA_VARARGS_DIRECT"),
            vh_synthetic_mh_table: on_unless_zero(src, "CRATONVM_VH_SYNTHETIC_MH_TABLE"),
            vh_real_varform: truthy_word(src, "CRATONVM_VH_REAL_VARFORM"),
            mh_leaf_desc: truthy_word(src, "CRATONVM_MH_LEAF_DESC"),
            mh_site_link_memo: truthy_word(src, "CRATONVM_MH_SITE_LINK_MEMO"),
            vh_to_method_handle_bound: on_unless_zero(src, "CRATONVM_VH_TO_METHOD_HANDLE_BOUND"),
            msc_real_start: on_unless_off_word_cased(src, "CRATONVM_MSC_REAL_START"),
            native_ec_multiply: present(src, "CRATONVM_NATIVE_EC_MULTIPLY"),
            // JIT r23 wave 2c, lane L5 (`CRATONVM_REGEX_MATCHER_BYTECODE`,
            // default ON, `=0` restores the natives): the real-layout
            // `Matcher.find` / `start` / `end` / `group` natives yield to the
            // JDK bytecode, which the JIT compiles and splices as HotSpot does
            // (`R23ArraysEqRegex` regex rows 2-20x faster, every answer
            // identical; `docs/known-issues/jit/r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`).
            native_matcher_find: on_unless_zero_or_false(src, "CRATONVM_NATIVE_MATCHER_FIND")
                && !on_unless_zero(src, "CRATONVM_REGEX_MATCHER_BYTECODE"),
            native_pbe_keyfactory: exactly_one(src, "CRATONVM_NATIVE_PBE_KEYFACTORY"),
            native_string_regex: on_unless_zero_or_false(src, "CRATONVM_NATIVE_STRING_REGEX"),
            netty_queue_bridge: exactly_one(src, "CRATONVM_NETTY_QUEUE_BRIDGE"),
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
            stackwalk_caller_class_tail: truthy_word(src, "CRATONVM_STACKWALK_CALLER_CLASS_TAIL"),
            // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
            stackwalk_deep_walk_lines: truthy_word_default_true(src, "CRATONVM_STACKWALK_DEEP_WALK_LINES"),
            stackwalk_deferred_lines: truthy_word(src, "CRATONVM_STACKWALK_DEFERRED_LINES"),
            // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
            stackwalk_option_ordinals_once: truthy_word_default_true(src, "CRATONVM_STACKWALK_OPTION_ORDINALS_ONCE"),
            stackwalk_pipeline_invoke_by_id: truthy_word(src, "CRATONVM_STACKWALK_PIPELINE_INVOKE_BY_ID"),
            stackwalk_pipeline_layout_once: truthy_word(src, "CRATONVM_STACKWALK_PIPELINE_LAYOUT_ONCE"),
            stackwalk_reflection_screen_exact: truthy_word_default_true(src, "CRATONVM_STACKWALK_REFLECTION_SCREEN_EXACT"),
            stackwalk_window_capture: truthy_word(src, "CRATONVM_STACKWALK_WINDOW_CAPTURE"),
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
            tpe_doors_self_first: truthy_word_default_true(src, "CRATONVM_TPE_DOORS_SELF_FIRST"),
            tpe_shutdown_cas_advance: truthy_word_default_true(
                src,
                "CRATONVM_TPE_SHUTDOWN_CAS_ADVANCE",
            ),
            tpe_shutdown_real_body: truthy_word_default_true(src, "CRATONVM_TPE_SHUTDOWN_REAL_BODY"),
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

/// The value rule behind [`parse::truthy_word`] and [`runtime_flag_on`]: off
/// iff the value, trimmed and ASCII-lowercased, is empty, `0`, `false`, `off`
/// or `no`. Everything else — `1`, `true`, `yes`, `anything` — is on.
#[inline]
fn word_is_on(value: &str) -> bool {
    let v = value.trim().to_ascii_lowercase();
    !(v.is_empty() || v == "0" || v == "false" || v == "off" || v == "no")
}

/// A boolean switch read through [`runtime_var_os`]: `false` when the variable
/// is unset, otherwise the [`parse::truthy_word`] rule applied to its value
/// (lossily decoded, so a non-UTF-8 value is on rather than silently off).
///
/// This is what a presence-only `runtime_var_os(NAME).is_some()` gate should
/// have been. Under presence semantics `NAME=0`, `NAME=false`, `NAME=off` and
/// `NAME=` all turned the switch ON — see
/// `jit-presence-only-flag-reads-FIXED.md`. It keeps every property of
/// [`runtime_var_os`]: the latched snapshot for declared names, per-thread
/// test overrides for undeclared ones, and the read census.
#[inline]
pub fn runtime_flag_on<K: AsRef<OsStr>>(key: K) -> bool {
    match runtime_var_os(key) {
        Some(value) => word_is_on(&value.to_string_lossy()),
        None => false,
    }
}

/// The value rule for a **default-ON opt-out** switch: off iff the value,
/// trimmed and ASCII-lowercased, is `0`, `false`, `off` or `no`.
///
/// Deliberately NOT [`word_is_on`]: an EMPTY value is **on** here and off
/// there. That is the majority idiom among the default-on gates already in the
/// tree (`!matches!(runtime_var(..).as_deref(), Ok("0") | Ok("false") |
/// Ok("off") | Ok("no"))`, ~20 sites in `jit/src/ir.rs` and
/// `jit/src/ir_lower.rs`), and matching it is what makes this helper a
/// *replacement* for those hand-rolled matches rather than a sixth parser with
/// its own truth table. See the "Truth tables" note in this module's header.
#[inline]
pub fn value_is_off(value: &str) -> bool {
    let v = value.trim().to_ascii_lowercase();
    v == "0" || v == "false" || v == "off" || v == "no"
}

/// A **default-ON** boolean switch — the opt-out shape — read through
/// [`runtime_var_os`]: `true` when the variable is unset, otherwise `false`
/// iff the value is one of `0`, `false`, `off`, `no` (trimmed,
/// case-insensitive).
///
/// # Why this exists
///
/// The JIT's kill switches are the levers an operator reaches for first when
/// compiled code is suspected, and two of them —`CRATONVM_JIT_LICM` and
/// `CRATONVM_JIT_UNROLL` — were spelled `map_or(true, |v| v != "0")`. Under
/// that parser `CRATONVM_JIT_LICM=false` and `CRATONVM_JIT_UNROLL=off` leave
/// the pass **on**, silently. A kill switch that cannot be turned off by the
/// spelling a reader would try is worse than no kill switch, because a bisect
/// then reports "no effect" for a pass that never stopped running — the same
/// class of defect `jit_force_interpret` documents for
/// `CRATONVM_JIT_DENY`/`CRATONVM_JIT_BISECT_ONLY`.
///
/// This is the counterpart of [`runtime_flag_on`] for that shape. Adopting it
/// can only ever turn a switch OFF where the hand-rolled `!= "0"` left it on;
/// no value that is off today becomes on.
#[inline]
pub fn runtime_flag_default_on<K: AsRef<OsStr>>(key: K) -> bool {
    match runtime_var_os(key) {
        Some(value) => !value_is_off(&value.to_string_lossy()),
        None => true,
    }
}

/// gcj j3/x2 (proposal `gcj-j2x1-proposal-collector-scoped-default-flips-20261019`,
/// part A): a switch that is default ON for ONE collector and opt-in for the
/// others. `scoped` says whether the caller's collector is the one the flip
/// was decided for (e.g. `heap.is_generational()`): then
/// [`runtime_flag_default_on`] (unset ON, `0/false/off/no` OFF), otherwise
/// [`runtime_flag_on`] (unset OFF, an explicit `=1` honoured as the opt-in
/// was). One line per collector-scoped flip instead of a hand-written
/// `if .. else` at every reader -- the shape whose absence let two wave j2
/// flips reach ZGC and G1. The `flag_default_matches_reader` guard
/// (`types/tests/`) counts this reader as default-ON.
#[inline]
pub fn runtime_flag_default_on_if<K: AsRef<OsStr>>(key: K, scoped: bool) -> bool {
    if scoped {
        runtime_flag_default_on(key)
    } else {
        runtime_flag_on(key)
    }
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

    /// `runtime_flag_on` is a boolean, not a presence test: the off words are
    /// off, and it agrees with `parse::truthy_word` on every one of them.
    #[test]
    fn runtime_flag_on_reads_the_off_words_as_off() {
        let _lock = process_override_lock();
        // Undeclared, so the per-thread override path serves it without any
        // write to `environ`.
        const NAME: &str = "cratonvm_undeclared_runtime_flag_on_probe";
        assert!(
            std::env::var_os(NAME).is_none(),
            "fixture name must not exist in the real environment"
        );
        assert!(!runtime_flag_on(NAME), "unset must read as off");
        with_thread_overrides(&[(NAME, None)], || {
            assert!(
                !runtime_flag_on(NAME),
                "an override to None must read as off"
            );
        });

        for (value, want) in [
            ("", false),
            ("0", false),
            ("false", false),
            ("OFF", false),
            (" no ", false),
            ("1", true),
            ("true", true),
            ("yes", true),
            ("anything", true),
        ] {
            with_thread_overrides(&[(NAME, Some(value))], || {
                assert_eq!(
                    runtime_flag_on(NAME),
                    want,
                    "runtime_flag_on with {NAME}={value:?}"
                );
            });
            assert_eq!(
                parse::truthy_word(&src(&[(NAME, value)]), NAME),
                want,
                "parse::truthy_word with {NAME}={value:?} must agree"
            );
        }
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
        let _lock = process_override_lock();
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

    /// gen r4w2/obs: the `[gcpause]` threshold defaults to the historical
    /// 100 ms (so the default output is byte-identical), accepts `0`, and a
    /// junk value falls back to the default rather than to "print everything".
    #[test]
    fn gcpause_min_us_defaults_to_100ms_and_accepts_zero() {
        let d = VmFlags::from_source(&MapSource::empty());
        assert_eq!(d.gc.dbg_gcpause_min_us, 100_000);
        let zero = VmFlags::from_source(&src(&[("CRATONVM_DBG_GCPAUSE_MIN_US", "0")]));
        assert_eq!(zero.gc.dbg_gcpause_min_us, 0);
        let some = VmFlags::from_source(&src(&[("CRATONVM_DBG_GCPAUSE_MIN_US", " 2500 ")]));
        assert_eq!(some.gc.dbg_gcpause_min_us, 2_500);
        let junk = VmFlags::from_source(&src(&[("CRATONVM_DBG_GCPAUSE_MIN_US", "fast")]));
        assert_eq!(junk.gc.dbg_gcpause_min_us, 100_000);
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
    /// [`runtime_flag_default_on`] honours every spelling of "off", and the
    /// literal-`0` parser it replaces did not.
    ///
    /// The two JIT kill switches this helper was added for
    /// (`CRATONVM_JIT_LICM`, `CRATONVM_JIT_UNROLL`) were
    /// `map_or(true, |v| v != "0")`, so `=false` and `=off` left the pass
    /// running. An operator bisecting a suspected miscompile with the spelling
    /// they would naturally try got "no effect" from a switch that never
    /// stopped the pass.
    #[test]
    fn runtime_flag_default_on_accepts_every_off_spelling() {
        const NAME: &str = "CVMSELFTEST_DEFAULT_ON_PROBE";

        with_thread_overrides(&[(NAME, None)], || {
            assert!(
                runtime_flag_default_on(NAME),
                "unset means ON — that is what 'default on' is"
            );
        });

        for off in ["0", "false", "off", "no", "FALSE", "Off", "NO", "  false  "] {
            with_thread_overrides(&[(NAME, Some(off))], || {
                assert!(
                    !runtime_flag_default_on(NAME),
                    "{off:?} must turn a default-ON switch off; the parser this \
                     replaced honoured only the literal \"0\""
                );
            });
        }

        for on in ["1", "true", "yes", "on", "anything", ""] {
            with_thread_overrides(&[(NAME, Some(on))], || {
                assert!(
                    runtime_flag_default_on(NAME),
                    "{on:?} must leave a default-ON switch on"
                );
            });
        }
    }

    /// gcj j3/x2: [`runtime_flag_default_on_if`] is the default-ON rule when
    /// scoped and the opt-in rule otherwise, for every spelling -- unset,
    /// off words, on words and the empty value (ON scoped, OFF unscoped).
    #[test]
    fn gcj3x2_runtime_flag_default_on_if_is_default_on_only_when_scoped() {
        const NAME: &str = "CVMSELFTEST_DEFAULT_ON_IF_PROBE";
        with_thread_overrides(&[(NAME, None)], || {
            assert!(runtime_flag_default_on_if(NAME, true), "unset, scoped: ON");
            assert!(!runtime_flag_default_on_if(NAME, false), "unset, unscoped: OFF");
        });
        for value in ["0", "false", "off", "no", "1", "true", "yes", "", "banana"] {
            with_thread_overrides(&[(NAME, Some(value))], || {
                assert_eq!(
                    runtime_flag_default_on_if(NAME, true),
                    runtime_flag_default_on(NAME),
                    "{value:?}: scoped"
                );
                assert_eq!(
                    runtime_flag_default_on_if(NAME, false),
                    runtime_flag_on(NAME),
                    "{value:?}: unscoped"
                );
            });
        }
    }

    /// The new helper can only ever turn a switch OFF where the parser it
    /// replaces left it on — never the reverse.
    ///
    /// This is the migration-safety property: adopting
    /// [`runtime_flag_default_on`] at a site spelled `map_or(true, |v| v != "0")`
    /// cannot enable something that was disabled.
    #[test]
    fn runtime_flag_default_on_only_widens_the_off_set() {
        const NAME: &str = "CVMSELFTEST_DEFAULT_ON_PROBE2";
        for value in [
            "0",
            "false",
            "off",
            "no",
            "FALSE",
            "Off",
            "NO",
            "  false  ",
            "1",
            "true",
            "yes",
            "on",
            "",
            "banana",
            "00",
        ] {
            with_thread_overrides(&[(NAME, Some(value))], || {
                let legacy = value != "0";
                let now = runtime_flag_default_on(NAME);
                assert!(
                    !(now && !legacy),
                    "{value:?}: the new parser says ON where `!= \"0\"` said OFF — \
                     that is the one direction this migration must never take"
                );
            });
        }
    }

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

    /// gen r4w2/oldgen2: the old-gen trigger hysteresis is a POLICY change, so
    /// it is opt-in — unset and `=0` both leave the historical trigger.
    #[test]
    fn old_trigger_hysteresis_is_opt_in_and_zero_is_off() {
        let on = |v: &str| {
            VmFlags::from_source(&src(&[("CRATONVM_GC_OLD_TRIGGER_HYSTERESIS", v)]))
                .gc
                .old_trigger_hysteresis
        };
        assert!(!VmFlags::from_source(&src(&[])).gc.old_trigger_hysteresis);
        assert!(!on("0"));
        assert!(!on(""));
        assert!(on("1"));
    }

    /// gen r4w4/concmark4: the hysteresis flag is read twice — the legacy
    /// explicitly-on bool (unchanged), and a tri-state the concurrent-first
    /// policy defaults. `=0` must be an explicit OFF, not "unset".
    #[test]
    fn old_trigger_hysteresis_explicit_is_a_tri_state() {
        let explicit = |v: &str| {
            VmFlags::from_source(&src(&[("CRATONVM_GC_OLD_TRIGGER_HYSTERESIS", v)]))
                .gc
                .old_trigger_hysteresis_explicit
        };
        assert_eq!(
            VmFlags::from_source(&src(&[]))
                .gc
                .old_trigger_hysteresis_explicit,
            None
        );
        assert_eq!(explicit("0"), Some(false));
        assert_eq!(explicit(""), Some(false));
        assert_eq!(explicit("1"), Some(true));
    }

    /// gen r4w4/concmark4: concurrent-first is the default and its switch is an
    /// opt-out; the fixed start percentage is off unless a positive number is
    /// given; the two concurrent-marker switches are opt-outs.
    #[test]
    fn the_concurrent_first_old_gen_policy_switches_parse_as_documented() {
        let f = |pairs: &[(&str, &str)]| VmFlags::from_source(&src(pairs));
        let d = f(&[]);
        assert!(!d.gc.old_no_concurrent_first);
        assert_eq!(d.gc.conc_start_percent, None);
        assert!(!d.gc.gen_conc_mark_no_local_stack);
        assert!(!d.gc.gen_conc_no_frozen_eager_scan);
        assert!(
            f(&[("CRATONVM_GC_NO_CONCURRENT_FIRST", "1")])
                .gc
                .old_no_concurrent_first
        );
        assert!(
            !f(&[("CRATONVM_GC_NO_CONCURRENT_FIRST", "0")])
                .gc
                .old_no_concurrent_first
        );
        assert_eq!(
            f(&[("CRATONVM_GC_CONC_START_PERCENT", "45")])
                .gc
                .conc_start_percent,
            Some(45)
        );
        assert_eq!(
            f(&[("CRATONVM_GC_CONC_START_PERCENT", "0")])
                .gc
                .conc_start_percent,
            None
        );
        assert_eq!(
            f(&[("CRATONVM_GC_CONC_START_PERCENT", "x")])
                .gc
                .conc_start_percent,
            None
        );
        assert!(
            f(&[("CRATONVM_GEN_CONC_MARK_NO_LOCAL_STACK", "1")])
                .gc
                .gen_conc_mark_no_local_stack
        );
        assert!(
            f(&[("CRATONVM_GEN_CONC_NO_FROZEN_EAGER_SCAN", "1")])
                .gc
                .gen_conc_no_frozen_eager_scan
        );
    }

    /// gen r4w5/concmark5: the concurrent-cycle service thread and the remark
    /// reference-processing hook are both opt-in, and `=0` is off.
    #[test]
    fn the_concurrent_service_and_remark_hook_switches_are_opt_in() {
        let f = |pairs: &[(&str, &str)]| VmFlags::from_source(&src(pairs));
        let d = f(&[]);
        assert!(!d.gc.gen_conc_service_thread);
        // gce ve2: default on since 2026-09-29 (`=0` kills).
        assert!(d.gc.gen_conc_remark_refproc_hook);
        assert!(
            f(&[("CRATONVM_GEN_CONC_SERVICE_THREAD", "1")])
                .gc
                .gen_conc_service_thread
        );
        assert!(
            !f(&[("CRATONVM_GEN_CONC_SERVICE_THREAD", "0")])
                .gc
                .gen_conc_service_thread
        );
        assert!(
            f(&[("CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK", "1")])
                .gc
                .gen_conc_remark_refproc_hook
        );
        assert!(
            !f(&[("CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK", "0")])
                .gc
                .gen_conc_remark_refproc_hook
        );
    }

    /// gen r5w1/refs5: the SATB young-value filter and the HotSpot soft-ref
    /// LRU inputs are policy/perf changes, so both are opt-in and `=0` is off.
    ///
    /// gen r5w2/conc6: the allocation-failure door's concurrent start
    /// (`CRATONVM_GEN_CONC_ALLOC_FAIL_DOOR`) was asserted opt-in here too. Its
    /// default is now ON — without it the default concurrent-first old-gen
    /// policy never ran a cycle in compiled code
    /// (`gengc-r5w1-refs5-concurrent-cycle-never-starts-from-compiled-code`)
    /// — so the assertion is now "on unless `=0`".
    #[test]
    fn the_satb_old_only_and_softref_hotspot_lru_switches_are_opt_in() {
        let f = |pairs: &[(&str, &str)]| VmFlags::from_source(&src(pairs));
        let d = f(&[]);
        assert!(d.gc.gen_satb_old_only, "gcf 2026-10-01: default on");
        // gen r5w4/defaults8: the soft-ref switch is per backend now — unset
        // is `None`, ON for the Generational heap and OFF for the others (see
        // `per_backend_switches_resolve_against_the_heap_family`).
        assert_eq!(d.gc.softref_hotspot_lru, None);
        assert!(!d.gc.softref_hotspot_lru_for(false));
        assert!(d.gc.softref_hotspot_lru_for(true));
        assert!(d.gc.gen_conc_alloc_fail_door);
        assert!(
            f(&[("CRATONVM_GEN_CONC_ALLOC_FAIL_DOOR", "1")])
                .gc
                .gen_conc_alloc_fail_door
        );
        assert!(
            !f(&[("CRATONVM_GEN_CONC_ALLOC_FAIL_DOOR", "0")])
                .gc
                .gen_conc_alloc_fail_door
        );
        assert!(
            f(&[("CRATONVM_GEN_SATB_OLD_ONLY", "1")])
                .gc
                .gen_satb_old_only
        );
        assert!(
            !f(&[("CRATONVM_GEN_SATB_OLD_ONLY", "0")])
                .gc
                .gen_satb_old_only
        );
        // gen r5w3/unload7: concurrent class unloading is opt-in, `=0` off.
        // gce ve2: default on since 2026-09-29 (`=0` kills).
        assert!(d.gc.gen_conc_class_unload);
        assert!(
            f(&[("CRATONVM_GEN_CONC_CLASS_UNLOAD", "1")])
                .gc
                .gen_conc_class_unload
        );
        assert!(
            !f(&[("CRATONVM_GEN_CONC_CLASS_UNLOAD", "0")])
                .gc
                .gen_conc_class_unload
        );
        // An explicit value wins on both families.
        for generational in [false, true] {
            assert!(f(&[("CRATONVM_SOFTREF_HOTSPOT_LRU", "1")])
                .gc
                .softref_hotspot_lru_for(generational));
            assert!(!f(&[("CRATONVM_SOFTREF_HOTSPOT_LRU", "0")])
                .gc
                .softref_hotspot_lru_for(generational));
        }
    }

    /// gen r4w3/oldgen3: the in-place old-gen give-back is a POLICY change, so
    /// it is opt-in — unset and `=0` both leave every page where it is.
    #[test]
    fn old_give_back_is_opt_in_and_zero_is_off() {
        let on = |v: &str| {
            VmFlags::from_source(&src(&[("CRATONVM_GC_OLD_GIVE_BACK", v)]))
                .gc
                .old_give_back
        };
        assert!(!VmFlags::from_source(&src(&[])).gc.old_give_back);
        assert!(!on("0"));
        assert!(!on(""));
        assert!(on("1"));
    }

    /// gen r4w4/oldgen4: the old-gen shrink, the walk-gap recovery and the
    /// refusal-armed compaction are default-ON with `=0` as the kill switch;
    /// the PROACTIVE fragmentation trigger is opt-in.
    #[test]
    fn old_gen_w4_switches_have_the_documented_defaults() {
        let flags = |k: &str, v: &str| VmFlags::from_source(&src(&[(k, v)])).gc;
        let none = VmFlags::from_source(&src(&[])).gc;
        assert!(none.old_shrink);
        assert!(none.old_walk_gap_recovery);
        assert!(none.old_oom_compact);
        assert!(!flags("CRATONVM_GC_OLD_OOM_COMPACT", "0").old_oom_compact);
        assert!(!none.old_frag_compact);
        assert!(!flags("CRATONVM_GC_OLD_SHRINK", "0").old_shrink);
        assert!(flags("CRATONVM_GC_OLD_SHRINK", "1").old_shrink);
        assert!(!flags("CRATONVM_GC_OLD_WALK_GAP_RECOVERY", "0").old_walk_gap_recovery);
        assert!(flags("CRATONVM_GC_OLD_FRAG_COMPACT", "1").old_frag_compact);
        assert!(!flags("CRATONVM_GC_OLD_FRAG_COMPACT", "0").old_frag_compact);
    }

    /// gen r4w5/oldcompact5: the compaction around pinned objects on the
    /// non-moving path is opt-in, and `=0` is off.
    #[test]
    fn old_gen_pinned_compaction_is_opt_in() {
        let flags = |k: &str, v: &str| VmFlags::from_source(&src(&[(k, v)])).gc;
        assert!(!VmFlags::from_source(&src(&[])).gc.old_pinned_compact);
        assert!(flags("CRATONVM_GC_OLD_PINNED_COMPACT", "1").old_pinned_compact);
        assert!(!flags("CRATONVM_GC_OLD_PINNED_COMPACT", "0").old_pinned_compact);
    }
    #[test]
    fn fragmented_nonmoving_pinned_compaction_defaults_on_and_can_be_killed() {
        let flags = |v: &str| {
            VmFlags::from_source(&src(&[("CRATONVM_GC_OLD_FRAG_PINNED_COMPACT", v)])).gc
        };
        assert!(VmFlags::from_source(&src(&[])).gc.old_frag_pinned_compact);
        assert!(flags("1").old_frag_pinned_compact);
        assert!(!flags("0").old_frag_pinned_compact);
    }

    /// gen r4w6/review6: thrash5's two kill switches are typed fields, default
    /// ON, off for the spellings the wave-5 readers honoured (`0`, `false`,
    /// `off`), and on for anything else.
    #[test]
    fn thrash5_kill_switches_are_default_on_typed_fields() {
        let gc = |k: &str, v: &str| VmFlags::from_source(&src(&[(k, v)])).gc;
        let unset = VmFlags::from_source(&src(&[])).gc;
        assert!(unset.gc_overhead_progress);
        assert!(unset.monitor_enter_spin);
        for off in ["0", "false", "off"] {
            assert!(
                !gc("CRATONVM_GC_OVERHEAD_PROGRESS", off).gc_overhead_progress,
                "{off}"
            );
            assert!(
                !gc("CRATONVM_MONITOR_ENTER_SPIN", off).monitor_enter_spin,
                "{off}"
            );
        }
        assert!(gc("CRATONVM_GC_OVERHEAD_PROGRESS", "1").gc_overhead_progress);
        assert!(gc("CRATONVM_MONITOR_ENTER_SPIN", "1").monitor_enter_spin);
        // Each switch is its own: turning one off leaves the other on.
        assert!(gc("CRATONVM_GC_OVERHEAD_PROGRESS", "0").monitor_enter_spin);
        assert!(gc("CRATONVM_MONITOR_ENTER_SPIN", "0").gc_overhead_progress);
    }

    /// gcd d2/j: the futile-young backoff is default ON with `=0` its only
    /// off spelling; the per-message preallocated OOMEs are default ON since
    /// gcf 2026-10-01 (vf2 o2-6: `k1` = `k0` = HotSpot), `=0` kills them.
    #[test]
    fn gcd_d2j_allocation_ladder_switches() {
        let gc = |k: &str, v: &str| VmFlags::from_source(&src(&[(k, v)])).gc;
        let unset = VmFlags::from_source(&src(&[])).gc;
        assert!(unset.futile_young_backoff);
        assert!(unset.preallocated_oome_kinds);
        assert!(!gc("CRATONVM_GC_FUTILE_YOUNG_BACKOFF", "0").futile_young_backoff);
        assert!(gc("CRATONVM_GC_FUTILE_YOUNG_BACKOFF", "1").futile_young_backoff);
        assert!(gc("CRATONVM_GC_PREALLOCATED_OOME_KINDS", "1").preallocated_oome_kinds);
        assert!(!gc("CRATONVM_GC_PREALLOCATED_OOME_KINDS", "0").preallocated_oome_kinds);
    }

    /// gen r4w6/oldpin6: the resize after a concurrent cycle's sweep is
    /// opt-in, and `=0` is off.
    #[test]
    fn old_gen_shrink_after_a_concurrent_sweep_is_opt_in() {
        let flags = |k: &str, v: &str| VmFlags::from_source(&src(&[(k, v)])).gc;
        assert!(
            !VmFlags::from_source(&src(&[]))
                .gc
                .old_shrink_after_concurrent
        );
        assert!(flags("CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT", "1").old_shrink_after_concurrent);
        assert!(!flags("CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT", "0").old_shrink_after_concurrent);
        assert!(!VmFlags::from_source(&src(&[])).gc.conc_walk_gap_recovery);
        assert!(flags("CRATONVM_GC_CONC_WALK_GAP_RECOVERY", "1").conc_walk_gap_recovery);
        assert!(!flags("CRATONVM_GC_CONC_WALK_GAP_RECOVERY", "0").conc_walk_gap_recovery);
    }

    /// gen r5w3/oldgen7: the old generation's interior decommit, its
    /// borrowing of the young budget and the top placement of humongous
    /// arrays are all opt-in, and `=0` is off.
    ///
    /// gen r5w4/defaults8: the `-Xms` usable-first split was opt-in here too;
    /// it is default ON now (`alloc_policy_defaults::GEN_XMS_USABLE_FIRST`, on
    /// `GenR5W3XmsTotalProbe`'s evidence), so the assertion is "on unless an
    /// off word".
    #[test]
    fn old_gen_sizing_switches_of_gen_r5w3_are_opt_in() {
        let flags = |k: &str, v: &str| VmFlags::from_source(&src(&[(k, v)])).gc;
        let unset = VmFlags::from_source(&src(&[])).gc;
        assert!(unset.gen_xms_usable_first);
        assert!(!flags("CRATONVM_GEN_XMS_USABLE_FIRST", "false").gen_xms_usable_first);
        assert!(!unset.old_interior_decommit);
        assert!(unset.old_borrow_young);
        // Default ON since the gcd triage (2026-09-28); `=0` restores the best fit.
        assert!(unset.old_humongous_top);
        assert!(flags("CRATONVM_GC_OLD_HUMONGOUS_TOP", "1").old_humongous_top);
        assert!(!flags("CRATONVM_GC_OLD_HUMONGOUS_TOP", "0").old_humongous_top);
        assert!(!flags("CRATONVM_GC_OLD_HUMONGOUS_TOP", "off").old_humongous_top);
        assert!(flags("CRATONVM_GEN_XMS_USABLE_FIRST", "1").gen_xms_usable_first);
        assert!(!flags("CRATONVM_GEN_XMS_USABLE_FIRST", "0").gen_xms_usable_first);
        assert!(flags("CRATONVM_GC_OLD_INTERIOR_DECOMMIT", "1").old_interior_decommit);
        assert!(!flags("CRATONVM_GC_OLD_INTERIOR_DECOMMIT", "0").old_interior_decommit);
        assert!(flags("CRATONVM_GC_OLD_BORROW_YOUNG", "1").old_borrow_young);
        assert!(!flags("CRATONVM_GC_OLD_BORROW_YOUNG", "0").old_borrow_young);
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
        // JIT r23 wave 2c, lane L5: `CRATONVM_REGEX_MATCHER_BYTECODE` (default
        // ON) makes the Matcher natives yield to the JDK bytecode, so the
        // effective gate is OFF by default and ON again under `=0`.
        assert!(!f.natives.native_matcher_find);
        let restored =
            VmFlags::from_source(&MapSource::new([("CRATONVM_REGEX_MATCHER_BYTECODE", "0")]));
        assert!(restored.natives.native_matcher_find);
        assert!(!f.natives.netty_queue_bridge);
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

    /// gen r4w4/alloc4: every allocation-policy arm reads its default from
    /// `alloc_policy_defaults` when unset, and the same words turn it on or
    /// off whichever way the default points — so flipping a default is the
    /// constant alone, and a control arm written `=0` is OFF on both sides of
    /// a flip.
    #[test]
    fn every_allocation_policy_arm_parses_against_its_one_default() {
        type Pick = fn(&GcFlags) -> bool;
        // gce e2/o: `CRATONVM_TLAB_SIZE_RETIRED`, `CRATONVM_TLAB_WASTE_SHRINK`
        // and `CRATONVM_GEN_PRISTINE_CHUNKS` were removed, so the table is 7.
        let arms: [(&str, bool, Pick); 7] = [
            (
                "CRATONVM_TLAB_FILLER_SKIP_ZERO",
                alloc_policy_defaults::TLAB_FILLER_SKIP_ZERO,
                |g| g.tlab_filler_skip_zero,
            ),
            (
                "CRATONVM_TLAB_GATE_BUMP_FLOOR",
                alloc_policy_defaults::TLAB_GATE_BUMP_FLOOR,
                |g| g.tlab_gate_bump_floor,
            ),
            // gen r5w4/defaults8: `CRATONVM_TLAB_SHARE_SIZER` left this table
            // (its default is per backend now — see
            // `per_backend_switches_resolve_against_the_heap_family`) and the
            // `-Xms` split, parsed against its constant since the same wave,
            // took its slot.
            (
                "CRATONVM_GEN_XMS_USABLE_FIRST",
                alloc_policy_defaults::GEN_XMS_USABLE_FIRST,
                |g| g.gen_xms_usable_first,
            ),
            (
                "CRATONVM_GEN_ZERO_ONCE",
                alloc_policy_defaults::GEN_ZERO_ONCE,
                |g| g.gen_zero_once,
            ),
            (
                "CRATONVM_GEN_HUMONGOUS_ZERO_UNLOCKED",
                alloc_policy_defaults::GEN_HUMONGOUS_ZERO_UNLOCKED,
                |g| g.gen_humongous_zero_unlocked,
            ),
            (
                "CRATONVM_GEN_TLAB_TAIL_SINK",
                alloc_policy_defaults::GEN_TLAB_TAIL_SINK,
                |g| g.gen_tlab_tail_sink,
            ),
            (
                "CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED",
                alloc_policy_defaults::GEN_HUMONGOUS_REF_ZERO_UNLOCKED,
                |g| g.gen_humongous_ref_zero_unlocked,
            ),
        ];
        for (key, default, pick) in arms {
            let read = |v: Option<&str>| with_thread_overrides(&[(key, v)], || pick(&flags().gc));
            assert_eq!(read(None), default, "{key}: unset must read the default");
            for off in ["0", "false", "off", "no"] {
                assert!(!read(Some(off)), "{key}={off} must be OFF");
            }
            for on in ["1", "true", "on", "yes"] {
                assert!(read(Some(on)), "{key}={on} must be ON");
            }
            assert!(
                alloc_policy_switch(&src(&[]), key, true),
                "unset + default-on"
            );
            assert!(
                !alloc_policy_switch(&src(&[]), key, false),
                "unset + default-off"
            );
        }
    }

    /// gcf f4/r: the `[GC] policy:` line names every `alloc_policy_defaults`
    /// constant, resolves the per-backend ones against the heap family, and
    /// says `(set)` only for a switch the environment carries.
    #[test]
    fn gcf_f4r_policy_line_names_every_switch_and_its_origin() {
        let env = |key: &str| runtime_var_os(key).map(|v| v.to_string_lossy().into_owned());
        let line = |generational: bool| {
            with_thread_overrides(&[("CRATONVM_TLAB_SHARE_SIZER", Some("0"))], || {
                flags().gc.policy_line(
                    if generational { "generational" } else { "g1" },
                    generational,
                    &env,
                )
            })
        };
        let gen = line(true);
        assert!(
            gen.starts_with("[GC] policy: backend=generational "),
            "{gen}"
        );
        assert!(gen.contains(" tlab_share_sizer=off(set)"), "{gen}");
        for constant in [
            "tlab_filler_skip_zero",
            "tlab_gate_bump_floor",
            "tlab_share_sizer",
            "gen_zero_once",
            "gen_humongous_zero_unlocked",
            "gen_tlab_tail_sink",
            "gen_humongous_ref_zero_unlocked",
            "softref_hotspot_lru",
            "gen_xms_usable_first",
        ] {
            assert!(
                gen.contains(&format!(" {constant}=")),
                "{constant} missing: {gen}"
            );
        }
        if std::env::var_os("CRATONVM_SOFTREF_HOTSPOT_LRU").is_none() {
            assert!(gen.contains(" softref_hotspot_lru=on(default)"), "{gen}");
            // jr20 wave 9 / gcE: HotSpot's LRU inputs on every backend.
            assert!(line(false).contains(" softref_hotspot_lru=on(default)"));
            // zr z1/rf: the ZGC line resolves the ZGC default.
            let zgc = flags().gc.policy_line("zgc", false, &env);
            assert!(zgc.contains(" softref_hotspot_lru=on(default)"), "{zgc}");
        }
    }

    /// gen r5w4/defaults8: a switch with a `BackendDefault` reads `None` when
    /// unset (or empty), resolves to the Generational default on a
    /// Generational heap and to the other default elsewhere, and an explicit
    /// value — in any of the allocation-policy spellings — wins on both
    /// families. `SOFTREF_HOTSPOT_LRU` is ON for Generational and OFF for G1
    /// and ZGC; `TLAB_SHARE_SIZER`'s Generational flip was reverted at the
    /// r5w4 merge and made again by gcg g6/jn (ON for Generational, OFF on G1
    /// and ZGC). A G1 or ZGC run with nothing set is
    /// byte-for-byte what it was (except that, since jr20 wave 9 / gcE, the
    /// VM itself latches `SOFTREF_HOTSPOT_LRU`'s Generational default on every
    /// backend: `vm_init.rs`).
    #[test]
    fn per_backend_switches_resolve_against_the_heap_family() {
        use alloc_policy_defaults::BackendDefault;
        type Pick = fn(&GcFlags, bool) -> bool;
        type Raw = fn(&GcFlags) -> Option<bool>;
        let arms: [(&str, BackendDefault, Pick, Raw, bool); 2] = [
            (
                "CRATONVM_TLAB_SHARE_SIZER",
                alloc_policy_defaults::TLAB_SHARE_SIZER,
                |g, generational| g.tlab_share_sizer_for(generational),
                |g| g.tlab_share_sizer,
                true,
            ),
            (
                "CRATONVM_SOFTREF_HOTSPOT_LRU",
                alloc_policy_defaults::SOFTREF_HOTSPOT_LRU,
                |g, generational| g.softref_hotspot_lru_for(generational),
                |g| g.softref_hotspot_lru,
                true,
            ),
        ];
        for (key, default, pick, raw, generational_on) in arms {
            assert_eq!(
                default,
                BackendDefault {
                    generational: generational_on,
                    other: false,
                },
                "{key}: only a Generational flip, never G1/ZGC"
            );
            let gc = |v: Option<&str>| {
                let pairs: Vec<(&str, &str)> = v.map(|v| (key, v)).into_iter().collect();
                VmFlags::from_source(&src(&pairs)).gc
            };
            for unset in [None, Some(""), Some("  ")] {
                let g = gc(unset);
                assert_eq!(raw(&g), None, "{key}={unset:?} keeps the default");
                assert_eq!(
                    pick(&g, true),
                    generational_on,
                    "{key}={unset:?}: the Generational default"
                );
                assert!(!pick(&g, false), "{key}={unset:?}: OFF on G1/ZGC");
            }
            for off in ["0", "false", "off", "no", "OFF"] {
                let g = gc(Some(off));
                assert_eq!(raw(&g), Some(false), "{key}={off}");
                assert!(
                    !pick(&g, true) && !pick(&g, false),
                    "{key}={off} is OFF everywhere"
                );
            }
            for on in ["1", "true", "on", "yes"] {
                let g = gc(Some(on));
                assert_eq!(raw(&g), Some(true), "{key}={on}");
                assert!(
                    pick(&g, true) && pick(&g, false),
                    "{key}={on} is ON everywhere"
                );
            }
        }
        // zr z1/rf: the soft LRU's ZGC arm. Unset: ON on ZGC, the family
        // default elsewhere (G1 still OFF, Generational still ON); an
        // explicit value wins on ZGC too (`=0` is the ZGC kill switch).
        {
            let gc = |v: Option<&str>| {
                let pairs: Vec<(&str, &str)> = v
                    .map(|v| ("CRATONVM_SOFTREF_HOTSPOT_LRU", v))
                    .into_iter()
                    .collect();
                VmFlags::from_source(&src(&pairs)).gc
            };
            let unset = gc(None);
            assert!(
                unset.softref_hotspot_lru_for_backend(false, true),
                "ZGC: ON"
            );
            assert!(
                !unset.softref_hotspot_lru_for_backend(false, false),
                "G1: OFF"
            );
            assert!(
                unset.softref_hotspot_lru_for_backend(true, false),
                "Gen: ON"
            );
            assert!(!gc(Some("0")).softref_hotspot_lru_for_backend(false, true));
            assert!(gc(Some("1")).softref_hotspot_lru_for_backend(false, false));
        }
        // The resolver itself, both defaults both ways.
        let d = BackendDefault {
            generational: false,
            other: true,
        };
        assert!(!d.resolve(None, true));
        assert!(d.resolve(None, false));
        assert!(d.resolve(Some(true), true));
        assert!(!d.resolve(Some(false), false));
    }

    /// gci i48/zl3 (`zx48-collector-scope-audit.md` A7): the share sizer's G1
    /// arm. Unset: ON on G1 (the Generational default), Generational ON, ZGC
    /// OFF; `=0` kills it on G1 and Generational; `=1` still arms ZGC. The
    /// `[GC] policy:` line resolves the G1 default on a G1 heap.
    #[test]
    fn gci_i48_zl3_tlab_share_sizer_g1_arm() {
        const KEY: &str = "CRATONVM_TLAB_SHARE_SIZER";
        let gc = |v: Option<&str>| {
            let pairs: Vec<(&str, &str)> = v.map(|v| (KEY, v)).into_iter().collect();
            VmFlags::from_source(&src(&pairs)).gc
        };
        assert!(alloc_policy_defaults::TLAB_SHARE_SIZER_G1, "G1 follows Gen: ON");
        assert_eq!(
            alloc_policy_defaults::TLAB_SHARE_SIZER_G1,
            alloc_policy_defaults::TLAB_SHARE_SIZER.generational,
            "same default as the Generational heap"
        );
        for unset in [None, Some(""), Some("  ")] {
            let g = gc(unset);
            assert!(g.tlab_share_sizer_for_backend(false, true), "{unset:?}: G1 ON");
            assert!(g.tlab_share_sizer_for_backend(true, false), "{unset:?}: Gen ON");
            assert!(!g.tlab_share_sizer_for_backend(false, false), "{unset:?}: ZGC OFF");
            assert_eq!(
                g.tlab_share_sizer_for_backend(false, false),
                g.tlab_share_sizer_for(false),
                "the non-G1 arms are tlab_share_sizer_for"
            );
        }
        for off in ["0", "false", "off", "no"] {
            let g = gc(Some(off));
            assert!(!g.tlab_share_sizer_for_backend(false, true), "{off}: G1 killed");
            assert!(!g.tlab_share_sizer_for_backend(true, false), "{off}: Gen killed");
        }
        for on in ["1", "true", "on", "yes"] {
            let g = gc(Some(on));
            assert!(g.tlab_share_sizer_for_backend(false, false), "{on}: ZGC armed");
            assert!(g.tlab_share_sizer_for_backend(false, true), "{on}: G1 armed");
        }
        let env = |_: &str| -> Option<String> { None };
        let g1 = gc(None).policy_line("g1", false, &env);
        assert!(g1.contains(" tlab_share_sizer=on(default)"), "{g1}");
        let zgc = gc(None).policy_line("zgc", false, &env);
        assert!(zgc.contains(" tlab_share_sizer=off(default)"), "{zgc}");
        let set = |k: &str| -> Option<String> { (k == KEY).then(|| "0".to_string()) };
        let killed = gc(Some("0")).policy_line("g1", false, &set);
        assert!(killed.contains(" tlab_share_sizer=off(set)"), "{killed}");
    }

    /// gci i4/fl: the opt-in `GcFlags` booleans that parsed PRESENCE
    /// (`gci-i3fl-gc-typed-flag-fields-and-shared-readers-still-parse-presence`)
    /// read the value: unset and every off word are off, any other word is
    /// on. Before the fix `CRATONVM_DBG_FORCE_MOVING=0` ran the unsafe arm.
    type GcPick = fn(&GcFlags) -> bool;
    const GCI_I4FL_GC_OPT_INS: &[(&str, GcPick)] = &[
        ("CRATONVM_MOVING_YOUNG_FALLBACKS", |f| f.moving_young_fallbacks),
        ("CRATONVM_NO_GC_PROMOTION_GUARD", |f| f.no_gc_promotion_guard),
        ("CRATONVM_PROMOTION_OOM_GUARD_BROAD", |f| f.promotion_oom_guard_broad),
        ("CRATONVM_NO_SELECTIVE_PROMOTE", |f| f.no_selective_promote),
        ("CRATONVM_SP_NO_COALESCE", |f| f.sp_no_coalesce),
        ("CRATONVM_NO_DEFRAG_PROMOTE", |f| f.no_defrag_promote),
        ("CRATONVM_CARD_TABLE_ONLY", |f| f.card_table_only),
        ("CRATONVM_GC_FULL_RSET_SCAN", |f| f.full_rset_scan),
        ("CRATONVM_GC_VERIFY_RSET", |f| f.verify_rset),
        ("CRATONVM_GC_SYNC_YOUNG_WIPE", |f| f.gc_sync_young_wipe),
        // CRATONVM_GC_PAR_EVAC_CARD_SEED left this list: default ON since gcj
        // j1yg-pause, pinned in `gcj_j1yg_the_parallel_card_seed_is_default_on`.
        // CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL left this list: default ON
        // (WARNING gci-user-flip-20261013), pinned in
        // `gci_user_flip_20261013_typed_fields_default_on`.
        ("CRATONVM_GEN_PINNED_TAIL_WINDOW_LIVE", |f| f.gen_pinned_tail_window_live),
        ("CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG", |f| f.gc_system_gc_moving_young),
        ("CRATONVM_DBG_FORCE_MOVING", |f| f.dbg_force_moving),
        ("CRATONVM_DBG_NO_NONMOVING_RECLAIM", |f| f.dbg_no_nonmoving_reclaim),
        ("CRATONVM_DBG_COMPACT_LEGACY", |f| f.dbg_compact_legacy),
        ("CRATONVM_DBG_SEED_ALL_OLD", |f| f.dbg_seed_all_old),
        ("CRATONVM_DBG_STALE_OBJREF", |f| f.dbg_stale_objref),
        ("CRATONVM_FWD_RESOLVE_STRICT", |f| f.fwd_resolve_strict),
        ("CRATONVM_MOVING_YOUNG_VERIFY", |f| f.moving_young_verify),
        ("CRATONVM_SP_VERIFY", |f| f.sp_verify),
        ("CRATONVM_GC_ARRAY_GUARD_BT", |f| f.gc_array_guard_bt),
        ("CRATONVM_DBG_A2", |f| f.dbg_a2),
        ("CRATONVM_DBG_SWEEP_LIVENESS", |f| f.dbg_sweep_liveness),
        ("CRATONVM_DBG_BADREF", |f| f.dbg_badref),
        ("CRATONVM_DBG_DEADREF_STORE", |f| f.dbg_deadref_store),
        ("CRATONVM_DBG_CELLCORRUPT", |f| f.dbg_cellcorrupt),
        ("CRATONVM_DBG_DESCTRACE", |f| f.dbg_desctrace),
        ("CRATONVM_DBG_FWDGUARD", |f| f.dbg_fwdguard),
        ("CRATONVM_DBG_GCPAUSE", |f| f.dbg_gcpause),
        ("CRATONVM_DBG_GCPHASE", |f| f.dbg_gcphase),
        ("CRATONVM_DBG_GCWRITE", |f| f.dbg_gcwrite),
        ("CRATONVM_DBG_HEAP_TRACE", |f| f.dbg_heap_trace),
        ("CRATONVM_DBG_MIRRORPIN", |f| f.dbg_mirrorpin),
        ("CRATONVM_DBG_OOBFIELD", |f| f.dbg_oobfield),
        ("CRATONVM_DBG_PRECISE", |f| f.dbg_precise),
        ("CRATONVM_DBG_RSET_AUDIT", |f| f.dbg_rset_audit),
        ("CRATONVM_DBG_RSET_AUDIT_YOUNG_SCAN", |f| f.dbg_rset_audit_young_scan),
        ("CRATONVM_DBG_SEEDHUNT", |f| f.dbg_seedhunt),
        ("CRATONVM_DBG_SWEEP_CENSUS", |f| f.dbg_sweep_census),
        ("CRATONVM_DBG_SWEEP_EDGES", |f| f.dbg_sweep_edges),
        ("CRATONVM_DBG_SWEEP_ZERO", |f| f.dbg_sweep_zero),
        ("CRATONVM_GC_LATE_RESOLVE_DROPPED", |f| f.late_resolve_dropped),
        ("CRATONVM_DBG_WATCHREF", |f| f.dbg_watchref),
        ("CRATONVM_DBG_YOUNGSTATE", |f| f.dbg_youngstate),
        ("CRATONVM_DBG_ZERO_RANGES", |f| f.dbg_zero_ranges),
        ("CRATONVM_DIAG_HIB32", |f| f.diag_hib32),
        ("CRATONVM_NO_EXACT_REFPROC_SURVIVAL", |f| f.no_exact_refproc_survival),
        ("CRATONVM_NO_REFERENT_IDENTITY_SCREEN", |f| f.no_referent_identity_screen),
        ("CRATONVM_NO_OLDGEN_COALESCE", |f| f.no_oldgen_coalesce),
        ("CRATONVM_GC_G1_ONLY_JIT_PINS", |f| f.g1_only_jit_pins),
        ("CRATONVM_DBG_G1DIAG", |f| f.g1_dbg_diag),
        ("CRATONVM_DBG_G1ACCESSOR", |f| f.g1_dbg_accessor),
        ("CRATONVM_SP_STATS", |f| f.sp_stats),
        ("CRATONVM_SP_TRACE", |f| f.sp_trace),
    ];

    #[test]
    fn gci_i4fl_typed_gc_opt_ins_read_the_off_words_as_off() {
        let cases = GCI_I4FL_GC_OPT_INS;
        // gcj j1yg-pause: 54 -- CRATONVM_GC_PAR_EVAC_CARD_SEED left the list.
        assert_eq!(cases.len(), 54);
        let unset = VmFlags::from_source(&MapSource::empty()).gc;
        for &(name, pick) in cases {
            assert!(!pick(&unset), "{name} unset must stay off (no default moves)");
            for off in ["", "0", "false", "off", "no", " OFF ", "No"] {
                let f = VmFlags::from_source(&src(&[(name, off)])).gc;
                assert!(!pick(&f), "{name}={off:?} must read as off");
            }
            for on in ["1", "true", "yes", "on", "rescue"] {
                let f = VmFlags::from_source(&src(&[(name, on)])).gc;
                assert!(pick(&f), "{name}={on:?} must read as on");
            }
        }
    }

    /// WARNING gci-user-flip-20261013: the typed GC fields the user flipped
    /// default ON (`truthy_word_default_true`). Unset and empty read ON; the
    /// off words kill; the on words stay on.
    #[test]
    fn gci_user_flip_20261013_typed_fields_default_on() {
        let cases: [(&str, GcPick); 3] = [
            ("CRATONVM_GEN_PINNED_YOUNG_COPY", |f| f.gen_pinned_young_copy),
            ("CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL", |f| f.gen_pinned_young_copy_parallel),
            ("CRATONVM_OLDGEN_COMPACT", |f| f.oldgen_compact),
        ];
        let unset = VmFlags::from_source(&MapSource::empty()).gc;
        for (name, pick) in cases {
            assert!(pick(&unset), "{name} unset reads ON (user flip)");
            for off in ["0", "false", "off", "no", " OFF "] {
                let f = VmFlags::from_source(&src(&[(name, off)])).gc;
                assert!(!pick(&f), "{name}={off:?} must kill");
            }
            for on in ["", "1", "true", "yes"] {
                let f = VmFlags::from_source(&src(&[(name, on)])).gc;
                assert!(pick(&f), "{name}={on:?} must read as on");
            }
        }
    }

    /// gcj j1yg-pause: the parallel card seed is default ON; every off word
    /// kills it.
    #[test]
    fn gcj_j1yg_the_parallel_card_seed_is_default_on() {
        let unset = VmFlags::from_source(&MapSource::empty()).gc;
        assert!(unset.gc_par_evac_card_seed, "unset reads ON");
        for off in ["0", "false", "off", "no"] {
            let f = VmFlags::from_source(&src(&[("CRATONVM_GC_PAR_EVAC_CARD_SEED", off)])).gc;
            assert!(!f.gc_par_evac_card_seed, "{off:?} must kill");
        }
    }

    /// The two value companions are `None` whenever their gate is off, so a
    /// consumer that tests only the value (`gen_heap`'s OOB write guard tests
    /// `dbg_oobfield_value.is_some()`) cannot be armed by `=0`.
    #[test]
    fn gci_i4fl_value_companions_follow_their_gate() {
        for off in ["", "0", "false", "off", "no"] {
            let f = VmFlags::from_source(&src(&[
                ("CRATONVM_DBG_OOBFIELD", off),
                ("CRATONVM_DBG_SWEEP_LIVENESS", off),
            ]))
            .gc;
            assert!(!f.dbg_oobfield && f.dbg_oobfield_value.is_none(), "OOBFIELD={off:?}");
            assert!(
                !f.dbg_sweep_liveness && f.dbg_sweep_liveness_value.is_none(),
                "SWEEP_LIVENESS={off:?}"
            );
        }
        let f = VmFlags::from_source(&src(&[
            ("CRATONVM_DBG_OOBFIELD", "ATNConfig"),
            ("CRATONVM_DBG_SWEEP_LIVENESS", "rescue"),
        ]))
        .gc;
        assert!(f.dbg_oobfield);
        assert_eq!(f.dbg_oobfield_value.as_deref(), Some("ATNConfig"));
        assert!(f.dbg_sweep_liveness);
        assert_eq!(f.dbg_sweep_liveness_value.as_deref(), Some("rescue"));
    }

    /// gci i4/fl review: the Generational opt-ins that parsed
    /// `non_empty_non_zero` (`=false` / `=off` / `=no` switched them ON) and
    /// `DBG_BLOCKED_ACCESS` (`=false` selected Panic) use the same value rule;
    /// unset, empty and `0` read exactly as before.
    #[test]
    fn gci_i4fl_generational_opt_ins_read_false_as_off() {
        // CRATONVM_GEN_PINNED_YOUNG_COPY left this list: default ON
        // (WARNING gci-user-flip-20261013), pinned in
        // `gci_user_flip_20261013_typed_fields_default_on`.
        let cases: [(&str, GcPick); 12] = [
            ("CRATONVM_GC_OLD_TRIGGER_HYSTERESIS", |f| f.old_trigger_hysteresis),
            ("CRATONVM_GC_NO_CONCURRENT_FIRST", |f| f.old_no_concurrent_first),
            ("CRATONVM_GEN_CONC_MARK_NO_LOCAL_STACK", |f| f.gen_conc_mark_no_local_stack),
            ("CRATONVM_GEN_CONC_NO_FROZEN_EAGER_SCAN", |f| f.gen_conc_no_frozen_eager_scan),
            ("CRATONVM_GEN_CONC_SERVICE_THREAD", |f| f.gen_conc_service_thread),
            ("CRATONVM_GC_OLD_GIVE_BACK", |f| f.old_give_back),
            ("CRATONVM_GC_OLD_FRAG_COMPACT", |f| f.old_frag_compact),
            ("CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4", |f| f.gen_young_pin_ledger_term4),
            ("CRATONVM_GC_OLD_PINNED_COMPACT", |f| f.old_pinned_compact),
            ("CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT", |f| f.old_shrink_after_concurrent),
            ("CRATONVM_GC_CONC_WALK_GAP_RECOVERY", |f| f.conc_walk_gap_recovery),
            ("CRATONVM_GC_OLD_INTERIOR_DECOMMIT", |f| f.old_interior_decommit),
        ];
        let unset = VmFlags::from_source(&MapSource::empty()).gc;
        for (name, pick) in cases {
            assert!(!pick(&unset), "{name} unset must stay off");
            for off in ["", "0", "false", "off", "no", "FALSE"] {
                let f = VmFlags::from_source(&src(&[(name, off)])).gc;
                assert!(!pick(&f), "{name}={off:?} must read as off");
            }
            for on in ["1", "true", "yes"] {
                let f = VmFlags::from_source(&src(&[(name, on)])).gc;
                assert!(pick(&f), "{name}={on:?} must read as on");
            }
        }
        let hyst = |v: &str| {
            VmFlags::from_source(&src(&[("CRATONVM_GC_OLD_TRIGGER_HYSTERESIS", v)]))
                .gc
                .old_trigger_hysteresis_explicit
        };
        assert_eq!(hyst("false"), Some(false));
        assert_eq!(hyst("no"), Some(false));
        assert_eq!(hyst("1"), Some(true));
        let blocked = |v: &str| {
            VmFlags::from_source(&src(&[("CRATONVM_DBG_BLOCKED_ACCESS", v)]))
                .gc
                .dbg_blocked_access
        };
        for off in ["", "0", "false", "off", "no"] {
            assert_eq!(blocked(off), BlockedAccessMode::Off, "BLOCKED_ACCESS={off:?}");
        }
        assert_eq!(blocked("warn"), BlockedAccessMode::Warn);
        assert_eq!(blocked("1"), BlockedAccessMode::Panic);
    }

    /// `JitFlags::shadow_stack` reads by value too; its VM twin
    /// (`conservative_roots::shadow_stack_enabled`) must use
    /// `runtime_flag_on` in the same build (gci i4/fl report, hunk X-SS).
    #[test]
    fn gci_i4fl_shadow_stack_reads_by_value() {
        assert!(!VmFlags::from_source(&MapSource::empty()).jit.shadow_stack);
        for off in ["", "0", "false", "off", "no"] {
            let f = VmFlags::from_source(&src(&[("CRATONVM_SHADOW_STACK", off)]));
            assert!(!f.jit.shadow_stack, "SHADOW_STACK={off:?}");
        }
        assert!(VmFlags::from_source(&src(&[("CRATONVM_SHADOW_STACK", "1")])).jit.shadow_stack);
    }

    /// gci i4/fl, page `gci-i3fl-default-on-root-switches-whose-minus-token-is-inert`:
    /// after R1 the grouped minus token of the four default-ON root switches
    /// resolves to a word their reader (`runtime_flag_default_on`) reads as
    /// OFF, and the plus token to one it reads as ON.
    #[test]
    fn gci_i4fl_default_on_root_switches_minus_token_reads_off() {
        for (group, token, key) in [
            ("CRATONVM_JIT", "xt-jit-root-scan", "CRATONVM_XT_JIT_ROOT_SCAN"),
            ("CRATONVM_JIT", "xt-helper-window-scan", "CRATONVM_XT_HELPER_WINDOW_SCAN"),
            ("CRATONVM_JIT", "xt-helper-window-pin", "CRATONVM_XT_HELPER_WINDOW_PIN"),
            ("CRATONVM_GC", "register-image-remap", "CRATONVM_REGISTER_IMAGE_REMAP"),
        ] {
            let minus = format!("-{token}");
            let raw = src(&[(group, minus.as_str())]);
            let got = crate::flag_groups::resolve(&raw).get(key);
            let off = match &got {
                Some(v) => value_is_off(&v.to_string_lossy()),
                None => false,
            };
            assert!(off, "{group}=-{token} must write an off word to {key}, got {got:?}");

            let raw = src(&[(group, token)]);
            let got = crate::flag_groups::resolve(&raw).get(key);
            let on = match &got {
                Some(v) => !value_is_off(&v.to_string_lossy()),
                None => true,
            };
            assert!(on, "{group}={token} must leave {key} on, got {got:?}");
        }
    }

    /// Source guard: `GcFlags::from_source` keeps exactly one presence pair,
    /// the `moving_young` tri-state (paired with the JIT's publication gate).
    /// The needle is built at run time so this test does not match itself.
    #[test]
    fn gci_i4fl_gc_flags_from_source_keeps_only_the_moving_young_presence_pair() {
        let text = include_str!("flags.rs");
        let impl_head = format!("{} GcFlags {{", "impl");
        let start = text.rfind(&impl_head).expect("the GcFlags::from_source impl");
        let end_mark = format!("pub struct {}", "JitFlags");
        let end = start + text[start..].find(&end_mark).expect("JitFlags follows it");
        let needle = format!("{}(src,", "present");
        let hits: Vec<&str> = text[start..end]
            .lines()
            .filter(|l| l.contains(&needle))
            .collect();
        assert_eq!(hits.len(), 2, "presence parses left in GcFlags::from_source: {hits:#?}");
        assert!(
            hits.iter().all(|l| l.contains("MOVING_YOUNG\")")),
            "only the moving_young pair may parse presence: {hits:#?}"
        );
    }
}
