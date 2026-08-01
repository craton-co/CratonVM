// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Typed, immutable configuration for the subsystems being migrated off direct
//! environment reads.
//!
//! # What this is for
//!
//! Report P1 ("Centralize configuration reads") asks for one thing:
//!
//! > Environment variables are parsed once into a typed immutable
//! > configuration; semantic code contains no direct environment reads.
//!
//! [`crate::flags::VmFlags`] already does the "parsed once" half for the GC,
//! loader, I/O and native surfaces. This module is where the **remaining**
//! subsystems land as they migrate, and it exists as its own module rather than
//! as five more fields on `GcFlags`/`JitFlags` for a reason worth stating: the
//! flags collected here are exactly the ones whose call sites are *not* a plain
//! `bool`. They are tri-states, paths, capacities and mode words, and flattening
//! them into `bool` at the parse boundary is precisely the mistake this module
//! is meant to make impossible — see [`JitVerifyConfig::verify_ir`].
//!
//! # One snapshot, not a second one
//!
//! [`SubsystemConfig`] hangs off [`crate::flags::VmFlags`], so it is populated
//! by the same single walk of `environ` and latched by the same `OnceLock`.
//! There is no separate cache to fall out of step, and
//! [`crate::flags::with_thread_overrides`] reaches these fields exactly as it
//! reaches every other typed flag.
//!
//! # Migrating a call site
//!
//! A call site that reads `cratonvm_types::flags::runtime_var_os` is already
//! inside the declared-flag boundary and already sees the snapshot; moving it
//! here buys **typing**, not correctness. A call site that reaches `std::env`
//! directly is not inside that boundary, and moving it here is a behaviour fix.
//! `docs/config/flag-inventory.md` lists both kinds and the order to take them
//! in.
//!
//! Each accessor below reproduces its call site's parse **byte for byte**, and
//! names the function it came from. Where the call site's default depends on the
//! build profile, the accessor takes that profile as an argument
//! (`debug_build: bool`) instead of evaluating `cfg!(debug_assertions)` here:
//! this crate can be compiled under a different profile than its caller, and
//! silently answering for the wrong one is the kind of divergence the typed
//! config exists to remove.

use std::ffi::{OsStr, OsString};

use crate::flags::{parse, FlagSource};

// ───────────────────────────────────────────────────────────────────────────
// JIT IR verifier
// ───────────────────────────────────────────────────────────────────────────

/// `jit::ir_verify` — the optional IR verification lanes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitVerifyConfig {
    /// `CRATONVM_JIT_VERIFY_IR`, as a **tri-state**.
    /// [`parse::tristate_word`].
    ///
    /// The three states are genuinely three, and collapsing them loses a
    /// behaviour:
    ///
    /// * `None` (unset, or an unrecognised spelling) — the per-pass verifier
    ///   follows the build profile, and the unconditional pre-lowering check
    ///   stays on.
    /// * `Some(true)` — the per-pass verifier runs even in a release build.
    /// * `Some(false)` — the per-pass verifier is off *and* the pre-lowering
    ///   check is disabled. That second effect is the operator's escape hatch
    ///   from a verifier false positive costing them the optimizing tier, and
    ///   it is reachable only from an explicit `0`, never from "unset".
    ///
    /// Use [`Self::per_pass_enabled`] and [`Self::pre_lower_disabled`] rather
    /// than reading this field directly.
    pub verify_ir: Option<bool>,
    /// `CRATONVM_JIT_VERIFY_TYPES` — the type-lattice lane. Default **off**;
    /// an unrecognised value is also off. [`parse::tristate_word`].
    pub check_types: bool,
    /// `CRATONVM_JIT_VERIFY_FRAME_STATES` — the safepoint-snapshot lane.
    /// Default **off**. [`parse::tristate_word`].
    pub check_frame_states: bool,
    /// `CRATONVM_JIT_VERIFY_SCHEDULE` — the **compatibility alias** for the two
    /// lanes below, which used to be one `check_schedule` flag. Default
    /// **off**. [`parse::tristate_word`].
    ///
    /// Kept as its own field rather than folded away because it is what seeds
    /// the other two when neither is set explicitly, and an operator's existing
    /// incantation has to keep meaning what it meant.
    pub check_schedule: bool,
    /// `CRATONVM_JIT_VERIFY_MEMORY_CHAIN` — memory-token chain integrity.
    /// Defaults to [`Self::check_schedule`]. [`parse::tristate_word`].
    pub check_memory_chain: bool,
    /// `CRATONVM_JIT_VERIFY_ARENA_ORDER` — the heuristic arena-order
    /// definition-before-use lane. Defaults to [`Self::check_schedule`].
    /// [`parse::tristate_word`].
    pub check_arena_order: bool,
}

