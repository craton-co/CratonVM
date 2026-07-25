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
//! In practice only three test binaries depend on the old behaviour, and each
//! already documents that it must be its own binary and must `set_var` before
//! anything else runs (`gc/tests/stale_objref_debug_assertion.rs`,
//! `gc/tests/stale_objref_quarantine_ring.rs`). Those remain correct. New tests
//! should use [`VmFlags::from_source`] with a [`MapSource`] instead of mutating
//! the process environment, which is also what makes them safe under Rust 2024,
//! where `set_var` is `unsafe`.
//!
//! [`install`] lets a launcher build the config explicitly — environment plus
//! `-XX:` command-line flags — and publish it before anything reads a flag.
//!
//! # Truth tables
//!
//! There is no single answer in this tree to "what does `CRATONVM_FOO=false`
//! mean": the [`parse`] module carries five *different* boolean parsers because
//! five different ones are in use today. `docs/internal/flag-census.md` §10 has
//! the full matrix. Unifying them is a behaviour change and is deliberately not
//! part of this refactor; naming each parser at each field is what makes the
//! divergence visible enough to retire later, flag by flag, with benchmarks.

use std::collections::HashMap;
use std::ffi::OsString;
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
pub struct MapSource(HashMap<String, OsString>);

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

    /// Set one entry, builder-style.
    pub fn with(mut self, name: &str, value: &str) -> Self {
        self.0.insert(name.to_string(), OsString::from(value));
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
    /// Used by `native_io::env_flag_enabled` for the certified/untrusted
    /// deployment profile.
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

/// Flags read by `cratonvm-gc` (and, for the shared ones, by `jit` and `vm`).
///
/// Unless a field says otherwise it was built with [`parse::present`], i.e. it
/// is `true` whenever the variable is set to anything at all.
#[derive(Debug, Clone, Default)]
pub struct GcFlags {
    // ── Semantics-changing ────────────────────────────────────────────────
    /// `CRATONVM_MOVING_YOUNG` — enable the moving young generation.
    pub moving_young: bool,
    /// `CRATONVM_ALLOW_MOVING_YOUNG` — permit moving young collection in
    /// contexts that otherwise force non-moving.
    pub allow_moving_young: bool,
    /// `CRATONVM_MOVING_YOUNG_FALLBACKS` — take the moving-young fallback
    /// paths instead of failing.
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
    /// `CRATONVM_CARD_TABLE_ONLY` — restrict old→young discovery to the card
    /// table.
    pub card_table_only: bool,
    /// `CRATONVM_OLD_SWEEP_JIT` — default **ON** opt-out for the old-gen
    /// non-moving sweep. [`parse::on_unless_zero`].
    pub old_sweep_jit: bool,
    /// `CRATONVM_G1_PARALLEL_EVAC` — parallel STW evacuation.
    /// [`parse::one_or_true`].
    pub g1_parallel_evac: bool,
    /// `CRATONVM_G1_NO_EVAC_RETRY` — do not retry a failed evacuation.
    pub g1_no_evac_retry: bool,
    /// `CRATONVM_G1_WORKERS` — override the G1 worker count, clamped to `>= 1`.
    /// [`parse::usize_min1`].
    pub g1_workers: Option<usize>,
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
    /// `CRATONVM_G1_DBG_HEADERS`
    pub g1_dbg_headers: bool,
    /// `CRATONVM_G1_DBG_PINS`
    pub g1_dbg_pins: bool,
    /// `CRATONVM_G1_DBG_REACH`
    pub g1_dbg_reach: bool,
    /// `CRATONVM_G1_DBG_ROOTCENSUS`
    pub g1_dbg_rootcensus: bool,
    /// `CRATONVM_G1_DBG_ZERO`
    pub g1_dbg_zero: bool,
    /// `CRATONVM_SP_STATS`
    pub sp_stats: bool,
    /// `CRATONVM_SP_TRACE`
    pub sp_trace: bool,
}

impl GcFlags {
    fn from_source(src: &dyn FlagSource) -> Self {
        use parse::*;
        Self {
            moving_young: present(src, "CRATONVM_MOVING_YOUNG"),
            allow_moving_young: present(src, "CRATONVM_ALLOW_MOVING_YOUNG"),
            moving_young_fallbacks: present(src, "CRATONVM_MOVING_YOUNG_FALLBACKS"),
            no_gc_promotion_guard: present(src, "CRATONVM_NO_GC_PROMOTION_GUARD"),
            promotion_oom_guard_broad: present(src, "CRATONVM_PROMOTION_OOM_GUARD_BROAD"),
            no_selective_promote: present(src, "CRATONVM_NO_SELECTIVE_PROMOTE"),
            sp_no_coalesce: present(src, "CRATONVM_SP_NO_COALESCE"),
            card_table_only: present(src, "CRATONVM_CARD_TABLE_ONLY"),
            old_sweep_jit: on_unless_zero(src, "CRATONVM_OLD_SWEEP_JIT"),
            g1_parallel_evac: one_or_true(src, "CRATONVM_G1_PARALLEL_EVAC"),
            g1_no_evac_retry: present(src, "CRATONVM_G1_NO_EVAC_RETRY"),
            g1_workers: usize_min1(src, "CRATONVM_G1_WORKERS"),
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
            dbg_watchref: present(src, "CRATONVM_DBG_WATCHREF"),
            dbg_watch_cell: hex_addr_or_zero(src, "CRATONVM_DBG_WATCH_CELL"),
            dbg_youngstate: present(src, "CRATONVM_DBG_YOUNGSTATE"),
            dbg_zero_ranges: present(src, "CRATONVM_DBG_ZERO_RANGES"),
            diag_hib32: present(src, "CRATONVM_DIAG_HIB32"),
            g1_dbg_headers: present(src, "CRATONVM_G1_DBG_HEADERS"),
            g1_dbg_pins: present(src, "CRATONVM_G1_DBG_PINS"),
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
    /// `CRATONVM_SHADOW_STACK` — use the JIT shadow stack for root discovery.
    /// Read from `gc`, `jit` and `vm`, which is why it is in the shared config
    /// rather than a crate-private cache.
    pub shadow_stack: bool,
}

impl JitFlags {
    fn from_source(src: &dyn FlagSource) -> Self {
        Self {
            shadow_stack: parse::present(src, "CRATONVM_SHADOW_STACK"),
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
    /// `CRATONVM_DBG_DUPCLASS` — [`parse::present_utf8`].
    pub dbg_dupclass: bool,
    /// `CRATONVM_DBG_DUPCLASS_BT`
    pub dbg_dupclass_bt: bool,
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
            dbg_dupclass: present_utf8(src, "CRATONVM_DBG_DUPCLASS"),
            dbg_dupclass_bt: present(src, "CRATONVM_DBG_DUPCLASS_BT"),
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
    /// `CRATONVM_CONFINE_IO` — certified deployment profile: enable CWD
    /// confinement, fail closed. [`parse::truthy_word`].
    pub confine_io: bool,
    /// `CRATONVM_UNTRUSTED_CODE` — untrusted-bytecode profile: enable CWD
    /// confinement, warn loudly. [`parse::truthy_word`].
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
    /// `CRATONVM_SYNTHETIC_FILEWRITER=1` — opt back into the synthetic (known
    /// lossy) `FileWriter`. Consumers want `!synthetic_filewriter_forced`.
    /// [`parse::exactly_one`].
    pub synthetic_filewriter_forced: bool,
    /// `CRATONVM_SYNTHETIC_RAF=1` — opt back into the synthetic
    /// `RandomAccessFile`. Consumers want `!synthetic_raf_forced`.
    /// [`parse::exactly_one`].
    pub synthetic_raf_forced: bool,
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
}

impl IoFlags {
    fn from_source(src: &dyn FlagSource) -> Self {
        use parse::*;
        Self {
            confine_io: truthy_word(src, "CRATONVM_CONFINE_IO"),
            untrusted_code: truthy_word(src, "CRATONVM_UNTRUSTED_CODE"),
            block_private_nets: truthy_word(src, "CRATONVM_BLOCK_PRIVATE_NETS"),
            resolve_outbound_host: truthy_word(src, "CRATONVM_RESOLVE_OUTBOUND_HOST"),
            real_net_sockets: present(src, "CRATONVM_REAL_NET_SOCKETS"),
            synthetic_filewriter_forced: exactly_one(src, "CRATONVM_SYNTHETIC_FILEWRITER"),
            synthetic_raf_forced: exactly_one(src, "CRATONVM_SYNTHETIC_RAF"),
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
        }
    }

    /// Build from the process environment.
    pub fn from_env() -> Self {
        Self::from_source(&EnvSource)
    }
}

static FLAGS: OnceLock<VmFlags> = OnceLock::new();

/// The process-wide configuration.
///
/// Initialised from the environment on first call. See the module docs for the
/// latching rules.
#[inline]
pub fn flags() -> &'static VmFlags {
    FLAGS.get_or_init(VmFlags::from_env)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn src(pairs: &[(&str, &str)]) -> MapSource {
        MapSource::new(pairs.iter().copied())
    }

    #[test]
    fn empty_source_matches_all_documented_defaults() {
        let f = VmFlags::from_source(&MapSource::empty());
        // Every opt-in flag is off…
        assert!(!f.gc.moving_young);
        assert!(!f.gc.card_table_only);
        assert!(!f.gc.dbg_a2);
        assert!(!f.jit.shadow_stack);
        // …and every default-ON flag is on.
        assert!(f.gc.old_sweep_jit);
        // Value flags fall back.
        assert_eq!(f.gc.g1_workers, None);
        assert_eq!(f.gc.gc_stress_bytes, None);
        assert_eq!(f.gc.dbg_stale_objref_cycles, 1);
        assert_eq!(f.gc.dbg_watch_cell, 0);
        assert_eq!(f.gc.dbg_blocked_access, BlockedAccessMode::Off);
    }

    #[test]
    fn presence_parser_treats_zero_as_set() {
        // This is the surprising-but-existing majority semantics: `X=0` is ON
        // for every `var_os(..).is_some()` site. Locked down so the migration
        // cannot quietly "fix" it.
        let f = VmFlags::from_source(&src(&[("CRATONVM_MOVING_YOUNG", "0")]));
        assert!(f.gc.moving_young);
        let f = VmFlags::from_source(&src(&[("CRATONVM_MOVING_YOUNG", "")]));
        assert!(f.gc.moving_young);
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
    fn g1_parallel_evac_accepts_one_or_true_only() {
        assert!(
            VmFlags::from_source(&src(&[("CRATONVM_G1_PARALLEL_EVAC", "1")]))
                .gc
                .g1_parallel_evac
        );
        assert!(
            VmFlags::from_source(&src(&[("CRATONVM_G1_PARALLEL_EVAC", "TRUE")]))
                .gc
                .g1_parallel_evac
        );
        assert!(
            !VmFlags::from_source(&src(&[("CRATONVM_G1_PARALLEL_EVAC", "yes")]))
                .gc
                .g1_parallel_evac
        );
        assert!(
            !VmFlags::from_source(&src(&[("CRATONVM_G1_PARALLEL_EVAC", "2")]))
                .gc
                .g1_parallel_evac
        );
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
}