impl JitVerifyConfig {
    fn from_source(src: &dyn FlagSource) -> Self {
        let schedule = parse::tristate_word(src, "CRATONVM_JIT_VERIFY_SCHEDULE").unwrap_or(false);
        Self {
            verify_ir: parse::tristate_word(src, "CRATONVM_JIT_VERIFY_IR"),
            check_types: parse::tristate_word(src, "CRATONVM_JIT_VERIFY_TYPES").unwrap_or(false),
            check_frame_states: parse::tristate_word(src, "CRATONVM_JIT_VERIFY_FRAME_STATES")
                .unwrap_or(false),
            check_schedule: schedule,
            check_memory_chain: parse::tristate_word(src, "CRATONVM_JIT_VERIFY_MEMORY_CHAIN")
                .unwrap_or(schedule),
            check_arena_order: parse::tristate_word(src, "CRATONVM_JIT_VERIFY_ARENA_ORDER")
                .unwrap_or(schedule),
        }
    }

    /// Whether the *per-pass* verifier runs, given the caller's build profile.
    ///
    /// Pass `cfg!(debug_assertions)` from the crate that owns the pass. Matches
    /// `jit::ir_verify::verify_enabled`.
    #[inline]
    pub fn per_pass_enabled(&self, debug_build: bool) -> bool {
        self.verify_ir.unwrap_or(debug_build)
    }

    /// Whether the *unconditional pre-lowering* check is disabled.
    ///
    /// Deliberately not the negation of [`Self::per_pass_enabled`]: it responds
    /// only to an explicit `0`, never to the build profile. Matches
    /// `jit::ir_verify::pre_lower_verify_disabled`.
    #[inline]
    pub fn pre_lower_disabled(&self) -> bool {
        self.verify_ir == Some(false)
    }

    /// Whether any optional lane is on — the cheap "is `from_env` worth
    /// building" test. `check_schedule` is deliberately absent: it is an alias
    /// that has already been folded into the two lanes it seeds.
    #[inline]
    pub fn any_optional_lane(&self) -> bool {
        self.check_types
            || self.check_frame_states
            || self.check_memory_chain
            || self.check_arena_order
    }
}

// ───────────────────────────────────────────────────────────────────────────
// JIT compilation metrics
// ───────────────────────────────────────────────────────────────────────────

/// `jit::metrics` — per-compilation measurement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JitMetricsConfig {
    /// `CRATONVM_JIT_METRICS` — collect per-compilation reports. Default
    /// **off**, including in debug builds: this is a measurement, and a clock
    /// read per phase would perturb the compile times it reports.
    /// [`parse::tristate_word`], defaulted to `false`.
    pub enabled: bool,
    /// `CRATONVM_JIT_METRICS_OUT` — append one JSON object per line per
    /// compilation to this path. `None` when unset or empty.
    /// [`parse::os_non_empty`].
    pub out_path: Option<OsString>,
    /// `CRATONVM_JIT_METRICS_RING` — retained report count. `None` when unset,
    /// unparseable, or zero, because a ring of zero would make the last report
    /// permanently unreadable, which is never what an operator means.
    /// [`parse::usize_positive`]. The fallback constant stays in `jit`; see
    /// [`Self::ring_capacity_or`].
    pub ring_capacity: Option<usize>,
}

impl JitMetricsConfig {
    fn from_source(src: &dyn FlagSource) -> Self {
        Self {
            enabled: parse::tristate_word(src, "CRATONVM_JIT_METRICS").unwrap_or(false),
            out_path: parse::os_non_empty(src, "CRATONVM_JIT_METRICS_OUT"),
            ring_capacity: parse::usize_positive(src, "CRATONVM_JIT_METRICS_RING"),
        }
    }

    /// The retained report count, falling back to the caller's own default.
    ///
    /// `jit::metrics::DEFAULT_RING_CAPACITY` is deliberately **not** duplicated
    /// here: it is a tuning constant of the ring, not of the environment.
    #[inline]
    pub fn ring_capacity_or(&self, default: usize) -> usize {
        self.ring_capacity.unwrap_or(default)
    }

    /// The JSON-lines sink path, if one was configured.
    #[inline]
    pub fn out_path(&self) -> Option<&OsStr> {
        self.out_path.as_deref()
    }
}

// ───────────────────────────────────────────────────────────────────────────
// GC card / barrier metrics
// ───────────────────────────────────────────────────────────────────────────

/// `gc::gc_metrics` — the counters that sit on the write barrier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcMetricsConfig {
    /// `CRATONVM_GC_CARD_METRICS` — arm the per-reference-store barrier
    /// counters. Presence is truth, so `=0` **enables** them.
    /// [`parse::present`].
    ///
    /// This gate is what keeps an unconditional atomic increment off a barrier
    /// that runs on every reference store, so its cost when off must stay one
    /// relaxed load. Reading it from this snapshot is for the *resolve* step
    /// only — `gc_metrics` keeps its thread-local byte gate in front.
    pub card_metrics: bool,
}

impl GcMetricsConfig {
    fn from_source(src: &dyn FlagSource) -> Self {
        Self {
            card_metrics: parse::present(src, "CRATONVM_GC_CARD_METRICS"),
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Thread-state stress checking
// ───────────────────────────────────────────────────────────────────────────

/// `vm::threading::thread_state` — the transition-legality tripwire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThreadStressConfig {
    /// `CRATONVM_STRESS_THREAD_STATES`, as a **tri-state**.
    /// [`parse::tristate_off_word`].
    ///
    /// `None` (unset or non-UTF-8) means "follow the build profile"; `0`,
    /// `false` and `off` stand the tripwire down even in a debug build, which
    /// is the bisection escape hatch for a wrong entry in the legality table
    /// itself; any other value arms it.
    ///
    /// Note the asymmetry the two accessors below preserve: arming the checks
    /// and making a violation *fatal* are the same variable but not the same
    /// predicate — a debug build reports without aborting an otherwise-passing
    /// run, and only an explicit setting escalates to a panic.
    pub thread_states: Option<bool>,
}

impl ThreadStressConfig {
    fn from_source(src: &dyn FlagSource) -> Self {
        Self {
            thread_states: parse::tristate_off_word(src, "CRATONVM_STRESS_THREAD_STATES"),
        }
    }

    /// Whether the legality tripwire is armed, given the caller's build
    /// profile. Matches `thread_state::stress_checks_enabled`.
    #[inline]
    pub fn checks_enabled(&self, debug_build: bool) -> bool {
        self.thread_states.unwrap_or(debug_build)
    }

    /// Whether an observed illegal transition panics rather than being counted
    /// and logged. Matches `thread_state::violations_are_fatal` — note it does
    /// **not** consult the build profile.
    #[inline]
    pub fn violations_are_fatal(&self) -> bool {
        self.thread_states == Some(true)
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Capability model
// ───────────────────────────────────────────────────────────────────────────

/// `native_api::capability` — the per-VM capability model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilityConfig {
    /// `CRATONVM_CAPABILITY_MODE` — the requested mode word, unparsed.
    /// [`parse::utf8`].
    ///
    /// Kept raw on purpose. `CapabilityMode::parse` accepts operator-friendly
    /// aliases and *warns on stderr* for an unrecognised word before falling
    /// back to permissive; that diagnostic belongs to `native-api`, and moving
    /// the parse down here would either duplicate the alias table or move a
    /// user-facing message into a crate that has no business printing one.
    pub mode: Option<String>,
    /// `CRATONVM_CAPABILITY_GRANTS` — the `;`-separated grant list, unparsed.
    /// [`parse::utf8`]. Same reasoning as [`Self::mode`]: the grant grammar and
    /// its per-entry warning live in `native-api`.
    pub grants: Option<String>,
    /// `CRATONVM_CAPABILITY_LOG` — log the first use of each distinct
    /// capability to stderr. Presence is truth. [`parse::present`].
    pub log_first_use: bool,
}

impl CapabilityConfig {
    fn from_source(src: &dyn FlagSource) -> Self {
        Self {
            mode: parse::utf8(src, "CRATONVM_CAPABILITY_MODE"),
            grants: parse::utf8(src, "CRATONVM_CAPABILITY_GRANTS"),
            log_first_use: parse::present(src, "CRATONVM_CAPABILITY_LOG"),
        }
    }

    /// The mode word as requested, for `CapabilityMode::parse`.
    #[inline]
    pub fn mode_word(&self) -> Option<&str> {
        self.mode.as_deref()
    }

    /// The grant list as requested, for `parse_grant_list`.
    #[inline]
    pub fn grant_list(&self) -> Option<&str> {
        self.grants.as_deref()
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Root
// ───────────────────────────────────────────────────────────────────────────

/// The migrated subsystem configuration, grouped by owner.
///
/// Reached through [`crate::flags::flags`], so it is parsed once per process
/// from one walk of `environ` and is immutable thereafter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubsystemConfig {
    /// `jit::ir_verify`.
    pub jit_verify: JitVerifyConfig,
    /// `jit::metrics`.
    pub jit_metrics: JitMetricsConfig,
    /// `gc::gc_metrics`.
    pub gc_metrics: GcMetricsConfig,
    /// `vm::threading::thread_state`.
    pub thread_stress: ThreadStressConfig,
    /// `native_api::capability`.
    pub capability: CapabilityConfig,
}

impl SubsystemConfig {
    /// Build from an explicit source. Touches no globals, so a unit test never
    /// has to mutate the process environment to exercise a parse.
    pub fn from_source(src: &dyn FlagSource) -> Self {
        Self {
            jit_verify: JitVerifyConfig::from_source(src),
            jit_metrics: JitMetricsConfig::from_source(src),
            gc_metrics: GcMetricsConfig::from_source(src),
            thread_stress: ThreadStressConfig::from_source(src),
            capability: CapabilityConfig::from_source(src),
        }
    }
}

/// The process-wide subsystem configuration.
///
/// Shorthand for `flags().subsystems`; latches on first use exactly as
/// [`crate::flags::flags`] does, and honours the same test overrides.
#[inline]
pub fn subsystems() -> &'static SubsystemConfig {
    &crate::flags::flags().subsystems
}

/// The JIT IR-verifier configuration.
#[inline]
pub fn jit_verify() -> &'static JitVerifyConfig {
    &subsystems().jit_verify
}

/// The JIT metrics configuration.
#[inline]
pub fn jit_metrics() -> &'static JitMetricsConfig {
    &subsystems().jit_metrics
}

/// The GC metrics configuration.
#[inline]
pub fn gc_metrics() -> &'static GcMetricsConfig {
    &subsystems().gc_metrics
}

/// The thread-state stress configuration.
#[inline]
pub fn thread_stress() -> &'static ThreadStressConfig {
    &subsystems().thread_stress
}

/// The capability-model configuration.
#[inline]
pub fn capability() -> &'static CapabilityConfig {
    &subsystems().capability
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flags::MapSource;

    fn src(pairs: &[(&str, &str)]) -> MapSource {
        MapSource::new(pairs.iter().copied())
    }

    #[test]
    fn empty_source_is_the_shipped_default() {
        let c = SubsystemConfig::from_source(&MapSource::empty());
        assert_eq!(c, SubsystemConfig::default());

        // Every knob here is opt-in, so "nothing set" must be "nothing armed".
        assert_eq!(c.jit_verify.verify_ir, None);
        assert!(!c.jit_verify.any_optional_lane());
        assert!(!c.jit_metrics.enabled);
        assert_eq!(c.jit_metrics.out_path(), None);
        assert!(!c.gc_metrics.card_metrics);
        assert_eq!(c.thread_stress.thread_states, None);
        assert!(!c.thread_stress.violations_are_fatal());
        assert_eq!(c.capability.mode_word(), None);
        assert!(!c.capability.log_first_use);
    }

    #[test]
    fn verify_ir_keeps_its_three_states_apart() {
        let unset = JitVerifyConfig::from_source(&MapSource::empty());
        assert!(unset.per_pass_enabled(true));
        assert!(!unset.per_pass_enabled(false));
        assert!(
            !unset.pre_lower_disabled(),
            "unset must never disable the pre-lowering check — only an \
             explicit 0 does"
        );

        let on = JitVerifyConfig::from_source(&src(&[("CRATONVM_JIT_VERIFY_IR", "1")]));
        assert!(on.per_pass_enabled(false));
        assert!(!on.pre_lower_disabled());

        let off = JitVerifyConfig::from_source(&src(&[("CRATONVM_JIT_VERIFY_IR", "0")]));
        assert!(!off.per_pass_enabled(true));
        assert!(off.pre_lower_disabled());

        // An unrecognised spelling is "unset", not "on": `env_flag` in
        // `jit::ir_verify` returns `None` for it and the caller defaults.
        let junk = JitVerifyConfig::from_source(&src(&[("CRATONVM_JIT_VERIFY_IR", "maybe")]));
        assert_eq!(junk.verify_ir, None);
        assert!(!junk.pre_lower_disabled());
    }

    #[test]
    fn verify_lanes_accept_every_spelling_the_jit_helper_accepts() {
        for word in ["1", "true", "yes", "on", " ON ", "True"] {
            let c = JitVerifyConfig::from_source(&src(&[("CRATONVM_JIT_VERIFY_TYPES", word)]));
            assert!(c.check_types, "{word:?} should enable the type lane");
        }
        for word in ["0", "false", "no", "off", "", "banana"] {
            let c = JitVerifyConfig::from_source(&src(&[("CRATONVM_JIT_VERIFY_TYPES", word)]));
            assert!(!c.check_types, "{word:?} should leave the type lane off");
        }
    }

    #[test]
    fn verify_schedule_still_seeds_the_two_lanes_it_was_split_into() {
        let alias = JitVerifyConfig::from_source(&src(&[("CRATONVM_JIT_VERIFY_SCHEDULE", "1")]));
        assert!(alias.check_memory_chain);
        assert!(alias.check_arena_order);

        // An explicit lane wins over the alias in both directions.
        let narrowed = JitVerifyConfig::from_source(&src(&[
            ("CRATONVM_JIT_VERIFY_SCHEDULE", "1"),
            ("CRATONVM_JIT_VERIFY_ARENA_ORDER", "0"),
        ]));
        assert!(narrowed.check_memory_chain);
        assert!(!narrowed.check_arena_order);

        let widened =
            JitVerifyConfig::from_source(&src(&[("CRATONVM_JIT_VERIFY_MEMORY_CHAIN", "1")]));
        assert!(!widened.check_schedule);
        assert!(widened.check_memory_chain);
        assert!(!widened.check_arena_order);
    }

    #[test]
    fn metrics_ring_of_zero_falls_back_rather_than_blinding_the_ring() {
        let zero = JitMetricsConfig::from_source(&src(&[("CRATONVM_JIT_METRICS_RING", "0")]));
        assert_eq!(zero.ring_capacity, None);
        assert_eq!(zero.ring_capacity_or(256), 256);

        let junk = JitMetricsConfig::from_source(&src(&[("CRATONVM_JIT_METRICS_RING", "lots")]));
        assert_eq!(junk.ring_capacity_or(256), 256);

        let set = JitMetricsConfig::from_source(&src(&[("CRATONVM_JIT_METRICS_RING", " 1024 ")]));
        assert_eq!(set.ring_capacity_or(256), 1024);
    }

    #[test]
    fn an_empty_metrics_path_is_no_sink_not_a_file_called_nothing() {
        let empty = JitMetricsConfig::from_source(&src(&[("CRATONVM_JIT_METRICS_OUT", "")]));
        assert_eq!(empty.out_path(), None);

        let path = src(&[("CRATONVM_JIT_METRICS_OUT", "/tmp/j.jsonl")]);
        let set = JitMetricsConfig::from_source(&path);
        assert_eq!(set.out_path(), Some(OsStr::new("/tmp/j.jsonl")));
    }

    #[test]
    fn card_metrics_is_presence_so_zero_still_arms_it() {
        // Not a typo, and not a parser to "fix" without measuring: the read
        // site is `runtime_var_os(...).is_some()`.
        let c = GcMetricsConfig::from_source(&src(&[("CRATONVM_GC_CARD_METRICS", "0")]));
        assert!(c.card_metrics);
    }

    #[test]
    fn thread_stress_arming_and_fatality_are_different_questions() {
        let unset = ThreadStressConfig::from_source(&MapSource::empty());
        assert!(unset.checks_enabled(true));
        assert!(!unset.checks_enabled(false));
        assert!(
            !unset.violations_are_fatal(),
            "a debug build reports; only an explicit setting aborts"
        );

        for word in ["0", "false", "off"] {
            let s = src(&[("CRATONVM_STRESS_THREAD_STATES", word)]);
            let c = ThreadStressConfig::from_source(&s);
            assert!(!c.checks_enabled(true), "{word:?} must disarm it");
            assert!(!c.violations_are_fatal());
        }

        // Unlike `tristate_word`, an unrecognised value here is *on* — the read
        // site is `Ok(_) => true`.
        for word in ["1", "yes", "banana"] {
            let s = src(&[("CRATONVM_STRESS_THREAD_STATES", word)]);
            let c = ThreadStressConfig::from_source(&s);
            assert!(c.checks_enabled(false), "{word:?} must arm the tripwire");
            assert!(c.violations_are_fatal());
        }
    }

    #[test]
    fn capability_words_reach_native_api_unparsed() {
        let c = CapabilityConfig::from_source(&src(&[
            ("CRATONVM_CAPABILITY_MODE", "audit"),
            ("CRATONVM_CAPABILITY_GRANTS", "file-read:/tmp;net-connect:*"),
            ("CRATONVM_CAPABILITY_LOG", ""),
        ]));
        assert_eq!(c.mode_word(), Some("audit"));
        assert_eq!(c.grant_list(), Some("file-read:/tmp;net-connect:*"));
        // Presence is truth, so an empty value still turns logging on.
        assert!(c.log_first_use);
    }

    #[test]
    fn grouped_tokens_reach_every_subsystem_knob() {
        // The point of declaring these: `CRATONVM_JIT=...` must be able to say
        // what `CRATONVM_JIT_VERIFY_TYPES=1` says.
        let raw = src(&[
            ("CRATONVM_JIT", "verify-types,metrics,metrics-ring=64"),
            ("CRATONVM_GC", "card-metrics"),
            ("CRATONVM_THREADS", "-stress-thread-states"),
            ("CRATONVM_SECURITY", "capability-mode=enforce"),
        ]);
        let resolved = crate::flag_groups::resolve(&raw);
        let c = SubsystemConfig::from_source(&resolved);

        assert!(c.jit_verify.check_types);
        assert!(!c.jit_verify.check_schedule);
        assert!(!c.jit_verify.check_memory_chain);
        assert!(!c.jit_verify.check_arena_order);
        assert!(c.jit_metrics.enabled);
        assert_eq!(c.jit_metrics.ring_capacity_or(256), 64);
        assert!(c.gc_metrics.card_metrics);
        assert_eq!(c.thread_stress.thread_states, Some(false));
        assert!(!c.thread_stress.checks_enabled(true));
        assert_eq!(c.capability.mode_word(), Some("enforce"));
    }
}
