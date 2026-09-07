// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The canonical `CRATONVM_*` environment surface.
//!
//! # The problem this solves
//!
//! `flag-census.md` counted **692** distinct `CRATONVM_*`
//! identifiers: 559 with a Rust read site, 133 referenced only by prose. They
//! accumulated at roughly one per fixed bug with no retirement path, they
//! duplicate each other (`CRATONVM_REAL_AQS` / `CRATONVM_SYNTHETIC_AQS` are one
//! switch spelled twice), and eight *documented* ones are silent no-ops because
//! a default was flipped and only the opt-out half was renamed.
//!
//! This module replaces that surface with **fifteen** environment variables:
//! ten grouped ones that take a comma-separated token list, and five scalars
//! that are genuinely independent.
//!
//! ```text
//! CRATONVM_JIT=-bce,unroll,threshold=200
//! CRATONVM_DBG=gc-stress=65536,loader-trace
//! CRATONVM_REAL=aqs,-agroal
//! ```
//!
//! A token may be prefixed `-` to turn the knob **off** and `+` (or nothing) to
//! turn it on, and may carry `=value`. `all` enables every token in the group —
//! useful for `CRATONVM_DBG`, meaningless-but-harmless elsewhere.
//!
//! # Polarity is now stated once, in one place
//!
//! The old surface encoded polarity in the *name*, inconsistently:
//! `CRATONVM_JIT_NO_BCE`, `CRATONVM_NO_PRECISE_JIT_MAPS`,
//! `CRATONVM_DISABLE_UNROLL` and `CRATONVM_JIT_UNROLL` are four knobs in three
//! spellings, two of which are opt-outs whose opt-in twin is documented but dead.
//! [`INVENTORY`] gives each knob **one** positive token with an explicit
//! `on_key`/`off_key` pair, so `-bce` and `unroll` read the way they mean. The
//! seven places where an `X` and a `NO_X` both existed collapse into one token
//! each; that merge is asserted by [`tests::every_token_is_unique`].
//!
//! # Legacy names keep working — by one rule, not 491 aliases
//!
//! Every entry in [`INVENTORY`] names the legacy environment variable the
//! token expands to. That expansion *is* the compatibility rule: a runbook that
//! exports `CRATONVM_DBG_GC_STRESS=65536` still works, because that is
//! precisely the key `CRATONVM_DBG=gc-stress=65536` writes. There is no
//! separate alias table to maintain and no per-flag compatibility handle — the
//! legacy names simply stop being the *documented* surface and become the
//! internal keys the typed [`crate::flags::VmFlags`] fields already read.
//!
//! Legacy names are still recognised when set directly; [`resolve`] reports
//! them so the launcher can print a single deprecation line naming the grouped
//! spelling to switch to.
//!
//! # Precedence
//!
//! A grouped variable **wins** over a legacy variable naming the same knob.
//! That is what makes `CRATONVM_DBG=-heap-stale` able to switch off a stale
//! `CRATONVM_DBG_HEAP_STALE=1` inherited from a parent shell; the reverse rule
//! would leave no way to express it.

use std::collections::BTreeMap;
use std::ffi::OsString;

use crate::flags::{FlagSource, MapSource};

/// A canonical grouped environment variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[allow(non_camel_case_types)]
pub enum Group {
    /// `CRATONVM_DBG` — tracing, dumps, extra verification. Class (a) in the
    /// census: no token here can change a program's result — **except for the
    /// three named below**, which can.
    ///
    /// The unqualified rule was already false before it was ever written down,
    /// so stating it plainly is worth more than an invariant nobody can rely
    /// on. The exceptions, all verified against their read sites:
    ///
    /// * `force-moving` (`CRATONVM_DBG_FORCE_MOVING`) — forces the moving
    ///   (Cheney) young collection even when quiescence says JIT frames are
    ///   live. `gc_quiescence.rs` records that it is the *only* thing that can
    ///   carry a cycle past `divert_non_moving`, and `gen_heap.rs` calls it
    ///   UNSAFE when a JIT frame genuinely is live, because it relocates
    ///   JIT-held raw pointers.
    /// * `sweep-zero` (`CRATONVM_DBG_SWEEP_ZERO`) — sets `retain_dead_objects`
    ///   in the young sweep, so the sweep stops collapsing adjacent dead
    ///   objects and keeps one record each. It changes what the collector
    ///   reclaims, not merely what it prints.
    /// * `nativelibraries-load-ok` (`CRATONVM_DBG_NATIVELIBRARIES_LOAD_OK`) —
    ///   restores an unconditional `System.loadLibrary` success, which flips
    ///   callers like Netty's `NativeLibraryLoader` out of their pure-Java
    ///   fallback.
    ///
    /// These are deliberately NOT renamed out of `DBG`. Moving them would leave
    /// the invariant just as broken with one fewer visible counterexample,
    /// which is the state that let the rule read as true for so long.
    ///
    /// Consequence for the generated docs: `render-inventory.py` derives the
    /// `Class` column from the group alone, so all three are labelled `diag`
    /// there. That is a known generator limitation with three instances, not a
    /// claim about any one of them.
    ///
    /// A *new* behaviour-changing knob should still go in the group that
    /// matches what it does — `COMPAT`, `GC`, `SECURITY` — rather than
    /// lengthening this list.
    DBG,
    /// `CRATONVM_JIT` — compiler passes, tiering, deopt, precise maps, shadow stack.
    JIT,
    /// `CRATONVM_GC` — collector selection, heap sizing, barriers, object layout.
    GC,
    /// `CRATONVM_REAL` — real JDK implementation vs synthetic Rust shim, per subsystem.
    REAL,
    /// `CRATONVM_LOADER` — class loading, resolution, verification.
    LOADER,
    /// `CRATONVM_IO` — files, sockets, HTTP, zip.
    IO,
    /// `CRATONVM_THREADS` — threading, async handoff, watchdog, lock order.
    THREADS,
    /// `CRATONVM_SECURITY` — sandboxing, trust anchors, policy.
    SECURITY,
    /// `CRATONVM_COMPAT` — per-application workarounds (JBoss, Spring, Quarkus).
    COMPAT,
    /// `CRATONVM_TEST` — soak/difftest harness knobs; never set in production.
    TEST,
}

impl Group {
    /// The environment variable name.
    pub const fn var(self) -> &'static str {
        match self {
            Group::DBG => "CRATONVM_DBG",
            Group::JIT => "CRATONVM_JIT",
            Group::GC => "CRATONVM_GC",
            Group::REAL => "CRATONVM_REAL",
            Group::LOADER => "CRATONVM_LOADER",
            Group::IO => "CRATONVM_IO",
            Group::THREADS => "CRATONVM_THREADS",
            Group::SECURITY => "CRATONVM_SECURITY",
            Group::COMPAT => "CRATONVM_COMPAT",
            Group::TEST => "CRATONVM_TEST",
        }
    }

    /// Every group, in declaration order.
    pub const ALL: &'static [Group] = &[
        Group::DBG,
        Group::JIT,
        Group::GC,
        Group::REAL,
        Group::LOADER,
        Group::IO,
        Group::THREADS,
        Group::SECURITY,
        Group::COMPAT,
        Group::TEST,
    ];
}

/// The scalar variables that keep their own name.
///
/// These are not knobs with an on/off sense — they are a path, a binary
/// location, a repository root, an assertion list — and folding them into a
/// comma-separated token list would only obscure them. `CRATONVM_DISABLE_JIT`
/// is here as the one exception: it is the master JIT switch, it appears in
/// 190 places across docs and CI, and a master switch is worth its own name.
pub const SCALARS: &[&str] = &[
    "CRATONVM_JAVA_HOME",
    "CRATONVM_BIN",
    "CRATONVM_MAVEN_REPO_LOCAL",
    "CRATONVM_ENABLE_ASSERTIONS",
    "CRATONVM_DISABLE_JIT",
    // B10 (2026-09-01): a comma-separated list of JFR event NAMES to arm on the
    // boot recording. A scalar rather than an `INVENTORY` token because it
    // carries a value, and the `E` model is presence-only. It exists because
    // `Vm::new` is also entered by embedders that never see argv, and because
    // the `-XX:StartFlightRecording:+<Event>#enabled=true` spelling is parsed in
    // `vm-cli`, which an embedder does not link.
    "CRATONVM_JFR_ENABLE_EVENTS",
    // D3 (2026-09-01): override the host-derived `native.encoding`. A scalar,
    // not an `E` row, because it carries a VALUE and the `E` model is
    // presence-only. Unset = derived from the LC_ALL/LC_CTYPE/LANG chain the
    // JDK itself uses; `=UTF-8` restores the constant this VM used to hard-code
    // under a comment wrongly claiming JEP 400 required it (JEP 400 pinned
    // `file.encoding` only, and `System.java` specifies `native.encoding` as
    // host-derived and command-line-immune).
    "CRATONVM_NATIVE_ENCODING",
    // 2026-09-01: pin the three stream encodings — `stdout.encoding`,
    // `stderr.encoding`, `stdin.encoding` — and with them the `Charset`
    // `install_charset` stamps on `System.out`/`System.err`, which is what
    // decides the BYTES. Scalar for the same reason as the row above: it
    // carries a VALUE. Unset = derived from the host (the locale codeset on
    // Unix, the console/ANSI code page on Windows); `=UTF-8` restores the
    // constant this VM hard-coded before that date, in one binary, which is
    // the A/B `stdout-encoding-differs-from-hotspot-on-windows-20260901.md`
    // §9 asked for when it recommended following the host.
    "CRATONVM_STDOUT_ENCODING",
];

/// One knob: a token in a group, and the legacy key(s) it expands to.
#[derive(Debug, Clone, Copy)]
pub struct E {
    /// Which grouped variable this token belongs to.
    pub group: Group,
    /// The canonical token, lower-case, `-`-separated. Always stated
    /// **positively**: `bce`, never `no-bce`.
    pub token: &'static str,
    /// Legacy key set when the token is enabled. `None` for knobs that only
    /// ever had an opt-out spelling (`CRATONVM_JIT_NO_BCE` and friends) — for
    /// those, enabling the token means *removing* `off_key`.
    pub on_key: Option<&'static str>,
    /// Legacy key set when the token is disabled.
    pub off_key: Option<&'static str>,
    /// For a default-ON knob with no `off_key`: the value that its parser reads
    /// as false. `None` means "off is expressed by unsetting `on_key`".
    pub off_word: Option<&'static str>,
    /// ISO `YYYY-MM-DD`: when this knob's *name* first entered the tree.
    ///
    /// Not when the row was written. Most rows were written on 2026-07-26, the
    /// day this table was created, for names that had already existed for
    /// months; dating a row by its own line's birthday would say the whole
    /// surface is six weeks old and make the field useless for the one job it
    /// has. The checked-in dates are the earlier of "the row appeared in this
    /// file" and "the name appeared anywhere in the repository", both taken
    /// from `git log`:
    ///
    /// ```text
    /// git log --format='@@@%ad' --date=short -U0 -G'CRATONVM_[A-Z_0-9]'
    ///   | grep -E '^@@@|^[+].*CRATONVM_'    # keep the dates and the added lines
    /// ```
    ///
    /// # Why the table needs a date at all
    ///
    /// The census that motivated the grouping recorded that flags accumulate
    /// "at roughly one per fixed bug with no retirement path". Grouping was a
    /// renaming: 991 declared knobs behind 15 environment variables is still
    /// 991 knobs, and nothing in this repository has ever removed one. A knob
    /// cannot be retired without an answer to "how long has it been here and
    /// does anything still use it", and until this field existed the table
    /// could answer neither.
    ///
    /// [`tests::every_row_states_when_it_arrived`] enforces the format, and
    /// [`tests::a_dbg_knob_declared_since_the_horizon_has_a_live_consumer`]
    /// enforces the policy. `docs/config/flag-retirement-candidates-20260901.md`
    /// is the backlog those two tests are meant to stop growing.
    ///
    /// A new row states today's date. It is not a guess: if you cannot say when
    /// the name arrived, it arrived now.
    pub since: &'static str,
}

/// Every knob in the VM, exactly once.
///
/// Generated from the census scan; `types/tests/flag_surface.rs` asserts it
/// stays complete and `tools/flag-census/check-surface.sh` asserts the code and
/// the reference docs agree with it.
///
/// # Adding a `CRATONVM_*` flag: the four files, all of them
///
/// The enforcing tests are `cargo test` assertions, not compile errors, so
/// `cargo build --all-targets` is green while any of these is missing. Editing
/// two of the four and stopping is how this has gone red before.
///
/// 1. `types/src/flag_groups.rs` — an [`E`] row here (or a [`SCALARS`] entry).
///    This is the only file that makes a name *declared*. The row carries a
///    [`since`](E::since) date; for a new knob that is today's date, and for a
///    `DBG` knob it is also what puts the row under the live-consumer rule in
///    [`tests::a_dbg_knob_declared_since_the_horizon_has_a_live_consumer`].
/// 2. `types/tests/flag-surface.txt` — the name, in sort order. Compared
///    byte-for-byte, so match the file's existing line endings.
/// 3. `docs/flag-tokens.md` — a `` | `token` | `KEY` | `` row in the group's
///    section, and that section's "N tokens." count.
/// 4. `docs/config/flag-inventory.md` — a Full-inventory row, the
///    "N rows: D declared, A allowlisted." header, and the **declared** count
///    in "Where the surface stands".
///
/// Enforced by `types/tests/flag_declaration_guard.rs` (a literal with no
/// declaration), `flag_surface.rs` (1 vs 2, both directions) and
/// `flag_docs_generated.rs` (1 vs 3 and 4, both directions). Files 3 and 4 are
/// generated — `tools/flag-census/render-tokens.sh` and `render-inventory.py`
/// write them from this table, and running them beats hand-editing.
///
/// A `CRATONVM_*` name is not free-standing in either direction: a literal with
/// no row fails the guard, and a row with no read site fails check 5 of
/// `check-surface.sh`. Land the declaration and its consumer together.
///
/// Declaring a name is also what routes it through the latched snapshot, so the
/// read site must use `flags::runtime_var[_os]` — a raw `std::env::var` on a
/// declared name additionally trips check 4 of `check-surface.sh`. No
/// `flags.rs` field is needed: `VmFlags::legacy_var_os` serves every declared
/// name from one map.
///
/// One row per line, deliberately. rustfmt would break each entry across five
/// lines, turning a 541-line table that `grep` can answer questions about into
/// a 2700-line one that it cannot.
#[rustfmt::skip]
pub const INVENTORY: &[E] = &[
    E { group: Group::DBG, token: "a2", on_key: Some("CRATONVM_DBG_A2"), off_key: None, off_word: None, since: "2026-06-22" },
    // B6: one line per compiled aastore SITE naming the gate verdict, so a
    // zero in the census is readable as "consulted and correctly declined"
    // rather than as an instrument armed where it cannot fire.
    E { group: Group::DBG, token: "aastore-barrier-gate-sites", on_key: Some("CRATONVM_DBG_AASTORE_BARRIER_GATE"), off_key: None, off_word: None, since: "2026-09-01" },
    E { group: Group::DBG, token: "a5-census", on_key: Some("CRATONVM_DBG_A5_CENSUS"), off_key: None, off_word: None, since: "2026-08-11" },
    E { group: Group::DBG, token: "a5-fallback", on_key: Some("CRATONVM_DBG_A5_FALLBACK"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::DBG, token: "ffm", on_key: Some("CRATONVM_DBG_FFM"), off_key: None, off_word: None, since: "2026-08-26" },
    E { group: Group::DBG, token: "sweep-liveness", on_key: Some("CRATONVM_DBG_SWEEP_LIVENESS"), off_key: None, off_word: None, since: "2026-07-31" },
    // Declared 2026-08-06: these nine were read by `runtime_var`/`runtime_var_os`
    // but named nowhere, so each was served by a live `getenv` instead of the
    // latched `VmFlags` snapshot -- `CRATONVM_DBG=token` could not reach them
    // and `flags::with_thread_overrides` could not arrange one in a test.
    E { group: Group::DBG, token: "callee-deopt", on_key: Some("CRATONVM_DBG_CALLEE_DEOPT"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::DBG, token: "layout-alias", on_key: Some("CRATONVM_DBG_LAYOUT_ALIAS"), off_key: None, off_word: None, since: "2026-08-06" },
    E { group: Group::DBG, token: "check-override", on_key: Some("CRATONVM_DBG_CHECK_OVERRIDE"), off_key: None, off_word: None, since: "2026-08-06" },
    // The enforcement dial's per-door census. The TOTALS print on any armed
    // run without this; the token adds the per-triple rows.
    E { group: Group::DBG, token: "dial-doors", on_key: Some("CRATONVM_DBG_DIAL_DOORS"), off_key: None, off_word: None, since: "2026-08-21" },
    E { group: Group::DBG, token: "direct-memory", on_key: Some("CRATONVM_DBG_DM"), off_key: None, off_word: None, since: "2026-08-06" },
    E { group: Group::DBG, token: "dupx-trace", on_key: Some("CRATONVM_DBG_DUPX_TRACE"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::DBG, token: "watch-pun", on_key: Some("CRATONVM_DBG_WATCH_PUN"), off_key: None, off_word: None, since: "2026-08-24" },
    // Name the receiver, slot and compiled method for a field store the JIT
    // helper family discarded (implausible receiver, or slot past num_slots).
    // The counters are always on; this is the per-event dump.
    E { group: Group::DBG, token: "dropped-putfield", on_key: Some("CRATONVM_DBG_DROPPED_PUTFIELD"), off_key: None, off_word: None, since: "2026-08-27" },
    E { group: Group::DBG, token: "read0-latency", on_key: Some("CRATONVM_DBG_READ0LAT"), off_key: None, off_word: None, since: "2026-08-06" },
    E { group: Group::DBG, token: "refdisc", on_key: Some("CRATONVM_DBG_REFDISC"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::DBG, token: "site-alias", on_key: Some("CRATONVM_DBG_SITE_ALIAS"), off_key: None, off_word: None, since: "2026-08-06" },
    E { group: Group::DBG, token: "sp-ic-sites", on_key: Some("CRATONVM_DBG_SP_IC_SITES"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::DBG, token: "stub-yield", on_key: Some("CRATONVM_DBG_STUB_YIELD"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::DBG, token: "access", on_key: Some("CRATONVM_DBG_ACCESS"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "active-profiles-identity-trace", on_key: Some("CRATONVM_ACTIVE_PROFILES_IDENTITY_TRACE"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "aio", on_key: Some("CRATONVM_DBG_AIO"), off_key: None, off_word: None, since: "2026-06-23" },
    E { group: Group::DBG, token: "aio-inline", on_key: Some("CRATONVM_DBG_AIO_INLINE"), off_key: None, off_word: None, since: "2026-07-30" },
    E { group: Group::DBG, token: "aioobe", on_key: Some("CRATONVM_DBG_AIOOBE"), off_key: None, off_word: None, since: "2026-05-25" },
    E { group: Group::DBG, token: "aioobe2", on_key: Some("CRATONVM_DBG_AIOOBE2"), off_key: None, off_word: None, since: "2026-06-04" },
    E { group: Group::DBG, token: "aioobe3", on_key: Some("CRATONVM_DBG_AIOOBE3"), off_key: None, off_word: None, since: "2026-07-24" },
    E { group: Group::DBG, token: "altrace", on_key: Some("CRATONVM_DBG_ALTRACE"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "ann-proxy-dispatch-trace", on_key: Some("CRATONVM_ANN_PROXY_DISPATCH_TRACE"), off_key: None, off_word: None, since: "2026-07-14" },
    // Per-phase timing for `annotation_proxy_dispatch_impl` (total / walk /
    // flagread / namecmp), printed every 100k dispatches. Reading one annotation
    // attribute costs ~10.5 us against ~10 ns on HotSpot; this splits it.
    E { group: Group::DBG, token: "ann-proxy-prof", on_key: Some("CRATONVM_DBG_ANN_PROXY_PROF"), off_key: None, off_word: None, since: "2026-08-10" },
    E { group: Group::DBG, token: "ann-trace", on_key: Some("CRATONVM_ANN_TRACE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "annproxy-wrap", on_key: Some("CRATONVM_DBG_ANNPROXY_WRAP"), off_key: None, off_word: None, since: "2026-07-04" },
    E { group: Group::DBG, token: "anonalloc", on_key: Some("CRATONVM_DBG_ANONALLOC"), off_key: None, off_word: None, since: "2026-05-21" },
    E { group: Group::DBG, token: "aqs-trace", on_key: Some("CRATONVM_DBG_AQS_TRACE"), off_key: None, off_word: None, since: "2026-07-24" },
    E { group: Group::DBG, token: "args", on_key: Some("CRATONVM_DBG_ARGS"), off_key: None, off_word: None, since: "2026-05-29" },
    E { group: Group::DBG, token: "arraycopy", on_key: Some("CRATONVM_DBG_ARRAYCOPY"), off_key: None, off_word: None, since: "2026-06-15" },
    E { group: Group::DBG, token: "arrlen", on_key: Some("CRATONVM_DBG_ARRLEN"), off_key: None, off_word: None, since: "2026-06-04" },
    E { group: Group::DBG, token: "arrstore", on_key: Some("CRATONVM_DBG_ARRSTORE"), off_key: None, off_word: None, since: "2026-06-09" },
    E { group: Group::DBG, token: "asserteq", on_key: Some("CRATONVM_DBG_ASSERTEQ"), off_key: None, off_word: None, since: "2026-05-29" },
    E { group: Group::DBG, token: "assertj-arr", on_key: Some("CRATONVM_DBG_ASSERTJ_ARR"), off_key: None, off_word: None, since: "2026-07-24" },
    E { group: Group::DBG, token: "athrow", on_key: Some("CRATONVM_DBG_ATHROW"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "atomic-updater", on_key: Some("CRATONVM_DBG_ATOMIC_UPDATER"), off_key: None, off_word: None, since: "2026-07-09" },
    E { group: Group::DBG, token: "badrecv", on_key: Some("CRATONVM_DBG_BADRECV"), off_key: None, off_word: None, since: "2026-06-05" },
    E { group: Group::DBG, token: "badref", on_key: Some("CRATONVM_DBG_BADREF"), off_key: None, off_word: None, since: "2026-06-05" },
    E { group: Group::DBG, token: "bb", on_key: Some("CRATONVM_DBG_BB"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "bblp", on_key: Some("CRATONVM_DBG_BBLP"), off_key: None, off_word: None, since: "2026-05-24" },
    E { group: Group::DBG, token: "bd-debug", on_key: Some("CRATONVM_BD_DEBUG"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "blocked-access", on_key: Some("CRATONVM_DBG_BLOCKED_ACCESS"), off_key: None, off_word: None, since: "2026-07-15" },
    E { group: Group::DBG, token: "blockgc", on_key: Some("CRATONVM_DBG_BLOCKGC"), off_key: None, off_word: None, since: "2026-06-10" },
    E { group: Group::DBG, token: "root-remap-audit", on_key: Some("CRATONVM_DBG_ROOT_REMAP_AUDIT"), off_key: None, off_word: None, since: "2026-08-17" },
    E { group: Group::DBG, token: "bufunder", on_key: Some("CRATONVM_DBG_BUFUNDER"), off_key: None, off_word: None, since: "2026-07-10" },
    E { group: Group::DBG, token: "bug03", on_key: Some("CRATONVM_DBG_BUG03"), off_key: None, off_word: None, since: "2026-06-29" },
    E { group: Group::DBG, token: "bytecode-dump", on_key: Some("CRATONVM_DBG_BYTECODE_DUMP"), off_key: None, off_word: None, since: "2026-07-15" },
    E { group: Group::DBG, token: "callee-probe", on_key: Some("CRATONVM_DBG_CALLEE_PROBE"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::DBG, token: "caller", on_key: Some("CRATONVM_DBG_CALLER"), off_key: None, off_word: None, since: "2026-06-04" },
    E { group: Group::DBG, token: "capval", on_key: Some("CRATONVM_DBG_CAPVAL"), off_key: None, off_word: None, since: "2026-06-10" },
    E { group: Group::DBG, token: "catalina", on_key: Some("CRATONVM_DBG_CATALINA"), off_key: None, off_word: None, since: "2026-05-21" },
    E { group: Group::DBG, token: "cause", on_key: Some("CRATONVM_DBG_CAUSE"), off_key: None, off_word: None, since: "2026-07-10" },
    // The native-invocation census. Already read through `flags::runtime_var_os`
    // in `vm_exec.rs`; it was simply never declared, so `CRATONVM_DBG=census-    // exact-invocations` could not reach it.
    E { group: Group::DBG, token: "census-exact-invocations", on_key: Some("CRATONVM_CENSUS_EXACT_INVOCATIONS"), off_key: None, off_word: None, since: "2026-08-17" },
    E { group: Group::DBG, token: "cce", on_key: Some("CRATONVM_DBG_CCE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "cce-bt", on_key: Some("CRATONVM_DBG_CCE_BT"), off_key: None, off_word: None, since: "2026-07-22" },
    // Declared 2026-08-22 with the fix it was written for. When a VM-side field
    // read decodes a `Value` cell with an out-of-range discriminant, name the
    // RECEIVER and the Java frames that reached it. The collector's own guard
    // reports the CELL and can report nothing else -- it runs in the collector
    // crate and cannot see a frame -- which is why the producer behind
    // `corrupt-value-cell-is-fatal-on-three-of-four-collectors` stayed open for
    // two days. Costs one relaxed load per `NativeContext::get_field` while
    // armed and nothing at all while it is not.
    E { group: Group::DBG, token: "coll-refresh", on_key: Some("CRATONVM_DBG_COLL_REFRESH"), off_key: None, off_word: None, since: "2026-08-24" },
    E { group: Group::DBG, token: "corrupt-cell", on_key: Some("CRATONVM_DBG_CORRUPT_CELL"), off_key: None, off_word: None, since: "2026-08-22" },
    // Declared 2026-08-23 with the widening it verifies. Fabricates one
    // corrupt-cell hit at a site that HAS a read door and one at a site that
    // does not, so a run can prove the door reporter and the safepoint backstop
    // both actually speak. A silent diagnostic and a broken one look identical
    // from the outside, and this instrument was read the wrong way round once
    // already -- it reported nothing through a Spring Boot sweep that had
    // tripped the collector's guard, and the silence was taken for "clean".
    E { group: Group::DBG, token: "corrupt-cell-selftest", on_key: Some("CRATONVM_DBG_CORRUPT_CELL_SELFTEST"), off_key: None, off_word: None, since: "2026-08-23" },
    E { group: Group::DBG, token: "ccecache", on_key: Some("CRATONVM_DBG_CCECACHE"), off_key: None, off_word: None, since: "2026-07-24" },
    E { group: Group::DBG, token: "ccsprobe", on_key: Some("CRATONVM_DBG_CCSPROBE"), off_key: None, off_word: None, since: "2026-05-24" },
    // The per-occurrence, backtrace-carrying arm of the descriptor-coercion
    // guard. The `gc::guard` WARN text tells operators to set this by name, so
    // an undeclared spelling of it was a live diagnostic that the grouped form
    // could not reach.
    E { group: Group::DBG, token: "coercion", on_key: Some("CRATONVM_DBG_COERCION"), off_key: None, off_word: None, since: "2026-08-17" },
    E { group: Group::DBG, token: "cellcorrupt", on_key: Some("CRATONVM_DBG_CELLCORRUPT"), off_key: None, off_word: None, since: "2026-07-03" },
    E { group: Group::DBG, token: "charset", on_key: Some("CRATONVM_DBG_CHARSET"), off_key: None, off_word: None, since: "2026-05-21" },
    E { group: Group::DBG, token: "class-resource", on_key: Some("CRATONVM_DBG_CLASS_RESOURCE"), off_key: None, off_word: None, since: "2026-07-29" },
    E { group: Group::DBG, token: "classpath", on_key: Some("CRATONVM_DBG_CLASSPATH"), off_key: None, off_word: None, since: "2026-07-22" },
    E { group: Group::DBG, token: "cleaners", on_key: None, off_key: Some("CRATONVM_DBG_NO_CLEANERS"), off_word: None, since: "2026-06-09" },
    E { group: Group::DBG, token: "clinit-fail", on_key: Some("CRATONVM_DBG_CLINIT_FAIL"), off_key: None, off_word: None, since: "2026-07-28" },
    E { group: Group::DBG, token: "clinit-order", on_key: Some("CRATONVM_DBG_CLINIT_ORDER"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::DBG, token: "clone", on_key: Some("CRATONVM_DBG_CLONE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "coerce", on_key: Some("CRATONVM_DBG_COERCE"), off_key: None, off_word: None, since: "2026-07-22" },
    E { group: Group::DBG, token: "checkcast-inline", on_key: Some("CRATONVM_DBG_CHECKCAST_INLINE"), off_key: None, off_word: None, since: "2026-08-28" },
    E { group: Group::DBG, token: "compact-inline", on_key: Some("CRATONVM_DBG_COMPACT_INLINE"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::DBG, token: "compact-legacy", on_key: Some("CRATONVM_DBG_COMPACT_LEGACY"), off_key: None, off_word: None, since: "2026-06-30" },
    E { group: Group::DBG, token: "compactvalue", on_key: Some("CRATONVM_DBG_COMPACTVALUE"), off_key: None, off_word: None, since: "2026-06-16" },
    E { group: Group::DBG, token: "component-type", on_key: Some("CRATONVM_DBG_COMPONENT_TYPE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "corrupt-frames", on_key: Some("CRATONVM_DBG_CORRUPT_FRAMES"), off_key: None, off_word: None, since: "2026-06-03" },
    E { group: Group::DBG, token: "ctor-fix", on_key: Some("CRATONVM_DBG_CTOR_FIX"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::DBG, token: "dbb-elem", on_key: Some("CRATONVM_DBG_DBB_ELEM"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::DBG, token: "debug-sfi", on_key: Some("CRATONVM_DEBUG_SFI"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "debug-stack-tag", on_key: Some("CRATONVM_DEBUG_STACK_TAG"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "debug-stackwalk", on_key: Some("CRATONVM_DEBUG_STACKWALK"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "define", on_key: Some("CRATONVM_DBG_DEFINE"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "define-census", on_key: Some("CRATONVM_DBG_DEFINE_CENSUS"), off_key: None, off_word: None, since: "2026-08-11" },
    E { group: Group::DBG, token: "deflate", on_key: Some("CRATONVM_DBG_DEFLATE"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "deopt", on_key: Some("CRATONVM_DBG_DEOPT"), off_key: None, off_word: None, since: "2026-06-11" },
    E { group: Group::DBG, token: "deopt-eager", on_key: Some("CRATONVM_DEOPT_EAGER"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::DBG, token: "deopt-eager-bci", on_key: Some("CRATONVM_DEOPT_EAGER_BCI"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::DBG, token: "deopt-verify", on_key: Some("CRATONVM_DEOPT_VERIFY"), off_key: None, off_word: None, since: "2026-06-18" },
    E { group: Group::DBG, token: "deoptslot", on_key: Some("CRATONVM_DBG_DEOPTSLOT"), off_key: None, off_word: None, since: "2026-07-31" },
    // Default-ON: the launcher prints one line when it sees a legacy per-flag
    // variable set directly. `CRATONVM_DBG=-deprecations` silences it.
    E { group: Group::DBG, token: "deprecations", on_key: None, off_key: Some("CRATONVM_QUIET_DEPRECATIONS"), off_word: None, since: "2026-07-26" },
    E { group: Group::DBG, token: "desctrace", on_key: Some("CRATONVM_DBG_DESCTRACE"), off_key: None, off_word: None, since: "2026-07-11" },
    E { group: Group::DBG, token: "diag-hib32", on_key: Some("CRATONVM_DIAG_HIB32"), off_key: None, off_word: None, since: "2026-06-23" },
    E { group: Group::DBG, token: "diag-jar-list", on_key: Some("CRATONVM_DIAG_JAR_LIST"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "diag-jboss-services", on_key: Some("CRATONVM_DIAG_JBOSS_SERVICES"), off_key: None, off_word: None, since: "2026-07-05" },
    E { group: Group::DBG, token: "diag-jca", on_key: Some("CRATONVM_DIAG_JCA"), off_key: None, off_word: None, since: "2026-06-02" },
    E { group: Group::DBG, token: "diag-method-invoke-null", on_key: Some("CRATONVM_DIAG_METHOD_INVOKE_NULL"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "diag-properties", on_key: Some("CRATONVM_DIAG_PROPERTIES"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::REAL, token: "props-unrooted-receivers", on_key: Some("CRATONVM_PROPS_UNROOTED_RECEIVERS"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::DBG, token: "diag-serviceloader", on_key: Some("CRATONVM_DIAG_SERVICELOADER"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "dispatch-tally", on_key: Some("CRATONVM_DBG_DISPATCH_TALLY"), off_key: None, off_word: None, since: "2026-07-28" },
    E { group: Group::DBG, token: "dopriv", on_key: Some("CRATONVM_DBG_DOPRIV"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "dropped-stubs", on_key: Some("CRATONVM_DBG_DROPPED_STUBS"), off_key: None, off_word: None, since: "2026-07-14" },
    // Names every "already defined" defineClass backend error and the verdict
    // `classify_duplicate_define` gave it. Declared rather than left to a live
    // getenv so `CRATONVM_DBG=dupdef` reaches it and a test can arrange it:
    // both arms print, which is how a probe proves it exercised the one it claims.
    E { group: Group::DBG, token: "dupdef", on_key: Some("CRATONVM_DBG_DUPDEF"), off_key: None, off_word: None, since: "2026-08-11" },
    E { group: Group::DBG, token: "dump-jit", on_key: Some("CRATONVM_DBG_DUMP_JIT"), off_key: None, off_word: None, since: "2026-06-03" },
    E { group: Group::DBG, token: "dupcall-filter", on_key: Some("CRATONVM_DBG_DUPCALL_FILTER"), off_key: None, off_word: None, since: "2026-07-23" },
    E { group: Group::DBG, token: "dupclass", on_key: Some("CRATONVM_DBG_DUPCLASS"), off_key: None, off_word: None, since: "2026-07-22" },
    E { group: Group::DBG, token: "dupclass-bt", on_key: Some("CRATONVM_DBG_DUPCLASS_BT"), off_key: None, off_word: None, since: "2026-07-22" },
    E { group: Group::DBG, token: "dupclass-filter", on_key: Some("CRATONVM_DBG_DUPCLASS_FILTER"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::DBG, token: "typecheck-filter", on_key: Some("CRATONVM_DBG_TYPECHECK_FILTER"), off_key: None, off_word: None, since: "2026-08-10" },
    E { group: Group::DBG, token: "dupx-methods", on_key: Some("CRATONVM_DBG_DUPX_METHODS"), off_key: None, off_word: None, since: "2026-06-16" },
    E { group: Group::DBG, token: "ecwatch", on_key: Some("CRATONVM_DBG_ECWATCH"), off_key: None, off_word: None, since: "2026-06-05" },
    E { group: Group::DBG, token: "ecwatch-native", on_key: Some("CRATONVM_DBG_ECWATCH_NATIVE"), off_key: None, off_word: None, since: "2026-06-05" },
    E { group: Group::DBG, token: "eintr-inject", on_key: Some("CRATONVM_DBG_EINTR_INJECT"), off_key: None, off_word: None, since: "2026-08-06" },
    E { group: Group::DBG, token: "eintr-no-retry", on_key: Some("CRATONVM_DBG_EINTR_NO_RETRY"), off_key: None, off_word: None, since: "2026-08-06" },
    E { group: Group::DBG, token: "enable-native-ring", on_key: Some("CRATONVM_ENABLE_NATIVE_RING"), off_key: None, off_word: None, since: "2026-05-29" },
    E { group: Group::DBG, token: "eqe", on_key: Some("CRATONVM_DBG_EQE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "excframe", on_key: Some("CRATONVM_DBG_EXCFRAME"), off_key: None, off_word: None, since: "2026-07-28" },
    E { group: Group::DBG, token: "exec", on_key: Some("CRATONVM_DBG_EXEC"), off_key: None, off_word: None, since: "2026-06-17" },
    E { group: Group::DBG, token: "exec-frame-trace", on_key: Some("CRATONVM_EXEC_FRAME_TRACE"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "exit", on_key: Some("CRATONVM_DBG_EXIT"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "fbcglib", on_key: Some("CRATONVM_DBG_FBCGLIB"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "fbref", on_key: Some("CRATONVM_DBG_FBREF"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "fc-fast-io-stats", on_key: Some("CRATONVM_FC_FAST_IO_STATS"), off_key: None, off_word: None, since: "2026-08-23" },
    E { group: Group::DBG, token: "field-get", on_key: Some("CRATONVM_DBG_FIELD_GET"), off_key: None, off_word: None, since: "2026-07-10" },
    // The socket transfer / selector engagement census
    // (`native-io::socket_fast_io::stats`). Diagnostic only: it prints counters
    // at exit and changes nothing a program can observe.
    E { group: Group::DBG, token: "sc-io-stats", on_key: Some("CRATONVM_SC_IO_STATS"), off_key: None, off_word: None, since: "2026-09-04" },
    E { group: Group::DBG, token: "field-watch", on_key: Some("CRATONVM_DBG_FIELD_WATCH"), off_key: None, off_word: None, since: "2026-07-23" },
    E { group: Group::DBG, token: "fieldaddr", on_key: Some("CRATONVM_DBG_FIELDADDR"), off_key: None, off_word: None, since: "2026-06-11" },
    E { group: Group::DBG, token: "force-moving", on_key: Some("CRATONVM_DBG_FORCE_MOVING"), off_key: None, off_word: None, since: "2026-06-03" },
    E { group: Group::DBG, token: "forname-trace", on_key: Some("CRATONVM_FORNAME_TRACE"), off_key: None, off_word: None, since: "2026-07-22" },
    E { group: Group::DBG, token: "frame-trace", on_key: Some("CRATONVM_FRAME_TRACE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "fsp", on_key: Some("CRATONVM_DBG_FSP"), off_key: None, off_word: None, since: "2026-06-12" },
    E { group: Group::DBG, token: "fullstack-scan", on_key: Some("CRATONVM_DBG_FULLSTACK_SCAN"), off_key: None, off_word: None, since: "2026-06-03" },
    E { group: Group::DBG, token: "fwdwalk", on_key: Some("CRATONVM_DBG_FWDWALK"), off_key: None, off_word: None, since: "2026-08-07" },
    E { group: Group::DBG, token: "fwdguard", on_key: Some("CRATONVM_DBG_FWDGUARD"), off_key: None, off_word: None, since: "2026-06-09" },
    E { group: Group::DBG, token: "g1-dbg-gray-prov", on_key: Some("CRATONVM_G1_DBG_GRAY_PROV"), off_key: None, off_word: None, since: "2026-08-30" },
    E { group: Group::GC, token: "g1-late-header-write", on_key: Some("CRATONVM_G1_LATE_HEADER_WRITE"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::GC, token: "g1-mark-oob-failsafe", on_key: Some("CRATONVM_G1_MARK_OOB_FAILSAFE"), off_key: None, off_word: None, since: "2026-08-30" },
    E { group: Group::DBG, token: "g1-dbg-headers", on_key: Some("CRATONVM_G1_DBG_HEADERS"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::DBG, token: "g1-dbg-pins", on_key: Some("CRATONVM_G1_DBG_PINS"), off_key: None, off_word: None, since: "2026-07-10" },
    E { group: Group::DBG, token: "g1-dbg-reach", on_key: Some("CRATONVM_G1_DBG_REACH"), off_key: None, off_word: None, since: "2026-07-03" },
    E { group: Group::DBG, token: "g1-dbg-rootcensus", on_key: Some("CRATONVM_G1_DBG_ROOTCENSUS"), off_key: None, off_word: None, since: "2026-07-03" },
    E { group: Group::DBG, token: "gdm-prof", on_key: Some("CRATONVM_DBG_GDM_PROF"), off_key: None, off_word: None, since: "2026-08-07" },
    E { group: Group::DBG, token: "g1-dbg-zero", on_key: Some("CRATONVM_G1_DBG_ZERO"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::DBG, token: "g1diag", on_key: Some("CRATONVM_DBG_G1DIAG"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "g1accessor", on_key: Some("CRATONVM_DBG_G1ACCESSOR"), off_key: None, off_word: None, since: "2026-08-17" },
    E { group: Group::DBG, token: "gc-array-guard-bt", on_key: Some("CRATONVM_GC_ARRAY_GUARD_BT"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "gc-fallback-reasons", on_key: Some("CRATONVM_DBG_GC_FALLBACK_REASONS"), off_key: None, off_word: None, since: "2026-08-10" },
    E { group: Group::DBG, token: "gc-overhead", on_key: Some("CRATONVM_DBG_GC_OVERHEAD"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::DBG, token: "gc-stats", on_key: Some("CRATONVM_GC_STATS"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::DBG, token: "gc-stress", on_key: Some("CRATONVM_DBG_GC_STRESS"), off_key: None, off_word: None, since: "2026-06-03" },
    E { group: Group::DBG, token: "oop-oracle-force-refute", on_key: Some("CRATONVM_DBG_OOP_ORACLE_FORCE_REFUTE"), off_key: None, off_word: None, since: "2026-08-22" },
    E { group: Group::DBG, token: "gc-verify-stale", on_key: Some("CRATONVM_GC_VERIFY_STALE"), off_key: None, off_word: None, since: "2026-05-23" },
    E { group: Group::GC, token: "late-resolve-dropped", on_key: Some("CRATONVM_GC_LATE_RESOLVE_DROPPED"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::GC, token: "tlab-skip", on_key: None, off_key: Some("CRATONVM_GC_NO_TLAB_SKIP"), off_word: None, since: "2026-09-06" },
    E { group: Group::GC, token: "g1-only-jit-pins", on_key: Some("CRATONVM_GC_G1_ONLY_JIT_PINS"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::GC, token: "peer-pin-divert", on_key: None, off_key: Some("CRATONVM_GC_NO_PEER_PIN_DIVERT"), off_word: None, since: "2026-09-06" },
    E { group: Group::GC, token: "conditional-tlab-skip-publish", on_key: Some("CRATONVM_GC_CONDITIONAL_TLAB_SKIP_PUBLISH"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::GC, token: "frame-trace-span-retire", on_key: None, off_key: Some("CRATONVM_GC_NO_FRAME_TRACE_SPAN_RETIRE"), off_word: None, since: "2026-09-06" },
    E { group: Group::DBG, token: "peer-reg-pairing", on_key: Some("CRATONVM_DBG_PEER_REG_PAIRING"), off_key: None, off_word: None, since: "2026-09-06" },
    // Coverage oracle for the slot list `static-root-slots` builds: after the
    // fast path has patched the recorded slots, re-walk every static the slow
    // way and report any that still names a moved address. A miss there is not
    // a wrong number, it is a live static field left pointing at a vacated
    // address, and a unit test cannot answer the question on a real class set.
    E { group: Group::DBG, token: "static-slot-verify", on_key: Some("CRATONVM_DBG_STATIC_SLOT_VERIFY"), off_key: None, off_word: None, since: "2026-09-05" },
    // The GC-trigger publish's coverage oracle: read the lock-free triple, then
    // take the lock and assert the arena agrees. Under the lock the two MUST be
    // equal -- every writer holds that mutex and the guard republishes before
    // releasing it -- so a divergence is a mutation that reached the arena
    // without passing through `YoungFromGuard::drop`, which is the only way the
    // design can be wrong.
    E { group: Group::DBG, token: "gc-trigger-verify", on_key: Some("CRATONVM_DBG_GC_TRIGGER_VERIFY"), off_key: None, off_word: None, since: "2026-09-06" },
    // `MarkBitmap::clear`: calls, calls that actually cleared, words, nanos.
    // The pair `calls`/`worked` is the engagement half -- the thing being
    // measured is the `any_marked` early return, so `worked == calls` says it
    // never fired and the reading is about the clear loop instead.
    E { group: Group::DBG, token: "markclear", on_key: Some("CRATONVM_DBG_MARKCLEAR"), off_key: None, off_word: None, since: "2026-09-06" },
    // G1's `is_object_address`: calls, acceptances, nanos. The question it
    // answers is whether G1 should get the exact object-start bitmap the
    // generational predicate got, and the answer is a share of a run.
    E { group: Group::DBG, token: "g1-objaddr", on_key: Some("CRATONVM_DBG_G1_OBJADDR"), off_key: None, off_word: None, since: "2026-09-06" },
    // `update_all_roots`: calls, pointer-map entries, nanos. What the
    // value-carrying root ABI costs per run -- `rootprof` prints only when one
    // call exceeds 20 ms, which cannot see a thousand 1 ms fix-ups.
    E { group: Group::DBG, token: "rootfixup", on_key: Some("CRATONVM_DBG_ROOTFIXUP"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::DBG, token: "gcpart", on_key: Some("CRATONVM_DBG_GCPART"), off_key: None, off_word: None, since: "2026-07-22" },
    E { group: Group::DBG, token: "jni-localref", on_key: Some("CRATONVM_DBG_JNI_LOCALREF"), off_key: None, off_word: None, since: "2026-08-26" },
    E { group: Group::DBG, token: "gcpause", on_key: Some("CRATONVM_DBG_GCPAUSE"), off_key: None, off_word: None, since: "2026-07-07" },
    E { group: Group::DBG, token: "gcphase", on_key: Some("CRATONVM_DBG_GCPHASE"), off_key: None, off_word: None, since: "2026-07-14" },
    E { group: Group::DBG, token: "gcwrite", on_key: Some("CRATONVM_DBG_GCWRITE"), off_key: None, off_word: None, since: "2026-06-05" },
    E { group: Group::DBG, token: "getfield-receivers", on_key: Some("CRATONVM_DBG_GETFIELD_RECEIVERS"), off_key: None, off_word: None, since: "2026-08-18" },
    // The PER-SITE half of the JIT reference-load census: which of the five
    // read-helper arms served each reference-slot read. Off by default because
    // one ungated relaxed increment on `getfield_compact_ref` -- the hottest
    // read in the VM -- was measured at 2-3 ns of a 9 ns reference-field read.
    // The two colored-word tripwire counters print either way; they sit on a
    // `#[cold]` arm and cost nothing to keep on.
    E { group: Group::DBG, token: "jit-ref-loads", on_key: Some("CRATONVM_DBG_JIT_REF_LOADS"), off_key: None, off_word: None, since: "2026-09-01" },
    E { group: Group::DBG, token: "getresources", on_key: Some("CRATONVM_DBG_GETRESOURCES"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "getstatic-prof", on_key: Some("CRATONVM_DBG_GETSTATIC_PROF"), off_key: None, off_word: None, since: "2026-08-01" },
    // Per-snapshot trace of the typed operand stack (x64::stack_kinds): what
    // the analysis had at each deopt bci and whether the depth / oop-mark
    // agreement checks accepted it.
    E { group: Group::DBG, token: "stack-kinds", on_key: Some("CRATONVM_DBG_STACK_KINDS"), off_key: None, off_word: None, since: "2026-08-03" },
    // Tally every `real_protected_stub_class` question and its answer, so the
    // stub door can be priced rather than argued about.
    E { group: Group::DBG, token: "stub-door", on_key: Some("CRATONVM_DBG_STUB_DOOR"), off_key: None, off_word: None, since: "2026-08-26" },
    // Companion to `stub-door`: that one says which doors ASK the SyntheticStub
    // arbitration, this one tallies which code path actually INVOKES the native
    // funnel, by call site.
    E { group: Group::DBG, token: "native-entry", on_key: Some("CRATONVM_DBG_NATIVE_ENTRY"), off_key: None, off_word: None, since: "2026-08-26" },
    E { group: Group::DBG, token: "gocbf", on_key: Some("CRATONVM_DBG_GOCBF"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "gpu-dump-ptx", on_key: Some("CRATONVM_GPU_DUMP_PTX"), off_key: None, off_word: None, since: "2026-08-21" },
    E { group: Group::DBG, token: "gpu-trace-bytes", on_key: Some("CRATONVM_GPU_TRACE_BYTES"), off_key: None, off_word: None, since: "2026-05-23" },
    E { group: Group::DBG, token: "gpu-time-dispatch", on_key: Some("CRATONVM_GPU_TIME_DISPATCH"), off_key: None, off_word: None, since: "2026-08-22" },
    E { group: Group::DBG, token: "gse", on_key: Some("CRATONVM_DBG_GSE"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "h2parserread", on_key: Some("CRATONVM_DBG_H2PARSERREAD"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "h2trace", on_key: Some("CRATONVM_DBG_H2TRACE"), off_key: None, off_word: None, since: "2026-07-24" },
    E { group: Group::DBG, token: "hang-sample", on_key: Some("CRATONVM_DBG_HANG_SAMPLE"), off_key: None, off_word: None, since: "2026-07-13" },
    E { group: Group::DBG, token: "hangwalk", on_key: Some("CRATONVM_DBG_HANGWALK"), off_key: None, off_word: None, since: "2026-06-18" },
    E { group: Group::DBG, token: "heap-stale", on_key: Some("CRATONVM_DBG_HEAP_STALE"), off_key: None, off_word: None, since: "2026-06-02" },
    E { group: Group::DBG, token: "fmt-wrongtype", on_key: Some("CRATONVM_DBG_FMT_WRONGTYPE"), off_key: None, off_word: None, since: "2026-08-22" },
    E { group: Group::DBG, token: "heap-trace", on_key: Some("CRATONVM_DBG_HEAP_TRACE"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "heapcopy", on_key: Some("CRATONVM_DBG_HEAPCOPY"), off_key: None, off_word: None, since: "2026-06-05" },
    E { group: Group::DBG, token: "heartbeat", on_key: Some("CRATONVM_DBG_HEARTBEAT"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "hm-trace", on_key: Some("CRATONVM_HM_TRACE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "hminit-purge", on_key: Some("CRATONVM_DBG_HMINIT_PURGE"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::DBG, token: "hmput", on_key: Some("CRATONVM_DBG_HMPUT"), off_key: None, off_word: None, since: "2026-05-27" },
    E { group: Group::DBG, token: "hotpath-counts", on_key: Some("CRATONVM_DBG_HOTPATH_COUNTS"), off_key: None, off_word: None, since: "2026-07-13" },
    E { group: Group::DBG, token: "hs-itr-dbg", on_key: Some("CRATONVM_HS_ITR_DBG"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "httpsrv", on_key: Some("CRATONVM_DBG_HTTPSRV"), off_key: None, off_word: None, since: "2026-06-18" },
    E { group: Group::DBG, token: "iae-trace", on_key: Some("CRATONVM_IAE_TRACE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "iae-trace2", on_key: Some("CRATONVM_IAE_TRACE2"), off_key: None, off_word: None, since: "2026-07-13" },
    E { group: Group::DBG, token: "imse", on_key: Some("CRATONVM_DBG_IMSE"), off_key: None, off_word: None, since: "2026-07-07" },
    E { group: Group::DBG, token: "indy-all", on_key: Some("CRATONVM_DBG_INDY_ALL"), off_key: None, off_word: None, since: "2026-07-03" },
    E { group: Group::DBG, token: "indy-generic", on_key: Some("CRATONVM_DBG_INDY_GENERIC"), off_key: None, off_word: None, since: "2026-07-03" },
    E { group: Group::DBG, token: "inline-fr", on_key: Some("CRATONVM_DBG_INLINE_FR"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::DBG, token: "interrupt", on_key: Some("CRATONVM_DBG_INTERRUPT"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::DBG, token: "intrinsic-stats", on_key: Some("CRATONVM_INTRINSIC_STATS"), off_key: None, off_word: None, since: "2026-05-22" },
    E { group: Group::DBG, token: "isolated-cnf", on_key: Some("CRATONVM_DBG_ISOLATED_CNF"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::DBG, token: "invoke-coerce", on_key: Some("CRATONVM_DBG_INVOKE_COERCE"), off_key: None, off_word: None, since: "2026-06-15" },
    E { group: Group::DBG, token: "invoke-virtual-entry-trace", on_key: Some("CRATONVM_INVOKE_VIRTUAL_ENTRY_TRACE"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "invokestatic-loader-trace", on_key: Some("CRATONVM_INVOKESTATIC_LOADER_TRACE"), off_key: None, off_word: None, since: "2026-07-22" },
    E { group: Group::DBG, token: "invokestats", on_key: Some("CRATONVM_DBG_INVOKESTATS"), off_key: None, off_word: None, since: "2026-07-15" },
    E { group: Group::DBG, token: "invspecial", on_key: Some("CRATONVM_DBG_INVSPECIAL"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "ir-bailout", on_key: Some("CRATONVM_DBG_IR_BAILOUT"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::DBG, token: "ir-call", on_key: Some("CRATONVM_DBG_IR_CALL"), off_key: None, off_word: None, since: "2026-06-20" },
    E { group: Group::DBG, token: "ir-bufsize", on_key: Some("CRATONVM_DBG_IR_BUFSIZE"), off_key: None, off_word: None, since: "2026-08-01" },
    E { group: Group::DBG, token: "ir-compiles", on_key: Some("CRATONVM_DBG_IR_COMPILES"), off_key: None, off_word: None, since: "2026-07-31" },
    // The trace half of `jit/ir-linear-scan`; same token name in the group that
    // owns tracing, exactly like `ir-long` and `xt-jit-root-scan` below.
    E { group: Group::DBG, token: "ir-isel", on_key: Some("CRATONVM_DBG_IR_ISEL"), off_key: None, off_word: None, since: "2026-08-03" },
    E { group: Group::DBG, token: "ir-linear-scan", on_key: Some("CRATONVM_DBG_IR_LINEAR_SCAN"), off_key: None, off_word: None, since: "2026-08-01" },
    E { group: Group::DBG, token: "ir-long", on_key: Some("CRATONVM_DBG_IR_LONG"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::DBG, token: "ir-reloc", on_key: Some("CRATONVM_DBG_IR_RELOC"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::DBG, token: "ir-slots", on_key: Some("CRATONVM_DBG_IR_SLOTS"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::DBG, token: "irslot", on_key: Some("CRATONVM_DBG_IRSLOT"), off_key: None, off_word: None, since: "2026-07-10" },
    E { group: Group::DBG, token: "isinstance", on_key: Some("CRATONVM_DBG_ISINSTANCE"), off_key: None, off_word: None, since: "2026-07-04" },
    E { group: Group::DBG, token: "jar", on_key: Some("CRATONVM_DBG_JAR"), off_key: None, off_word: None, since: "2026-07-25" },
    // Scalar, not a boolean: `=<n>` raises the `--jdk-only` shadow-observation
    // sink above its 256-row default so an APPLICATION census is complete
    // rather than truncated. Diagnostic only — it changes nothing a program can
    // observe. Declared rather than read through bare `getenv` so it comes from
    // the latched snapshot like every other knob; see the doc on
    // `vm::jdk_only_native_shadow_cap`.
    E { group: Group::DBG, token: "native-shadow-sink-cap", on_key: Some("CRATONVM_NATIVE_SHADOW_SINK_CAP"), off_key: None, off_word: None, since: "2026-08-17" },
    E { group: Group::DBG, token: "jetty", on_key: Some("CRATONVM_DBG_JETTY"), off_key: None, off_word: None, since: "2026-05-21" },
    E { group: Group::DBG, token: "jetty2", on_key: Some("CRATONVM_DBG_JETTY2"), off_key: None, off_word: None, since: "2026-05-21" },
    E { group: Group::DBG, token: "jit-alloc", on_key: Some("CRATONVM_DBG_JIT_ALLOC"), off_key: None, off_word: None, since: "2026-06-11" },
    E { group: Group::DBG, token: "jit-bisect-only", on_key: Some("CRATONVM_JIT_BISECT_ONLY"), off_key: None, off_word: None, since: "2026-05-25" },
    E { group: Group::DBG, token: "jit-code", on_key: Some("CRATONVM_DBG_JIT_CODE"), off_key: None, off_word: None, since: "2026-05-29" },
    E { group: Group::DBG, token: "jit-code-free", on_key: Some("CRATONVM_DBG_JIT_CODE_FREE"), off_key: None, off_word: None, since: "2026-07-28" },
    E { group: Group::DBG, token: "jit-compiled", on_key: Some("CRATONVM_DBG_JIT_COMPILED"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::DBG, token: "jit-disasm", on_key: Some("CRATONVM_DBG_JIT_DISASM"), off_key: None, off_word: None, since: "2026-06-11" },
    E { group: Group::DBG, token: "jit-slot-overlap", on_key: Some("CRATONVM_DBG_JIT_SLOT_OVERLAP"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::DBG, token: "jit-dispatch", on_key: Some("CRATONVM_DBG_JIT_DISPATCH"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "jit-entry", on_key: Some("CRATONVM_DBG_JIT_ENTRY"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "jit-borrow-sites", on_key: Some("CRATONVM_DBG_JIT_BORROW_SITES"), off_key: None, off_word: None, since: "2026-08-18" },
    E { group: Group::DBG, token: "jit-gen", on_key: Some("CRATONVM_DBG_JIT_GEN"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "jit-ldc", on_key: Some("CRATONVM_DBG_JIT_LDC"), off_key: None, off_word: None, since: "2026-06-11" },
    E { group: Group::DBG, token: "loop-work", on_key: Some("CRATONVM_DBG_LOOP_WORK"), off_key: None, off_word: None, since: "2026-08-04" },
    E { group: Group::DBG, token: "field-site", on_key: Some("CRATONVM_DBG_FIELD_SITE"), off_key: None, off_word: None, since: "2026-08-04" },
    // Traces a field resolution that matched on NAME only after the
    // name+descriptor lookup missed -- the separate-compilation shape where
    // the JVMS key and the name key disagree. Read presence-only
    // (`runtime_var_os(..).is_some()`) at vm/src/runtime/resolve/mod.rs, so no
    // off_word. Declared here after `flag_declaration_guard` caught it reading
    // through a live `getenv`, which is how a flag-dependent test ends up
    // measuring the developer's ambient environment.
    E { group: Group::DBG, token: "field-descriptor", on_key: Some("CRATONVM_DBG_FIELD_DESCRIPTOR"), off_key: None, off_word: None, since: "2026-08-27" },
    // Four more presence-only DBG traces from the 2026-08-27/28 JIT wave, all
    // read through a live `getenv` until this declaration.
    E { group: Group::DBG, token: "jit-direct-binds", on_key: Some("CRATONVM_DBG_JIT_DIRECT_BINDS"), off_key: None, off_word: None, since: "2026-08-28" },
    E { group: Group::DBG, token: "jit-ea", on_key: Some("CRATONVM_DBG_JIT_EA"), off_key: None, off_word: None, since: "2026-08-28" },
    E { group: Group::DBG, token: "ir-graph", on_key: Some("CRATONVM_DBG_IR_GRAPH"), off_key: None, off_word: None, since: "2026-08-28" },
    E { group: Group::DBG, token: "ir-sink", on_key: Some("CRATONVM_DBG_IR_SINK"), off_key: None, off_word: None, since: "2026-09-05" },
    E { group: Group::DBG, token: "zgc-target", on_key: Some("CRATONVM_DBG_ZGC_TARGET"), off_key: None, off_word: None, since: "2026-08-28" },
    // The LARGE-OBJECT end's compactor, one line per engaged cycle: what it
    // moved and what the high free list looked like on either side of it.
    // Declared 2026-08-29.
    E { group: Group::DBG, token: "zgc-high", on_key: Some("CRATONVM_DBG_ZGC_HIGH"), off_key: None, off_word: None, since: "2026-08-29" },
    E { group: Group::DBG, token: "jit-elide-ctor", on_key: Some("CRATONVM_DBG_JIT_ELIDE_CTOR"), off_key: None, off_word: None, since: "2026-08-28" },
    E { group: Group::DBG, token: "jit-field-sites", on_key: Some("CRATONVM_DBG_JIT_FIELD_SITES"), off_key: None, off_word: None, since: "2026-08-28" },
    E { group: Group::DBG, token: "g1-live-memo", on_key: Some("CRATONVM_DBG_G1_LIVE_MEMO"), off_key: None, off_word: None, since: "2026-08-17" },
    E { group: Group::DBG, token: "jit-method-stats", on_key: Some("CRATONVM_DBG_JIT_METHOD_STATS"), off_key: None, off_word: None, since: "2026-07-22" },
    E { group: Group::DBG, token: "jit-mic", on_key: Some("CRATONVM_DBG_JIT_MIC"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "jit-scan-prof", on_key: Some("CRATONVM_DBG_JIT_SCAN_PROF"), off_key: None, off_word: None, since: "2026-08-10" },
    E { group: Group::DBG, token: "jit-rootscan", on_key: Some("CRATONVM_DBG_JIT_ROOTSCAN"), off_key: None, off_word: None, since: "2026-08-20" },
    E { group: Group::DBG, token: "remap-residue", on_key: Some("CRATONVM_DBG_REMAP_RESIDUE"), off_key: None, off_word: None, since: "2026-08-28" },
    // Declared 2026-08-23. `jit-rootscan` reports the moving-young coverage
    // verdict as an aggregate `map_coverage=N` counter, which names neither the
    // METHOD nor WHICH of `fully_oop_covered`'s four terms said no. `oopcov`
    // prints both, per compile. See `jit/src/x64/driver.rs`.
    E { group: Group::DBG, token: "oopcov", on_key: Some("CRATONVM_DBG_OOPCOV"), off_key: None, off_word: None, since: "2026-08-23" },
    // Declared 2026-08-23 with the cross-thread JIT coverage handshake: the
    // per-cycle `peer_depth`/`proven` arithmetic and each peer's deposit. See
    // `vm/src/jit/conservative_roots.rs::publish_peer_jit_coverage_for_stw`.
    E { group: Group::DBG, token: "xt-coverage", on_key: Some("CRATONVM_DBG_XT_COVERAGE"), off_key: None, off_word: None, since: "2026-08-23" },
    E { group: Group::DBG, token: "jit-stale-after-remap", on_key: Some("CRATONVM_DBG_JIT_STALE_AFTER_REMAP"), off_key: None, off_word: None, since: "2026-08-22" },
    E { group: Group::DBG, token: "jit-stale-below-rbp", on_key: Some("CRATONVM_DBG_JIT_STALE_BELOW_RBP"), off_key: None, off_word: None, since: "2026-08-22" },
    E { group: Group::DBG, token: "jit-names", on_key: Some("CRATONVM_DBG_JIT_NAMES"), off_key: None, off_word: None, since: "2026-06-16" },
    E { group: Group::DBG, token: "jit-pin", on_key: Some("CRATONVM_DBG_JIT_PIN"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::DBG, token: "jit-putfield", on_key: Some("CRATONVM_DBG_JIT_PUTFIELD"), off_key: None, off_word: None, since: "2026-06-01" },
    E { group: Group::DBG, token: "jit-safepoints", on_key: Some("CRATONVM_DBG_JIT_SAFEPOINTS"), off_key: None, off_word: None, since: "2026-07-24" },
    E { group: Group::DBG, token: "jit-stale-ic", on_key: Some("CRATONVM_DBG_JIT_STALE_IC"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::DBG, token: "jit-unmap", on_key: Some("CRATONVM_DBG_JIT_UNMAP"), off_key: None, off_word: None, since: "2026-07-28" },
    E { group: Group::DBG, token: "intrinsic", on_key: Some("CRATONVM_DBG_INTRINSIC"), off_key: None, off_word: None, since: "2026-08-13" },
    E { group: Group::DBG, token: "jitc", on_key: Some("CRATONVM_DBG_JITC"), off_key: None, off_word: None, since: "2026-05-21" },
    E { group: Group::DBG, token: "jlm", on_key: Some("CRATONVM_DBG_JLM"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "jul", on_key: Some("CRATONVM_DBG_JUL"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::DBG, token: "kcbool", on_key: Some("CRATONVM_DBG_KCBOOL"), off_key: None, off_word: None, since: "2026-05-21" },
    E { group: Group::DBG, token: "lambda", on_key: Some("CRATONVM_DBG_LAMBDA"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "lambda-dispatch", on_key: Some("CRATONVM_DBG_LAMBDA_DISPATCH"), off_key: None, off_word: None, since: "2026-07-15" },
    E { group: Group::DBG, token: "lambda-generic", on_key: Some("CRATONVM_DBG_LAMBDA_GENERIC"), off_key: None, off_word: None, since: "2026-07-22" },
    // Per-phase timing for `try_lambda_dispatch` (lookup / prep / target /
    // other), printed every 200k dispatches. Arms the timers; an unarmed run
    // pays one relaxed load per dispatch. See `runtime::interpreter::lambda::lambda_prof`.
    E { group: Group::DBG, token: "lambda-jit", on_key: Some("CRATONVM_DBG_LAMBDA_JIT"), off_key: None, off_word: None, since: "2026-08-17" },
    E { group: Group::DBG, token: "lambda-prof", on_key: Some("CRATONVM_DBG_LAMBDA_PROF"), off_key: None, off_word: None, since: "2026-08-10" },
    E { group: Group::DBG, token: "layout", on_key: Some("CRATONVM_DBG_LAYOUT"), off_key: None, off_word: None, since: "2026-07-06" },
    E { group: Group::DBG, token: "ldc-classref-trace", on_key: Some("CRATONVM_LDC_CLASSREF_TRACE"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "letsgo", on_key: Some("CRATONVM_DBG_LETSGO"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "lhm-evict", on_key: Some("CRATONVM_DBG_LHM_EVICT"), off_key: None, off_word: None, since: "2026-07-06" },
    E { group: Group::DBG, token: "licm", on_key: Some("CRATONVM_DBG_LICM"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::DBG, token: "linkage", on_key: Some("CRATONVM_DBG_LINKAGE"), off_key: None, off_word: None, since: "2026-08-01" },
    E { group: Group::DBG, token: "linkage-bt", on_key: Some("CRATONVM_DBG_LINKAGE_BT"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::DBG, token: "linker", on_key: Some("CRATONVM_DBG_LINKER"), off_key: None, off_word: None, since: "2026-07-15" },
    E { group: Group::DBG, token: "loadclass", on_key: Some("CRATONVM_DBG_LOADCLASS"), off_key: None, off_word: None, since: "2026-07-14" },
    E { group: Group::DBG, token: "loader-chain", on_key: Some("CRATONVM_DBG_LOADER_CHAIN"), off_key: None, off_word: None, since: "2026-07-30" },
    E { group: Group::DBG, token: "loader-trace", on_key: Some("CRATONVM_DBG_LOADER_TRACE"), off_key: None, off_word: None, since: "2026-07-22" },
    // Restores the pre-fix load-time transform behaviour: offer every class to
    // the `ClassFileTransformer` chain on every constant-pool resolution rather
    // than once per name. The red control for the load-time-transform rescan
    // fix (see `runtime::instrument::LoadTimeOffered`) — with it set, a Spring
    // Boot `@ClassPathExclusions` test under Mockito's inline mock maker hangs
    // instead of passing.
    E { group: Group::DBG, token: "load-transform-no-memo", on_key: Some("CRATONVM_DBG_LOAD_TRANSFORM_NO_MEMO"), off_key: None, off_word: None, since: "2026-08-10" },
    E { group: Group::DBG, token: "logprov", on_key: Some("CRATONVM_DBG_LOGPROV"), off_key: None, off_word: None, since: "2026-06-14" },
    E { group: Group::DBG, token: "longroot", on_key: Some("CRATONVM_DBG_LONGROOT"), off_key: None, off_word: None, since: "2026-06-09" },
    E { group: Group::DBG, token: "lookup", on_key: Some("CRATONVM_DBG_LOOKUP"), off_key: None, off_word: None, since: "2026-06-11" },
    E { group: Group::DBG, token: "map-miss-audit", on_key: Some("CRATONVM_DBG_MAP_MISS_AUDIT"), off_key: None, off_word: None, since: "2026-07-31" },
    // The keySet-view rebuild-elision census, printed at exit by
    // `native_collections::report_map_view_cache_at_exit`. `resync_skipped` is
    // the ENGAGEMENT counter for that fast path: a wall-clock number quoted
    // without it cannot say whether the path ran at all.
    E { group: Group::DBG, token: "map-view-cache", on_key: Some("CRATONVM_DBG_MAP_VIEW_CACHE"), off_key: None, off_word: None, since: "2026-08-23" },
    E { group: Group::DBG, token: "mapper", on_key: Some("CRATONVM_DBG_MAPPER"), off_key: None, off_word: None, since: "2026-07-30" },
    E { group: Group::DBG, token: "mcl", on_key: Some("CRATONVM_DBG_MCL"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "memwatch", on_key: Some("CRATONVM_DBG_MEMWATCH"), off_key: None, off_word: None, since: "2026-06-09" },
    E { group: Group::DBG, token: "method-invoke-box", on_key: Some("CRATONVM_DBG_METHOD_INVOKE_BOX"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "mh-adapter", on_key: Some("CRATONVM_DBG_MH_ADAPTER"), off_key: None, off_word: None, since: "2026-07-15" },
    E { group: Group::DBG, token: "mh-dispatch", on_key: Some("CRATONVM_DBG_MH_DISPATCH"), off_key: None, off_word: None, since: "2026-07-15" },
    E { group: Group::DBG, token: "mh-stack", on_key: Some("CRATONVM_DBG_MH_STACK"), off_key: None, off_word: None, since: "2026-07-15" },
    E { group: Group::DBG, token: "mic-prof", on_key: Some("CRATONVM_DBG_MIC_PROF"), off_key: None, off_word: None, since: "2026-06-12" },
    // The notification-credit census -- how much of this VM's Object.wait()
    // waking actually came from the CONDITION rather than from the condvar
    // signal. `credits_consumed` is the engagement counter for the netty
    // lost-wakeup fix.
    E { group: Group::DBG, token: "monitor-notify", on_key: Some("CRATONVM_DBG_MONITOR_NOTIFY"), off_key: None, off_word: None, since: "2026-08-24" },
    // The execution profiler (`runtime::exec_sampler`). A VALUE flag: the
    // millisecond sampling interval, off when unset or 0. It exists because
    // there was no way to ask this VM where a workload's time goes --
    // `jdk.ExecutionSample` is defined in the JFR crate with no caller, and
    // `wpr -start CPU` needs a privilege this host does not carry.
    E { group: Group::DBG, token: "profile-sample-ms", on_key: Some("CRATONVM_PROFILE_SAMPLE_MS"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::DBG, token: "mic-method", on_key: Some("CRATONVM_DBG_MIC_METHOD"), off_key: None, off_word: None, since: "2026-08-11" },
    E { group: Group::DBG, token: "mark-why-class", on_key: Some("CRATONVM_DBG_MARK_WHY_CLASS"), off_key: None, off_word: None, since: "2026-08-10" },
    E { group: Group::DBG, token: "mirrorpin-why", on_key: Some("CRATONVM_DBG_MIRRORPIN_WHY"), off_key: None, off_word: None, since: "2026-08-10" },
    E { group: Group::DBG, token: "root-source", on_key: Some("CRATONVM_DBG_ROOT_SOURCE"), off_key: None, off_word: None, since: "2026-08-07" },
    E { group: Group::DBG, token: "zgc-verify-slide", on_key: Some("CRATONVM_DBG_ZGC_VERIFY_SLIDE"), off_key: None, off_word: None, since: "2026-08-14" },
    E { group: Group::DBG, token: "zgc-corpse", on_key: Some("CRATONVM_DBG_ZGC_CORPSE"), off_key: None, off_word: None, since: "2026-08-15" },
    // Declared 2026-08-17: both were read by `runtime_var_os` and named nowhere,
    // so `CRATONVM_DBG=mapgen` could not reach them and a test could not arrange
    // one with `flags::with_thread_overrides` -- and while the guard was red it
    // could not catch the NEXT undeclared flag, which is what it is for.
    // `mapgen` reports a safepoint waiter that resumed on a pointer map from a
    // different pause generation than the one it arrived for; `vacated-frames`
    // arms the vacated-address ledger that catches a frame slot still holding an
    // address the collector moved away from.
    E { group: Group::DBG, token: "mapgen", on_key: Some("CRATONVM_DBG_MAPGEN"), off_key: None, off_word: None, since: "2026-08-17" },
    E { group: Group::DBG, token: "vacated-frames", on_key: Some("CRATONVM_DBG_VACATED_FRAMES"), off_key: None, off_word: None, since: "2026-08-17" },
    E { group: Group::DBG, token: "atomic-intrinsic", on_key: Some("CRATONVM_DBG_ATOMIC_INTRINSIC"), off_key: None, off_word: None, since: "2026-08-13" },
    E { group: Group::DBG, token: "define-filter", on_key: Some("CRATONVM_DBG_DEFINE_FILTER"), off_key: None, off_word: None, since: "2026-08-13" },
    E { group: Group::DBG, token: "define-stack-filter", on_key: Some("CRATONVM_DBG_DEFINE_STACK_FILTER"), off_key: None, off_word: None, since: "2026-08-13" },
    E { group: Group::DBG, token: "hw-atomic", on_key: Some("CRATONVM_DBG_HW_ATOMIC"), off_key: None, off_word: None, since: "2026-08-13" },
    E { group: Group::DBG, token: "jca-getinstance", on_key: Some("CRATONVM_DBG_JCA_GETINSTANCE"), off_key: None, off_word: None, since: "2026-08-14" },
    E { group: Group::DBG, token: "mic-trace", on_key: Some("CRATONVM_DBG_MIC_TRACE"), off_key: None, off_word: None, since: "2026-08-10" },
    E { group: Group::DBG, token: "minvoke", on_key: Some("CRATONVM_DBG_MINVOKE"), off_key: None, off_word: None, since: "2026-06-12" },
    E { group: Group::DBG, token: "mirrorpin", on_key: Some("CRATONVM_DBG_MIRRORPIN"), off_key: None, off_word: None, since: "2026-07-13" },
    E { group: Group::DBG, token: "modprov", on_key: Some("CRATONVM_DBG_MODPROV"), off_key: None, off_word: None, since: "2026-06-23" },
    E { group: Group::DBG, token: "modstatic", on_key: Some("CRATONVM_DBG_MODSTATIC"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "monenter", on_key: Some("CRATONVM_DBG_MONENTER"), off_key: None, off_word: None, since: "2026-06-11" },
    E { group: Group::DBG, token: "monexit", on_key: Some("CRATONVM_DBG_MONEXIT"), off_key: None, off_word: None, since: "2026-07-11" },
    E { group: Group::DBG, token: "moving-young-band-dbg", on_key: Some("CRATONVM_MOVING_YOUNG_BAND_DBG"), off_key: None, off_word: None, since: "2026-07-26" },
    // Pairs the per-cycle relocation verdict with `ThreadStateCensus::
    // relocation_blockers()`, which is the codebase's own statement of the
    // `mark_moving_young_coverage_incomplete_because` obligation. Off by default.
    E { group: Group::DBG, token: "relocation-blockers", on_key: Some("CRATONVM_DBG_RELOCATION_BLOCKERS"), off_key: None, off_word: None, since: "2026-09-06" },
    // Diagnostic widening of the register-image remap to the whole unverifiable
    // frame tail, to TEST the four-region partition in `conservative_roots`'s
    // module comment rather than continue to argue it. Off by default; see
    // `register_image_remap_admits`.
    E { group: Group::DBG, token: "jit-remap-all-unverifiable", on_key: Some("CRATONVM_JIT_REMAP_ALL_UNVERIFIABLE"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::GC, token: "moving-young-band-skip-in-map", on_key: Some("CRATONVM_MOVING_YOUNG_BAND_SKIP_IN_MAP"), off_key: None, off_word: None, since: "2026-09-04" },
    E { group: Group::DBG, token: "moving-young-coverage-dbg", on_key: Some("CRATONVM_MOVING_YOUNG_COVERAGE_DBG"), off_key: None, off_word: None, since: "2026-07-01" },
    E { group: Group::DBG, token: "moving-young-fallbacks", on_key: Some("CRATONVM_MOVING_YOUNG_FALLBACKS"), off_key: None, off_word: None, since: "2026-07-01" },
    E { group: Group::DBG, token: "moving-young-no-band-verify", on_key: Some("CRATONVM_MOVING_YOUNG_NO_BAND_VERIFY"), off_key: None, off_word: None, since: "2026-07-26" },
    E { group: Group::DBG, token: "moving-young-verify", on_key: Some("CRATONVM_MOVING_YOUNG_VERIFY"), off_key: None, off_word: None, since: "2026-07-01" },
    E { group: Group::DBG, token: "msc", on_key: Some("CRATONVM_DBG_MSC"), off_key: None, off_word: None, since: "2026-06-05" },
    E { group: Group::DBG, token: "mtroots", on_key: Some("CRATONVM_DBG_MTROOTS"), off_key: None, off_word: None, since: "2026-06-16" },
    // A REVERT knob, not a trace: it restores the pre-2026-08 unconditional
    // `System.loadLibrary`/`NativeLibraries.load` success for a library this VM
    // does not implement, which flips callers like Netty's
    // `NativeLibraryLoader` out of their pure-Java fallback. Filed under DBG
    // because the key is spelled `DBG_` and every `CRATONVM_DBG_*` key in this
    // table is in `Group::DBG`.
    //
    // It is one of three DBG tokens that can change a program's result, NOT the
    // only one — `force-moving` is the prior art and the more dangerous case
    // (it relocates JIT-held raw pointers). All three are named in the
    // `Group::DBG` doc; `render-inventory.py` labels the whole group `diag`,
    // which is a generator limitation recorded there rather than a wart here.
    E { group: Group::DBG, token: "nativelibraries-load-ok", on_key: Some("CRATONVM_DBG_NATIVELIBRARIES_LOAD_OK"), off_key: None, off_word: None, since: "2026-08-07" },
    E { group: Group::DBG, token: "native-lookups", on_key: Some("CRATONVM_DBG_NATIVE_LOOKUPS"), off_key: None, off_word: None, since: "2026-08-14" },
    E { group: Group::DBG, token: "ncdfe", on_key: Some("CRATONVM_DBG_NCDFE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "native-shadow", on_key: Some("CRATONVM_DBG_NATIVE_SHADOW"), off_key: None, off_word: None, since: "2026-08-30" },
    E { group: Group::DBG, token: "needs-exact-trace", on_key: Some("CRATONVM_NEEDS_EXACT_TRACE"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "net", on_key: Some("CRATONVM_DBG_NET"), off_key: None, off_word: None, since: "2026-06-04" },
    E { group: Group::DBG, token: "netty-queue", on_key: Some("CRATONVM_DBG_NETTY_QUEUE"), off_key: None, off_word: None, since: "2026-07-08" },
    E { group: Group::DBG, token: "nextint", on_key: Some("CRATONVM_DBG_NEXTINT"), off_key: None, off_word: None, since: "2026-06-18" },
    E { group: Group::DBG, token: "nio-bind", on_key: Some("CRATONVM_DBG_NIO_BIND"), off_key: None, off_word: None, since: "2026-05-28" },
    E { group: Group::DBG, token: "nocode", on_key: Some("CRATONVM_DBG_NOCODE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "nonmoving-reclaim", on_key: None, off_key: Some("CRATONVM_DBG_NO_NONMOVING_RECLAIM"), off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "npe-invoke", on_key: Some("CRATONVM_DBG_NPE_INVOKE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "null-field-provenance", on_key: Some("CRATONVM_DBG_NULL_FIELD_PROVENANCE"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::DBG, token: "npe-none", on_key: Some("CRATONVM_DBG_NPE_NONE"), off_key: None, off_word: None, since: "2026-07-11" },
    E { group: Group::DBG, token: "npe-match", on_key: Some("CRATONVM_DBG_NPE_MATCH"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::DBG, token: "a5-engagement", on_key: Some("CRATONVM_DBG_A5_ENGAGEMENT"), off_key: None, off_word: None, since: "2026-08-18" },
    E { group: Group::DBG, token: "unreg-memo-audit", on_key: Some("CRATONVM_DBG_UNREG_MEMO_AUDIT"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::DBG, token: "redefine-dump", on_key: Some("CRATONVM_DBG_REDEFINE_DUMP"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::DBG, token: "npe-stack", on_key: Some("CRATONVM_DBG_NPE_STACK"), off_key: None, off_word: None, since: "2026-05-28" },
    E { group: Group::DBG, token: "npe-trace", on_key: Some("CRATONVM_DBG_NPE_TRACE"), off_key: None, off_word: None, since: "2026-05-23" },
    E { group: Group::DBG, token: "nsee-trace", on_key: Some("CRATONVM_NSEE_TRACE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "nsme", on_key: Some("CRATONVM_DBG_NSME"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "null-native", on_key: Some("CRATONVM_DBG_NULL_NATIVE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "nullthis", on_key: Some("CRATONVM_DBG_NULLTHIS"), off_key: None, off_word: None, since: "2026-06-11" },
    E { group: Group::DBG, token: "obj-equals", on_key: Some("CRATONVM_DBG_OBJ_EQUALS"), off_key: None, off_word: None, since: "2026-05-23" },
    E { group: Group::DBG, token: "objects", on_key: Some("CRATONVM_DBG_OBJECTS"), off_key: None, off_word: None, since: "2026-06-14" },
    E { group: Group::DBG, token: "objkey", on_key: Some("CRATONVM_DBG_OBJKEY"), off_key: None, off_word: None, since: "2026-08-01" },
    E { group: Group::DBG, token: "obsreg", on_key: Some("CRATONVM_DBG_OBSREG"), off_key: None, off_word: None, since: "2026-07-22" },
    E { group: Group::DBG, token: "oldsweep-owners", on_key: Some("CRATONVM_DBG_OLDSWEEP_OWNERS"), off_key: None, off_word: None, since: "2026-08-01" },
    E { group: Group::DBG, token: "oobfield", on_key: Some("CRATONVM_DBG_OOBFIELD"), off_key: None, off_word: None, since: "2026-05-22" },
    E { group: Group::DBG, token: "oom-bt", on_key: Some("CRATONVM_DBG_OOM_BT"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::DBG, token: "oop-span-probe", on_key: Some("CRATONVM_OOP_SPAN_PROBE"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "osr", on_key: Some("CRATONVM_DBG_OSR"), off_key: None, off_word: None, since: "2026-05-22" },
    // Declared 2026-08-11. The probe `d1648f133` left behind in
    // `memory::native_roots` after fixing `scan_collection_overlays`'s gate --
    // it prints which of the four predicates the conditional actually turned
    // on, which is the question that fix got wrong. It reads through
    // `runtime_var_os` already; it was simply never named here, so the grouped
    // `CRATONVM_DBG=overlay-gate` spelling could not reach it.
    E { group: Group::DBG, token: "overlay-gate", on_key: Some("CRATONVM_DBG_OVERLAY_GATE"), off_key: None, off_word: None, since: "2026-08-11" },
    E { group: Group::DBG, token: "owner-filter", on_key: Some("CRATONVM_DBG_OWNER_FILTER"), off_key: None, off_word: None, since: "2026-08-01" },
    E { group: Group::DBG, token: "osr-exit-after", on_key: Some("CRATONVM_OSR_EXIT_AFTER"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::DBG, token: "osr-exit-test", on_key: Some("CRATONVM_OSR_EXIT_TEST"), off_key: None, off_word: None, since: "2026-06-21" },
    // Takes a VALUE, not a presence: a class-name substring restricting the
    // frame trace. The grouped spelling expands to `=1`, which matches no class
    // name, so `CRATONVM_DBG=osr-frame-trace` arms nothing on its own — which is
    // deliberate. An unfiltered frame trace emits a line per back edge in the
    // JDK. Set `CRATONVM_DBG_OSR_FRAME_TRACE=<substring>` directly. Declared
    // here anyway, because an undeclared name is served from live `getenv`
    // rather than the latched snapshot and is invisible to the override hook.
    E { group: Group::DBG, token: "osr-bind", on_key: Some("CRATONVM_DBG_OSR_BIND"), off_key: None, off_word: None, since: "2026-08-17" },
    E { group: Group::DBG, token: "osr-frame-trace", on_key: Some("CRATONVM_DBG_OSR_FRAME_TRACE"), off_key: None, off_word: None, since: "2026-08-04" },
    E { group: Group::DBG, token: "osr-meta", on_key: Some("CRATONVM_DBG_OSR_META"), off_key: None, off_word: None, since: "2026-07-10" },
    E { group: Group::DBG, token: "osr-seed-collision", on_key: Some("CRATONVM_DBG_OSR_SEED_COLLISION"), off_key: None, off_word: None, since: "2026-08-06" },
    E { group: Group::DBG, token: "osr-slots", on_key: Some("CRATONVM_DBG_OSR_SLOTS"), off_key: None, off_word: None, since: "2026-08-19" },
    // Declared 2026-08-22. `view-kind` reports a map-view carrier whose CLASS
    // and whose head element disagree about what it holds, and every `vc_route`
    // rebuild; `view-resync` reports a TreeMap range view's rebuild -- pairs
    // seen, kept, bounds and the comparison verdict histogram. The pair
    // separated three defects that all present as one red vector: an entrySet
    // that came back holding values, a view that came back EMPTY, and a stale
    // element inside the scan. An empty view that saw no pairs and one whose
    // every comparison landed out of range are different defects and nothing
    // else can tell them apart.
    E { group: Group::DBG, token: "view-kind", on_key: Some("CRATONVM_DBG_VIEWKIND"), off_key: None, off_word: None, since: "2026-08-22" },
    E { group: Group::DBG, token: "view-resync", on_key: Some("CRATONVM_DBG_VIEWRESYNC"), off_key: None, off_word: None, since: "2026-08-22" },
    E { group: Group::DBG, token: "overlay", on_key: Some("CRATONVM_DBG_OVERLAY"), off_key: None, off_word: None, since: "2026-06-10" },
    E { group: Group::DBG, token: "overlay-all", on_key: Some("CRATONVM_DBG_OVERLAY_ALL"), off_key: None, off_word: None, since: "2026-06-10" },
    E { group: Group::DBG, token: "overlay-bt", on_key: Some("CRATONVM_DBG_OVERLAY_BT"), off_key: None, off_word: None, since: "2026-08-04" },
    // `overlay-nodedup` — report EVERY model-slot access, not the first per
    // (class, slot, read/write). The default cap exists because the complete
    // list of disagreeing slots is already printed once per class by the
    // shadow-layout census, so a repeat line adds nothing but volume — and
    // `java/lang/String` slot 1 alone would bury the run. Turn it off when you
    // want per-site COUNTS rather than per-site presence.
    E { group: Group::DBG, token: "overlay-nodedup", on_key: Some("CRATONVM_DBG_OVERLAY_NODEDUP"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::DBG, token: "overlay-prune", on_key: Some("CRATONVM_DBG_OVERLAY_PRUNE"), off_key: None, off_word: None, since: "2026-08-01" },
    E { group: Group::DBG, token: "tmview", on_key: Some("CRATONVM_DBG_TMVIEW"), off_key: None, off_word: None, since: "2026-08-22" },
    E { group: Group::DBG, token: "parklat", on_key: Some("CRATONVM_DBG_PARKLAT"), off_key: None, off_word: None, since: "2026-07-07" },
    E { group: Group::DBG, token: "pb", on_key: Some("CRATONVM_DBG_PB"), off_key: None, off_word: None, since: "2026-06-17" },
    E { group: Group::DBG, token: "pbe", on_key: Some("CRATONVM_DBG_PBE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "pbstart", on_key: Some("CRATONVM_DBG_PBSTART"), off_key: None, off_word: None, since: "2026-05-25" },
    // `jfr::phase` — phase accounting. The enable gate takes a *level*
    // (`1`/`true`/`on`/`coarse`, or `fine`), and the two sinks take paths:
    // `CRATONVM_DBG=phase-accounting=fine,phase-accounting-out=/tmp/p.json`.
    E { group: Group::DBG, token: "phase-accounting", on_key: Some("CRATONVM_PHASE_ACCOUNTING"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::DBG, token: "phase-accounting-jfr", on_key: Some("CRATONVM_PHASE_ACCOUNTING_JFR"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::DBG, token: "phase-accounting-out", on_key: Some("CRATONVM_PHASE_ACCOUNTING_OUT"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::DBG, token: "picocli-style", on_key: Some("CRATONVM_DBG_PICOCLI_STYLE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "popint", on_key: Some("CRATONVM_DBG_POPINT"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::DBG, token: "precise", on_key: Some("CRATONVM_DBG_PRECISE"), off_key: None, off_word: None, since: "2026-06-04" },
    E { group: Group::DBG, token: "promo-seed", on_key: Some("CRATONVM_DBG_PROMO_SEED"), off_key: None, off_word: None, since: "2026-08-01" },
    E { group: Group::DBG, token: "proxy", on_key: Some("CRATONVM_DBG_PROXY"), off_key: None, off_word: None, since: "2026-06-11" },
    E { group: Group::DBG, token: "prune", on_key: None, off_key: Some("CRATONVM_DBG_NO_PRUNE"), off_word: None, since: "2026-06-03" },
    E { group: Group::DBG, token: "jit-root-scan", on_key: None, off_key: Some("CRATONVM_DBG_NO_JIT_ROOT_SCAN"), off_word: None, since: "2026-08-30" },
    E { group: Group::DBG, token: "fincand", on_key: Some("CRATONVM_DBG_FINCAND"), off_key: None, off_word: None, since: "2026-08-30" },
    E { group: Group::GC, token: "forced-finalizers", on_key: Some("CRATONVM_FORCED_FINALIZERS"), off_key: None, off_word: Some("0"), since: "2026-08-30" },
    // The named-writer arm of the punned-reference counter: on a NON-ZERO
    // payload word under a non-`Object` tag, print the class and field so the
    // writer can be found rather than inferred. Diagnostic only -- the counter
    // itself is unconditional; this names what it counted.
    E { group: Group::DBG, token: "punned-ref", on_key: Some("CRATONVM_DBG_PUNNED_REF"), off_key: None, off_word: None, since: "2026-08-24" },
    // Per-event trace of the map/set VIEW comodification door: which check
    // fired, and whether the modCount stamp behind it is live or dead. A
    // pass/fail cell cannot tell a working door from a dead stamp, which is
    // what this exists to distinguish.
    E { group: Group::DBG, token: "view-comod", on_key: Some("CRATONVM_DBG_VIEW_COMOD"), off_key: None, off_word: None, since: "2026-08-24" },
    // Was the bare `CRATONVM_DBG`, which is now the group variable itself. It
    // gated exactly one call site (`quarkus_staticinit.rs`), so it becomes an
    // ordinary topic. `CRATONVM_DBG=1` no longer enables it — that spelling now
    // reports `1` as an unknown token, which is the point.
    E { group: Group::DBG, token: "quarkus-staticinit", on_key: Some("CRATONVM_DBG_QUARKUS_STATICINIT"), off_key: None, off_word: None, since: "2026-07-26" },
    E { group: Group::DBG, token: "quicken-stats", on_key: Some("CRATONVM_QUICKEN_STATS"), off_key: None, off_word: None, since: "2026-07-25" },
    // Suppresses the "falling back to the process environment" notice that
    // `spring_startup_bootstrap` prints; a *silencer*, so it is stated
    // positively here and the topic it silences is the notice itself.
    E { group: Group::DBG, token: "quiet-env-fallback", on_key: Some("CRATONVM_QUIET_ENV_FALLBACK"), off_key: None, off_word: None, since: "2026-07-30" },
    E { group: Group::DBG, token: "raf-getfd", on_key: Some("CRATONVM_DBG_RAF_GETFD"), off_key: None, off_word: None, since: "2026-05-28" },
    E { group: Group::DBG, token: "raf-init", on_key: Some("CRATONVM_DBG_RAF_INIT"), off_key: None, off_word: None, since: "2026-05-28" },
    E { group: Group::DBG, token: "rbc6", on_key: Some("CRATONVM_DBG_RBC6"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "rbc6-emit", on_key: Some("CRATONVM_DBG_RBC6_EMIT"), off_key: None, off_word: None, since: "2026-08-17" },
    E { group: Group::DBG, token: "re5", on_key: Some("CRATONVM_DBG_RE5"), off_key: None, off_word: None, since: "2026-07-15" },
    E { group: Group::DBG, token: "refersto", on_key: Some("CRATONVM_DBG_REFERSTO"), off_key: None, off_word: None, since: "2026-07-07" },
    E { group: Group::DBG, token: "reflection-factory", on_key: Some("CRATONVM_DBG_REFLECTION_FACTORY"), off_key: None, off_word: None, since: "2026-07-09" },
    E { group: Group::DBG, token: "refproc", on_key: None, off_key: Some("CRATONVM_DBG_NO_REFPROC"), off_word: None, since: "2026-06-09" },
    E { group: Group::DBG, token: "refproc-audit", on_key: Some("CRATONVM_DBG_REFPROC_AUDIT"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::DBG, token: "refproc-remark", on_key: Some("CRATONVM_DBG_REFPROC_REMARK"), off_key: None, off_word: None, since: "2026-07-10" },
    E { group: Group::DBG, token: "remap-trace", on_key: Some("CRATONVM_DBG_REMAP_TRACE"), off_key: None, off_word: None, since: "2026-07-23" },
    E { group: Group::DBG, token: "replovr", on_key: Some("CRATONVM_DBG_REPLOVR"), off_key: None, off_word: None, since: "2026-07-23" },
    E { group: Group::DBG, token: "resolve-shim", on_key: Some("CRATONVM_DBG_RESOLVE_SHIM"), off_key: None, off_word: None, since: "2026-06-19" },
    E { group: Group::DBG, token: "resource-timing", on_key: Some("CRATONVM_DBG_RESOURCE_TIMING"), off_key: None, off_word: None, since: "2026-07-22" },
    E { group: Group::DBG, token: "resume-pc", on_key: Some("CRATONVM_DBG_RESUME_PC"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "retransform", on_key: Some("CRATONVM_DBG_RETRANSFORM"), off_key: None, off_word: None, since: "2026-06-24" },
    E { group: Group::DBG, token: "rootsnap", on_key: Some("CRATONVM_DBG_ROOTSNAP"), off_key: None, off_word: None, since: "2026-06-14" },
    E { group: Group::DBG, token: "rootsnap-every", on_key: Some("CRATONVM_DBG_ROOTSNAP_EVERY"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::DBG, token: "rootsnap-verify", on_key: Some("CRATONVM_DBG_ROOTSNAP_VERIFY"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::DBG, token: "rootprof", on_key: Some("CRATONVM_DBG_ROOTPROF"), off_key: None, off_word: None, since: "2026-08-07" },
    E { group: Group::DBG, token: "rset-audit", on_key: Some("CRATONVM_DBG_RSET_AUDIT"), off_key: None, off_word: None, since: "2026-06-05" },
    E { group: Group::DBG, token: "rset-audit-young-scan", on_key: Some("CRATONVM_DBG_RSET_AUDIT_YOUNG_SCAN"), off_key: None, off_word: None, since: "2026-07-23" },
    E { group: Group::DBG, token: "rterr", on_key: Some("CRATONVM_DBG_RTERR"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::DBG, token: "rvas", on_key: Some("CRATONVM_DBG_RVAS"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::DBG, token: "s111-dbg", on_key: Some("CRATONVM_S111_DBG"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "sbload", on_key: Some("CRATONVM_DBG_SBLOAD"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "sc-close", on_key: Some("CRATONVM_DBG_SC_CLOSE"), off_key: None, off_word: None, since: "2026-07-22" },
    E { group: Group::DBG, token: "sc-read", on_key: Some("CRATONVM_DBG_SC_READ"), off_key: None, off_word: None, since: "2026-07-22" },
    E { group: Group::DBG, token: "sc-write", on_key: Some("CRATONVM_DBG_SC_WRITE"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "scalar-deopt", on_key: Some("CRATONVM_DBG_SCALAR_DEOPT"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::DBG, token: "scalar-new", on_key: Some("CRATONVM_DBG_SCALAR_NEW"), off_key: None, off_word: None, since: "2026-06-20" },
    E { group: Group::DBG, token: "scanner-debug", on_key: Some("CRATONVM_SCANNER_DEBUG"), off_key: None, off_word: None, since: "2026-08-22" },
    E { group: Group::DBG, token: "seed-all-old", on_key: Some("CRATONVM_DBG_SEED_ALL_OLD"), off_key: None, off_word: None, since: "2026-06-14" },
    E { group: Group::DBG, token: "seedhunt", on_key: Some("CRATONVM_DBG_SEEDHUNT"), off_key: None, off_word: None, since: "2026-06-05" },
    E { group: Group::DBG, token: "sel", on_key: Some("CRATONVM_DBG_SEL"), off_key: None, off_word: None, since: "2026-05-28" },
    E { group: Group::DBG, token: "selector", on_key: Some("CRATONVM_DBG_SELECTOR"), off_key: None, off_word: None, since: "2026-06-18" },
    E { group: Group::DBG, token: "setacc", on_key: Some("CRATONVM_DBG_SETACC"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::DBG, token: "sfi-null-trace", on_key: Some("CRATONVM_SFI_NULL_TRACE"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "shadow", on_key: Some("CRATONVM_DBG_SHADOW"), off_key: None, off_word: None, since: "2026-06-04" },
    E { group: Group::DBG, token: "shadow-depth", on_key: Some("CRATONVM_DBG_SHADOW_DEPTH"), off_key: None, off_word: None, since: "2026-06-16" },
    E { group: Group::DBG, token: "shadow-reload", on_key: Some("CRATONVM_DBG_SHADOW_RELOAD"), off_key: None, off_word: None, since: "2026-06-17" },
    E { group: Group::DBG, token: "shadow-sentinel", on_key: Some("CRATONVM_SHADOW_SENTINEL"), off_key: None, off_word: None, since: "2026-06-16" },
    E { group: Group::DBG, token: "shadow-watch", on_key: Some("CRATONVM_SHADOW_WATCH"), off_key: None, off_word: None, since: "2026-06-16" },
    E { group: Group::DBG, token: "shadow2", on_key: Some("CRATONVM_DBG_SHADOW2"), off_key: None, off_word: None, since: "2026-06-04" },
    E { group: Group::DBG, token: "shadow2-filter", on_key: Some("CRATONVM_DBG_SHADOW2_FILTER"), off_key: None, off_word: None, since: "2026-07-01" },
    E { group: Group::DBG, token: "sleep-trace", on_key: Some("CRATONVM_DBG_SLEEP_TRACE"), off_key: None, off_word: None, since: "2026-06-20" },
    E { group: Group::DBG, token: "sock", on_key: Some("CRATONVM_DBG_SOCK"), off_key: None, off_word: None, since: "2026-06-24" },
    E { group: Group::DBG, token: "sock-bytes", on_key: Some("CRATONVM_DBG_SOCK_BYTES"), off_key: None, off_word: None, since: "2026-07-09" },
    E { group: Group::DBG, token: "soe", on_key: Some("CRATONVM_DBG_SOE"), off_key: None, off_word: None, since: "2026-06-12" },
    E { group: Group::DBG, token: "soft-exit", on_key: Some("CRATONVM_SOFT_EXIT"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "sp-stats", on_key: Some("CRATONVM_SP_STATS"), off_key: None, off_word: None, since: "2026-06-04" },
    E { group: Group::DBG, token: "sp-trace", on_key: Some("CRATONVM_SP_TRACE"), off_key: None, off_word: None, since: "2026-06-04" },
    E { group: Group::DBG, token: "sp-verify", on_key: Some("CRATONVM_SP_VERIFY"), off_key: None, off_word: None, since: "2026-06-04" },
    E { group: Group::DBG, token: "spid", on_key: Some("CRATONVM_DBG_SPID"), off_key: None, off_word: None, since: "2026-07-13" },
    E { group: Group::DBG, token: "spring-dbg", on_key: Some("CRATONVM_SPRING_DBG"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "stackless", on_key: Some("CRATONVM_DBG_STACKLESS"), off_key: None, off_word: None, since: "2026-05-31" },
    E { group: Group::DBG, token: "stale-objref", on_key: Some("CRATONVM_DBG_STALE_OBJREF"), off_key: None, off_word: None, since: "2026-07-11" },
    E { group: Group::DBG, token: "stale-objref-cycles", on_key: Some("CRATONVM_DBG_STALE_OBJREF_CYCLES"), off_key: None, off_word: None, since: "2026-07-22" },
    E { group: Group::DBG, token: "stale-recv", on_key: Some("CRATONVM_DBG_STALE_RECV"), off_key: None, off_word: None, since: "2026-05-23" },
    E { group: Group::DBG, token: "stalelong", on_key: Some("CRATONVM_DBG_STALELONG"), off_key: None, off_word: None, since: "2026-06-09" },
    E { group: Group::DBG, token: "stamped", on_key: Some("CRATONVM_DBG_STAMPED"), off_key: None, off_word: None, since: "2026-07-28" },
    E { group: Group::DBG, token: "straystack", on_key: Some("CRATONVM_DBG_STRAYSTACK"), off_key: None, off_word: None, since: "2026-06-05" },
    E { group: Group::DBG, token: "streamsupp", on_key: Some("CRATONVM_DBG_STREAMSUPP"), off_key: None, off_word: None, since: "2026-05-24" },
    E { group: Group::DBG, token: "sttrace", on_key: Some("CRATONVM_DBG_STTRACE"), off_key: None, off_word: None, since: "2026-06-05" },
    E { group: Group::DBG, token: "stub-bt", on_key: Some("CRATONVM_DBG_STUB_BT"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::DBG, token: "stubloader", on_key: Some("CRATONVM_DBG_STUBLOADER"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::DBG, token: "stw-census", on_key: Some("CRATONVM_DBG_STW_CENSUS"), off_key: None, off_word: None, since: "2026-07-05" },
    E { group: Group::DBG, token: "stw-expected-ids", on_key: Some("CRATONVM_DBG_STW_EXPECTED_IDS"), off_key: None, off_word: None, since: "2026-07-13" },
    E { group: Group::DBG, token: "stw-native-ring", on_key: Some("CRATONVM_DBG_STW_NATIVE_RING"), off_key: None, off_word: None, since: "2026-07-05" },
    E { group: Group::DBG, token: "surefire-ipc-dbg", on_key: Some("CRATONVM_SUREFIRE_IPC_DBG"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "swchain", on_key: Some("CRATONVM_DBG_SWCHAIN"), off_key: None, off_word: None, since: "2026-08-20" },
    E { group: Group::DBG, token: "sweep-census", on_key: Some("CRATONVM_DBG_SWEEP_CENSUS"), off_key: None, off_word: None, since: "2026-07-07" },
    E { group: Group::DBG, token: "sweep-edges", on_key: Some("CRATONVM_DBG_SWEEP_EDGES"), off_key: None, off_word: None, since: "2026-06-03" },
    E { group: Group::DBG, token: "unreg-declined", on_key: Some("CRATONVM_DBG_UNREG_DECLINED"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::DBG, token: "sweep-referrers", on_key: Some("CRATONVM_DBG_SWEEP_REFERRERS"), off_key: None, off_word: None, since: "2026-08-03" },
    E { group: Group::DBG, token: "sweep-zero", on_key: Some("CRATONVM_DBG_SWEEP_ZERO"), off_key: None, off_word: None, since: "2026-06-16" },
    E { group: Group::DBG, token: "sweep-trace-class", on_key: Some("CRATONVM_DBG_SWEEP_TRACE_CLASS"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::DBG, token: "symbolize", on_key: Some("CRATONVM_SYMBOLIZE"), off_key: None, off_word: None, since: "2026-06-01" },
    E { group: Group::DBG, token: "symbolize-dbg", on_key: Some("CRATONVM_SYMBOLIZE_DBG"), off_key: None, off_word: None, since: "2026-06-01" },
    E { group: Group::DBG, token: "threadreg-perf", on_key: Some("CRATONVM_DBG_THREADREG_PERF"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "threadstart", on_key: Some("CRATONVM_DBG_THREADSTART"), off_key: None, off_word: None, since: "2026-06-11" },
    E { group: Group::DBG, token: "tier-enqueue", on_key: Some("CRATONVM_DBG_TIER_ENQUEUE"), off_key: None, off_word: None, since: "2026-07-24" },
    E { group: Group::DBG, token: "tlabmiss", on_key: Some("CRATONVM_DBG_TLABMISS"), off_key: None, off_word: None, since: "2026-07-14" },
    E { group: Group::DBG, token: "tls-auth", on_key: Some("CRATONVM_DBG_TLS_AUTH"), off_key: None, off_word: None, since: "2026-07-06" },
    E { group: Group::DBG, token: "tls-hs", on_key: Some("CRATONVM_DBG_TLS_HS"), off_key: None, off_word: None, since: "2026-07-06" },
    E { group: Group::DBG, token: "tls-pls", on_key: Some("CRATONVM_DBG_TLS_PLS"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "tls-sock", on_key: Some("CRATONVM_DBG_TLS_SOCK"), off_key: None, off_word: None, since: "2026-07-08" },
    E { group: Group::DBG, token: "tls-srv", on_key: Some("CRATONVM_DBG_TLS_SRV"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "toarray", on_key: Some("CRATONVM_DBG_TOARRAY"), off_key: None, off_word: None, since: "2026-06-03" },
    E { group: Group::DBG, token: "tohex", on_key: Some("CRATONVM_DBG_TOHEX"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "trace-arrays-hashcode", on_key: Some("CRATONVM_TRACE_ARRAYS_HASHCODE"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "trace-classvalue", on_key: Some("CRATONVM_TRACE_CLASSVALUE"), off_key: None, off_word: None, since: "2026-07-22" },
    E { group: Group::DBG, token: "trace-pti-args", on_key: Some("CRATONVM_TRACE_PTI_ARGS"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "trace-sb-filter", on_key: Some("CRATONVM_TRACE_SB_FILTER"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "trace-unimplemented", on_key: Some("CRATONVM_TRACE_UNIMPLEMENTED"), off_key: None, off_word: None, since: "2026-06-02" },
    E { group: Group::DBG, token: "track-native", on_key: Some("CRATONVM_TRACK_NATIVE"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "trivial-getter-verify", on_key: Some("CRATONVM_TRIVIAL_GETTER_VERIFY"), off_key: None, off_word: None, since: "2026-07-30" },
    E { group: Group::DBG, token: "uclreg", on_key: Some("CRATONVM_DBG_UCLREG"), off_key: None, off_word: None, since: "2026-06-12" },
    E { group: Group::DBG, token: "uclres", on_key: Some("CRATONVM_DBG_UCLRES"), off_key: None, off_word: None, since: "2026-07-23" },
    E { group: Group::DBG, token: "ucltrace", on_key: Some("CRATONVM_DBG_UCLTRACE"), off_key: None, off_word: None, since: "2026-07-26" },
    E { group: Group::DBG, token: "ueh-debug", on_key: Some("CRATONVM_UEH_DEBUG"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "uncaught", on_key: Some("CRATONVM_DBG_UNCAUGHT"), off_key: None, off_word: None, since: "2026-07-08" },
    E { group: Group::DBG, token: "underflow", on_key: Some("CRATONVM_DBG_UNDERFLOW"), off_key: None, off_word: None, since: "2026-06-04" },
    E { group: Group::DBG, token: "unpark-miss", on_key: Some("CRATONVM_DBG_UNPARK_MISS"), off_key: None, off_word: None, since: "2026-07-07" },
    E { group: Group::DBG, token: "unpin-ring", on_key: Some("CRATONVM_DBG_UNPIN_RING"), off_key: None, off_word: None, since: "2026-07-24" },
    E { group: Group::DBG, token: "unroll", on_key: Some("CRATONVM_DBG_UNROLL"), off_key: None, off_word: None, since: "2026-06-18" },
    E { group: Group::DBG, token: "urlcl", on_key: Some("CRATONVM_DBG_URLCL"), off_key: None, off_word: None, since: "2026-05-21" },
    E { group: Group::DBG, token: "ute", on_key: Some("CRATONVM_DBG_UTE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "validate-new", on_key: Some("CRATONVM_DBG_VALIDATE_NEW"), off_key: None, off_word: None, since: "2026-06-03" },
    E { group: Group::DBG, token: "vdisp", on_key: Some("CRATONVM_DBG_VDISP"), off_key: None, off_word: None, since: "2026-06-14" },
    E { group: Group::DBG, token: "vector-intrinsics-stats", on_key: Some("CRATONVM_VECTOR_INTRINSICS_STATS"), off_key: None, off_word: None, since: "2026-08-22" },
    E { group: Group::DBG, token: "verify-error", on_key: Some("CRATONVM_DBG_VERIFY_ERROR"), off_key: None, off_word: None, since: "2026-07-09" },
    E { group: Group::DBG, token: "verify-inline-frame-record", on_key: Some("CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::DBG, token: "verify-oop-maps", on_key: Some("CRATONVM_DBG_VERIFY_OOP_MAPS"), off_key: None, off_word: None, since: "2026-06-04" },
    E { group: Group::DBG, token: "visitfile", on_key: Some("CRATONVM_DBG_VISITFILE"), off_key: None, off_word: None, since: "2026-07-22" },
    E { group: Group::DBG, token: "vm-state", on_key: Some("CRATONVM_DBG_VM_STATE"), off_key: None, off_word: None, since: "2026-07-05" },
    E { group: Group::DBG, token: "watch-cause-self", on_key: Some("CRATONVM_DBG_WATCH_CAUSE_SELF"), off_key: None, off_word: None, since: "2026-07-10" },
    E { group: Group::DBG, token: "watch-cell", on_key: Some("CRATONVM_DBG_WATCH_CELL"), off_key: None, off_word: None, since: "2026-07-03" },
    E { group: Group::DBG, token: "watchaddr", on_key: Some("CRATONVM_DBG_WATCHADDR"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "watch-alloc-cid", on_key: Some("CRATONVM_DBG_WATCH_ALLOC_CID"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::DBG, token: "watchref", on_key: Some("CRATONVM_DBG_WATCHREF"), off_key: None, off_word: None, since: "2026-07-02" },
    E { group: Group::DBG, token: "weakref", on_key: Some("CRATONVM_DBG_WEAKREF"), off_key: None, off_word: None, since: "2026-06-29" },
    E { group: Group::DBG, token: "wf", on_key: Some("CRATONVM_DBG_WF"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "wf-npe", on_key: Some("CRATONVM_DBG_WF_NPE"), off_key: None, off_word: None, since: "2026-05-23" },
    E { group: Group::DBG, token: "xnio-tcp", on_key: Some("CRATONVM_DBG_XNIO_TCP"), off_key: None, off_word: None, since: "2026-07-09" },
    E { group: Group::DBG, token: "xt-jit-root-scan", on_key: Some("CRATONVM_DBG_XT_JIT_ROOT_SCAN"), off_key: None, off_word: None, since: "2026-06-23" },
    E { group: Group::DBG, token: "young-trigger", on_key: Some("CRATONVM_DBG_YOUNG_TRIGGER"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::DBG, token: "youngscan", on_key: Some("CRATONVM_DBG_YOUNGSCAN"), off_key: None, off_word: None, since: "2026-06-05" },
    E { group: Group::DBG, token: "youngstate", on_key: Some("CRATONVM_DBG_YOUNGSTATE"), off_key: None, off_word: None, since: "2026-07-14" },
    E { group: Group::DBG, token: "zero-ranges", on_key: Some("CRATONVM_DBG_ZERO_RANGES"), off_key: None, off_word: None, since: "2026-07-23" },
    E { group: Group::JIT, token: "aaload-licm", on_key: None, off_key: Some("CRATONVM_DISABLE_AALOAD_LICM"), off_word: None, since: "2026-05-22" },
    // Declared 2026-08-06 with the DBG block above. The five `sp-ic-*` /
    // `sp-inline-*` knobs are the single-pass inline-cache bisection surface;
    // three are DEFAULT-ON and read `=0` to disable, so they carry `on_key`
    // and the "0" off-word rather than an `off_key`.
    E { group: Group::JIT, token: "gpu-approx-math", on_key: Some("CRATONVM_GPU_APPROX_MATH"), off_key: None, off_word: None, since: "2026-08-22" },
    // Declared 2026-08-29 with the branch-to-`selp` if-conversion in
    // `jit-cuda/src/lowering/emit.rs`. OPT-IN: the transform does what it
    // was built to do and measured slower on the kernel it was built for,
    // so it is reachable rather than default. The companion knob sets the
    // budget it spends, in weighted PTX instructions per converted arm
    // pair, so one binary can sweep the curve -- picking that number by
    // rebuilding once per point is not possible on a host that moves 2x
    // between two runs.
    // Declared 2026-08-29 with the per-call-site dispatch memo in
    // `vm/src/runtime/offload.rs`. DEFAULT-ON with a "0" off-word: the memo
    // has no observable semantics, so the only honest way to price it is one
    // binary run both ways in the same minutes.
    E { group: Group::JIT, token: "gpu-dispatch-memo", on_key: Some("CRATONVM_GPU_DISPATCH_MEMO"), off_key: None, off_word: Some("0"), since: "2026-08-29" },
    // Consecutive below-min-work refusals after which a CALL SITE is allowed
    // into the invoke cache. Requires `invoke-cache-pc-key`: without it a
    // "site" is a method reference and promoting one caller deoptimises its
    // siblings (measured 18x on GpuHookOverheadBench).
    E { group: Group::GC, token: "gpu-min-work-giveup", on_key: Some("CRATONVM_GPU_MIN_WORK_GIVEUP"), off_key: None, off_word: None, since: "2026-09-03" },
    // Adds the call site's bytecode offset to the invoke-cache key, so two
    // sites invoking the same method stop sharing one entry.
    E { group: Group::JIT, token: "invoke-cache-pc-key", on_key: Some("CRATONVM_INVOKE_CACHE_PC_KEY"), off_key: None, off_word: None, since: "2026-09-03" },
    // Invoke-cache hit/miss counts at exit; the acceptance criterion for
    // `invoke-cache-pc-key`.
    E { group: Group::DBG, token: "invoke-cache-stats", on_key: Some("CRATONVM_INVOKE_CACHE_STATS"), off_key: None, off_word: None, since: "2026-09-03" },
    E { group: Group::JIT, token: "gpu-if-convert", on_key: Some("CRATONVM_GPU_IF_CONVERT"), off_key: None, off_word: None, since: "2026-08-29" },
    E { group: Group::JIT, token: "gpu-if-convert-max-ops", on_key: Some("CRATONVM_GPU_IF_CONVERT_MAX_OPS"), off_key: None, off_word: None, since: "2026-08-29" },
    E { group: Group::JIT, token: "sp-ic-deny", on_key: Some("CRATONVM_JIT_SP_IC_DENY"), off_key: None, off_word: None, since: "2026-08-05" },
    // A/B lever, never a supported configuration: restore the pre-fix
    // substitution of "slot 0, tagged int" for a field site the VM-side
    // resolver declined. Declared so the arm can be spelled, and so that
    // `flags` reports it as SET when somebody leaves it on by accident.
    E { group: Group::JIT, token: "unresolved-field-substitute", on_key: Some("CRATONVM_JIT_UNRESOLVED_FIELD_SUBSTITUTE"), off_key: None, off_word: None, since: "2026-08-27" },
    E { group: Group::JIT, token: "sp-ic-deopt-check", on_key: Some("CRATONVM_JIT_SP_IC_DEOPT_CHECK"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::JIT, token: "sp-ic-only", on_key: Some("CRATONVM_JIT_SP_IC_ONLY"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::JIT, token: "sp-inline-mega", on_key: Some("CRATONVM_JIT_SP_INLINE_MEGA"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::JIT, token: "sp-inline-mic", on_key: Some("CRATONVM_JIT_SP_INLINE_MIC"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::JIT, token: "sp-inline-pic", on_key: Some("CRATONVM_JIT_SP_INLINE_PIC"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::JIT, token: "unreg-memo-gc-reset", on_key: Some("CRATONVM_JIT_UNREG_MEMO_GC_RESET"), off_key: None, off_word: None, since: "2026-08-06" },
    E { group: Group::JIT, token: "frame-bands", on_key: None, off_key: Some("CRATONVM_JIT_NO_FRAME_BANDS"), off_word: None, since: "2026-08-20" },
    E { group: Group::JIT, token: "oopmap-coverage-presence-only", on_key: Some("CRATONVM_JIT_OOPMAP_COVERAGE_PRESENCE_ONLY"), off_key: None, off_word: None, since: "2026-08-20" },
    E { group: Group::GC, token: "moving-young-bounds-guard", on_key: None, off_key: Some("CRATONVM_MOVING_YOUNG_NO_BOUNDS_GUARD"), off_word: None, since: "2026-08-20" },
    E { group: Group::GC, token: "moving-young-band-object-screen", on_key: None, off_key: Some("CRATONVM_MOVING_YOUNG_NO_BAND_OBJECT_SCREEN"), off_word: None, since: "2026-09-03" },
    E { group: Group::GC, token: "moving-young-band-liveness-screen", on_key: None, off_key: Some("CRATONVM_MOVING_YOUNG_NO_BAND_LIVENESS_SCREEN"), off_word: None, since: "2026-09-03" },
    E { group: Group::GC, token: "moving-young-band-thread-window", on_key: None, off_key: Some("CRATONVM_MOVING_YOUNG_NO_BAND_THREAD_WINDOW"), off_word: None, since: "2026-09-04" },
    E { group: Group::JIT, token: "unreg-accept-residue", on_key: Some("CRATONVM_JIT_UNREG_ACCEPT_RESIDUE"), off_key: None, off_word: None, since: "2026-08-07" },
    E { group: Group::JIT, token: "a5-residue-filter", on_key: Some("CRATONVM_JIT_A5_RESIDUE_FILTER"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::JIT, token: "a5-shape-filter", on_key: Some("CRATONVM_JIT_A5_SHAPE_FILTER"), off_key: None, off_word: None, since: "2026-09-06" },
    // A/B opt-in restoring the pre-2026-07-31 single global `Mutex` in
    // `types::jit_activation`; presence-parsed (`runtime_var_os(..).is_some()`),
    // so `=0` still enables it and `off_word` must stay `None`.
    E { group: Group::JIT, token: "activation-global-mutex", on_key: Some("CRATONVM_JIT_ACTIVATION_GLOBAL_MUTEX"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::JIT, token: "alloc-class-cache", on_key: None, off_key: Some("CRATONVM_NO_JIT_ALLOC_CLASS_CACHE"), off_word: None, since: "2026-07-10" },
    // Sinking the per-safepoint blind GPR spill at an inline-TLAB `new` onto
    // the allocation's slow-path edges. Presence-parsed opt-out, so `=0` still
    // disables the sink and `off_word` must stay `None`.
    E { group: Group::JIT, token: "alloc-spill-sink", on_key: None, off_key: Some("CRATONVM_JIT_NO_ALLOC_SPILL_SINK"), off_word: None, since: "2026-08-06" },
    E { group: Group::JIT, token: "arith-licm", on_key: None, off_key: Some("CRATONVM_DISABLE_ARITH_LICM"), off_word: None, since: "2026-06-16" },
    // Declared 2026-09-02 with the three codegen changes of
    // `array-element-load-baseline-codegen-20260901`. All three are default-ON
    // and all three change the EMITTED BYTES of code that runs on every
    // iteration of every array loop, so each gets its own lever at the level
    // the change is at.
    E { group: Group::JIT, token: "arraylen-licm", on_key: None, off_key: Some("CRATONVM_DISABLE_ARRAYLEN_LICM"), off_word: None, since: "2026-09-02" },
    E { group: Group::JIT, token: "rip-safepoint-poll", on_key: Some("CRATONVM_JIT_RIP_SAFEPOINT_POLL"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "fused-bounds-load", on_key: Some("CRATONVM_JIT_FUSED_BOUNDS_LOAD"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "bce", on_key: None, off_key: Some("CRATONVM_JIT_NO_BCE"), off_word: None, since: "2026-06-03" },
    E { group: Group::JIT, token: "bg-compile", on_key: Some("CRATONVM_BG_COMPILE"), off_key: None, off_word: None, since: "2026-06-18" },
    // The bytecode loop rewriter (`x64::plan_bytecode_loop_xform`: peel,
    // unroll, guarded versioning). Default-**OFF**, and arming it also turns
    // the native byte-copy unroller off — the two are exact complements, and
    // both firing on one loop would put `(k+1)^2` bodies behind one back-edge
    // poll. Sufficient on its own since `loop-02` retired the `deopt-real`
    // whole-compile refusal: this token alone now reaches loops under the
    // default configuration, and pairing it with `-deopt-real` measures the
    // deopt-real-off configuration rather than the transform. See
    // `docs/jit/loop-rewriter-wiring.md`.
    E { group: Group::JIT, token: "bytecode-loop-xform", on_key: Some("CRATONVM_JIT_BYTECODE_LOOP_XFORM"), off_key: None, off_word: None, since: "2026-08-03" },
    // Default-ON: `x64::escape_analysis::bulk_byte_loops_enabled` reads
    // `0`/`false`/`off` (trimmed, case-insensitive) as the kill switch.
    E { group: Group::JIT, token: "bulk-byte-loops", on_key: Some("CRATONVM_JIT_BULK_BYTE_LOOPS"), off_key: None, off_word: Some("0"), since: "2026-07-30" },
    // Default-ON (PERF-01): the optimizing tier declines a method whose loops
    // the single-pass backend would VECTORISE, because an IR body for one of
    // those is a downgrade — measured at 6.4x on `CratonBench.sieve`.
    // `CRATONVM_JIT='-c1-vector-veto'` hands those methods back to the IR
    // tier, which is how the regression is reproduced and how a probe that
    // wants IR codegen for a sieve-shaped method gets it (this term sits
    // AFTER `CRATONVM_JIT_FORCE_C2` in the admission chain).
    E { group: Group::JIT, token: "c1-vector-veto", on_key: Some("CRATONVM_JIT_C1_VECTOR_VETO"), off_key: None, off_word: Some("0"), since: "2026-08-04" },
    E { group: Group::JIT, token: "census-direct-helpers", on_key: Some("CRATONVM_JIT_CENSUS_DIRECT_HELPERS"), off_key: None, off_word: Some("0"), since: "2026-08-17" },
    E { group: Group::JIT, token: "nio-byte-direct-helpers", on_key: Some("CRATONVM_JIT_NIO_BYTE_DIRECT_HELPERS"), off_key: None, off_word: Some("0"), since: "2026-08-28" },
    E { group: Group::JIT, token: "md-update-direct-helper", on_key: Some("CRATONVM_JIT_MD_UPDATE_DIRECT_HELPER"), off_key: None, off_word: Some("0"), since: "2026-08-28" },
    E { group: Group::JIT, token: "cached-entry-owner-reuse", on_key: Some("CRATONVM_JIT_CACHED_ENTRY_OWNER_REUSE"), off_key: None, off_word: Some("0"), since: "2026-08-17" },
    E { group: Group::JIT, token: "c2-first-call", on_key: Some("CRATONVM_JIT_C2_FIRST_CALL"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::JIT, token: "c2-supersede", on_key: Some("CRATONVM_C2_SUPERSEDE"), off_key: None, off_word: Some("0"), since: "2026-07-06" },
    E { group: Group::JIT, token: "callee-oop-flush", on_key: None, off_key: Some("CRATONVM_JIT_NO_CALLEE_OOP_FLUSH"), off_word: None, since: "2026-06-21" },
    E { group: Group::JIT, token: "spill-slots-cap", on_key: Some("CRATONVM_JIT_SPILL_SLOTS_CAP"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::JIT, token: "inline-reserve-path", on_key: None, off_key: Some("CRATONVM_JIT_NO_INLINE_RESERVE_PATH"), off_word: None, since: "2026-09-02" },
    E { group: Group::JIT, token: "code-cache-max-mb", on_key: Some("CRATONVM_JIT_CODE_CACHE_MAX_MB"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::JIT, token: "conservative-locals", on_key: None, off_key: Some("CRATONVM_NO_CONSERVATIVE_LOCALS"), off_word: None, since: "2026-06-16" },
    E { group: Group::JIT, token: "ctor-direct-call", on_key: None, off_key: Some("CRATONVM_NO_CTOR_DIRECT_CALL"), off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "osr-ctor-bind", on_key: None, off_key: Some("CRATONVM_NO_OSR_CTOR_BIND"), off_word: None, since: "2026-08-17" },
    E { group: Group::JIT, token: "real-new-site-flags", on_key: Some("CRATONVM_JIT_REAL_NEW_SITE_FLAGS"), off_key: None, off_word: None, since: "2026-08-17" },
    E { group: Group::JIT, token: "deny", on_key: Some("CRATONVM_JIT_DENY"), off_key: None, off_word: None, since: "2026-07-03" },
    E { group: Group::JIT, token: "deopt-real", on_key: Some("CRATONVM_DEOPT_REAL"), off_key: None, off_word: None, since: "2026-06-17" },
    E { group: Group::JIT, token: "direct-callee-calls", on_key: Some("CRATONVM_JIT_DIRECT_CALLEE_CALLS"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::JIT, token: "dispatch-cache-direct-entry", on_key: Some("CRATONVM_JIT_DISPATCH_CACHE_DIRECT_ENTRY"), off_key: None, off_word: None, since: "2026-07-22" },
    E { group: Group::JIT, token: "dispatch-cache-virtual-direct-entry", on_key: Some("CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::JIT, token: "string-intrinsic-pin", on_key: None, off_key: Some("CRATONVM_JIT_NO_STRING_INTRINSIC_PIN"), off_word: None, since: "2026-08-13" },
    // The pin's BLIND case, split out so it can be A/B'd on its own. The pin
    // above answers "keep this method off the optimizing tier"; this one
    // answers what to do when the compile door supplied no constant-pool
    // invoke resolver, so the pin cannot read the callee names it needs.
    // Default-ON means fail CLOSED (stay interpreted); the key's PRESENCE
    // restores the historical fail-open promotion to a tier that has no
    // String intrinsic emitter at all. Read by
    // `jit::string_pin_fail_closed_enabled`.
    E { group: Group::JIT, token: "string-pin-fail-closed", on_key: None, off_key: Some("CRATONVM_JIT_NO_STRING_PIN_FAIL_CLOSED"), off_word: None, since: "2026-09-01" },
    E { group: Group::JIT, token: "dup-x1", on_key: None, off_key: Some("CRATONVM_JIT_NO_DUP_X1"), off_word: None, since: "2026-06-16" },
    E { group: Group::JIT, token: "dup-x2", on_key: None, off_key: Some("CRATONVM_JIT_NO_DUP_X2"), off_word: None, since: "2026-06-16" },
    E { group: Group::JIT, token: "dup2-x2", on_key: None, off_key: Some("CRATONVM_JIT_NO_DUP2_X2"), off_word: None, since: "2026-08-17" },
    E { group: Group::JIT, token: "trusted-oop-getfield", on_key: None, off_key: Some("CRATONVM_JIT_NO_TRUSTED_OOP_GETFIELD"), off_word: None, since: "2026-08-18" },
    E { group: Group::JIT, token: "dupx", on_key: None, off_key: Some("CRATONVM_JIT_NO_DUPX"), off_word: None, since: "2026-06-11" },
    E { group: Group::JIT, token: "dupx-eager-canon", on_key: Some("CRATONVM_JIT_DUPX_EAGER_CANON"), off_key: None, off_word: None, since: "2026-06-16" },
    // Transitive eager callee compilation, so a body compiled bottom-up binds its
    // statically bound call sites to raw CALLs instead of the generic dispatch
    // helper. Default ON; `CRATONVM_JIT='-eager-callee-chain'` restores the
    // one-level behaviour, which is the A/B control for the ~6.5x measured on a
    // call-dense loop. Correct either way — the fallback is the checked helper.
    E { group: Group::JIT, token: "eager-callee-chain", on_key: Some("CRATONVM_JIT_EAGER_CALLEE_CHAIN"), off_key: None, off_word: Some("0"), since: "2026-08-17" },
    E { group: Group::JIT, token: "enable-callee-saved-gpr-locals", on_key: Some("CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS"), off_key: None, off_word: None, since: "2026-07-03" },
    E { group: Group::JIT, token: "enable-inline-new", on_key: Some("CRATONVM_JIT_ENABLE_INLINE_NEW"), off_key: None, off_word: None, since: "2026-07-12" },
    E { group: Group::JIT, token: "exc-table-c2", on_key: None, off_key: Some("CRATONVM_JIT_NO_EXC_TABLE_C2"), off_word: None, since: "2026-07-27" },
    E { group: Group::JIT, token: "force-c2", on_key: Some("CRATONVM_JIT_FORCE_C2"), off_key: None, off_word: None, since: "2026-07-31" },
    // The class-blind arm of the native-shadow seal. Default ON (correctness
    // guard); `-native-shadow-interface-blind` measures its cost.
    E { group: Group::JIT, token: "native-shadow-interface-blind", on_key: Some("CRATONVM_JIT_NATIVE_SHADOW_INTERFACE_BLIND"), off_key: None, off_word: Some("0"), since: "2026-08-10" },
    // The whole native-shadow caller seal. Default ON and load-bearing for
    // CORRECTNESS; off is a measurement configuration only, for pricing the
    // seal's ceiling. Never ship with it off.
    E { group: Group::JIT, token: "native-shadow-caller-seal", on_key: Some("CRATONVM_JIT_NATIVE_SHADOW_CALLER_SEAL"), off_key: None, off_word: Some("0"), since: "2026-08-10" },
    // The POSITIVE half of `execute()`'s static-eligibility short-circuit.
    // Default ON; `-gate-pass-memo` restores the pre-fix re-run-every-entry
    // behaviour so the fix can be A/B'd in one binary. Correct either way.
    E { group: Group::JIT, token: "gate-pass-memo", on_key: Some("CRATONVM_JIT_GATE_PASS_MEMO"), off_key: None, off_word: Some("0"), since: "2026-08-12" },
    E { group: Group::JIT, token: "full-self-call-spill", on_key: Some("CRATONVM_JIT_FULL_SELF_CALL_SPILL"), off_key: None, off_word: None, since: "2026-07-14" },
    // Default-ON: `x64::licm::gc_inert_selfrec_enabled` reads `0`/`false`/`off`.
    E { group: Group::JIT, token: "gc-inert-selfrec", on_key: Some("CRATONVM_JIT_GC_INERT_SELFREC"), off_key: None, off_word: Some("0"), since: "2026-07-30" },
    E { group: Group::JIT, token: "getfield-helper", on_key: Some("CRATONVM_JIT_GETFIELD_HELPER"), off_key: None, off_word: None, since: "2026-07-10" },
    // Presence-parsed kill switch for the inline (helper-free) compiled
    // `getstatic` load, exactly like `getfield-helper` above: setting it
    // routes every static read back through `jit_getstatic`.
    E { group: Group::JIT, token: "getstatic-helper", on_key: Some("CRATONVM_JIT_GETSTATIC_HELPER"), off_key: None, off_word: None, since: "2026-08-03" },
    // PGO-02: guarded monomorphic-virtual-call inlining (splice the callee
    // body behind a receiver class-id guard, miss falls to normal dispatch,
    // never a deopt). Default-OFF until soaked — see
    // docs/feature-designs/profile-guided-inlining.md.
    E { group: Group::JIT, token: "guarded-virtual-inline", on_key: Some("CRATONVM_JIT_GUARDED_VIRTUAL_INLINE"), off_key: None, off_word: Some("0"), since: "2026-08-03" },
    E { group: Group::JIT, token: "helpful-npe-opcodes", on_key: Some("CRATONVM_HELPFUL_NPE_OPCODES"), off_key: None, off_word: None, since: "2026-06-18" },
    E { group: Group::JIT, token: "inclusive-bce", on_key: Some("CRATONVM_JIT_INCLUSIVE_BCE"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::JIT, token: "inline-allow-static", on_key: Some("CRATONVM_INLINE_ALLOW_STATIC"), off_key: None, off_word: None, since: "2026-06-13" },
    E { group: Group::JIT, token: "inline-getfield", on_key: Some("CRATONVM_JIT_INLINE_GETFIELD"), off_key: None, off_word: None, since: "2026-07-09" },
    // Declared 2026-09-02. Two OPT-OUTs from the same measurement, both
    // default ON.
    //
    // `string-access-inline-rows`: the IR String-access expansion's `value`
    // and `coder` reads sit at an INVOKE pc, and the inline compact-getfield
    // table is keyed by GETFIELD pc — so both fell back to the checked
    // `jit_getfield` helper, 917,203,334 CALLs in one
    // `probes/CharAtCostCurve.java` run. `=1` restores that fallback.
    //
    // `licm-read-hoist`: `ir_optimize::licm` treated `Op::Guard`,
    // `Op::ArrayLoad` and `Op::ArrayLength` as arbitrary-memory barriers, so
    // the expansion disqualified its own loop from every hoist and re-read
    // both fields per character. `=1` restores that refusal.
    E { group: Group::JIT, token: "string-access-inline-rows", on_key: None, off_key: Some("CRATONVM_JIT_NO_STRING_ACCESS_INLINE_ROWS"), off_word: None, since: "2026-09-02" },
    E { group: Group::JIT, token: "licm-read-hoist", on_key: None, off_key: Some("CRATONVM_JIT_NO_LICM_READ_HOIST"), off_word: None, since: "2026-09-02" },
    E { group: Group::JIT, token: "inline-live-slot-clamp", on_key: None, off_key: Some("CRATONVM_JIT_NO_INLINE_LIVE_SLOT_CLAMP"), off_word: None, since: "2026-08-24" },
    E { group: Group::JIT, token: "inline-new", on_key: None, off_key: Some("CRATONVM_JIT_DISABLE_INLINE_NEW"), off_word: None, since: "2026-05-28" },
    E { group: Group::JIT, token: "inline-putfield", on_key: None, off_key: Some("CRATONVM_NO_JIT_INLINE_PUTFIELD"), off_word: None, since: "2026-07-24" },
    E { group: Group::JIT, token: "inline-self-guard", on_key: Some("CRATONVM_JIT_INLINE_SELF_GUARD"), off_key: None, off_word: None, since: "2026-07-10" },
    E { group: Group::JIT, token: "inline-tlab-new", on_key: None, off_key: Some("CRATONVM_NO_JIT_INLINE_TLAB_NEW"), off_word: None, since: "2026-07-24" },
    E { group: Group::JIT, token: "intrinsics", on_key: None, off_key: Some("CRATONVM_DISABLE_INTRINSICS"), off_word: None, since: "2026-05-22" },
    E { group: Group::JIT, token: "ir-branchy", on_key: None, off_key: Some("CRATONVM_NO_IR_BRANCHY"), off_word: None, since: "2026-06-18" },
    E { group: Group::JIT, token: "ir-call", on_key: Some("CRATONVM_JIT_IR_CALL"), off_key: None, off_word: None, since: "2026-06-20" },
    E { group: Group::JIT, token: "ir-call-special", on_key: Some("CRATONVM_JIT_IR_CALL_SPECIAL"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::JIT, token: "ir-call-virtual", on_key: Some("CRATONVM_JIT_IR_CALL_VIRTUAL"), off_key: None, off_word: Some("0"), since: "2026-06-21" },
    E { group: Group::JIT, token: "ir-over-intrinsic", on_key: Some("CRATONVM_JIT_IR_OVER_INTRINSIC"), off_key: None, off_word: None, since: "2026-08-17" },
    // Default-ON, `"0"` turns it off: the optimizing tier's String access
    // expander (`length`/`isEmpty`/`charAt` lowered to IR nodes rather than
    // dispatched). ON is the honest default even though the emitter is
    // normally unreachable -- `string-intrinsic-pin`, also default-ON, keeps
    // any method with a String-intrinsic site on the single-pass backend -- so
    // a default-OFF spelling would be dead twice over. The A/B that means
    // something is `-string-intrinsic-pin` against
    // `-string-intrinsic-pin,-ir-string-intrinsics`, with the pinned default as
    // the third reading.
    E { group: Group::JIT, token: "ir-string-intrinsics", on_key: Some("CRATONVM_JIT_IR_STRING_INTRINSICS"), off_key: None, off_word: Some("0"), since: "2026-09-01" },
    // The expander's census, printed on every decision and cumulative, so the
    // last line is the exit total. Cached in a `OnceLock` at the read site:
    // an uncached per-site flag read is how `CRATONVM_DBG_COMPACT_INLINE`
    // came to be 99.1% of all flag reads on a BigDecimal run.
    E { group: Group::DBG, token: "ir-string", on_key: Some("CRATONVM_DBG_IR_STRING"), off_key: None, off_word: None, since: "2026-09-01" },
    E { group: Group::JIT, token: "ir-deopt-resume", on_key: Some("CRATONVM_IR_DEOPT_RESUME"), off_key: None, off_word: None, since: "2026-06-18" },
    E { group: Group::JIT, token: "ir-direct-call", on_key: Some("CRATONVM_JIT_IR_DIRECT_CALL"), off_key: None, off_word: None, since: "2026-07-25" },
    // A/B opt-out: `-ir-buffer-estimate` restores the legacy code-buffer sizing
    // (`ir_lower.rs`), whose under-estimate floods "code buffer estimate too
    // small" bails. Declared as the OFF half of a default-on knob, so the
    // supported spelling is `CRATONVM_JIT=-ir-buffer-estimate`.
    E { group: Group::JIT, token: "ir-buffer-estimate", on_key: None, off_key: Some("CRATONVM_JIT_IR_LEGACY_BUFFER_ESTIMATE"), off_word: None, since: "2026-08-01" },
    E { group: Group::JIT, token: "ir-fp", on_key: Some("CRATONVM_JIT_IR_FP"), off_key: None, off_word: None, since: "2026-06-21" },
    // Default-**OFF**, unlike its neighbour `ir-reloc-emit`. `ir_lower::
    // linear_scan_enabled` answers `false` for `Err(_)`, so unsetting the key is
    // already the off state and `off_word` stays `None`; writing `"0"` here
    // would work too but would mislabel the row as a default-ON knob, which is
    // the one thing this table is supposed to state unambiguously.
    E { group: Group::JIT, token: "ir-isel-shadow", on_key: Some("CRATONVM_JIT_IR_ISEL_SHADOW"), off_key: None, off_word: None, since: "2026-08-03" },
    // Increment 2 of the machine level. `ir-isel-emit` makes the selector's
    // tiles the emitted bytes; `ir-isel-verify` builds the same machine list and
    // checks it against the per-opcode arms byte for byte WITHOUT emitting it.
    // Both default-OFF for the same reason as `ir-isel-shadow` above:
    // `ir_lower::isel_emit_enabled` / `isel_verify_enabled` answer `false` for
    // `Err(_)`, so unsetting the key is already the off state.
    E { group: Group::JIT, token: "ir-isel-emit", on_key: Some("CRATONVM_JIT_IR_ISEL_EMIT"), off_key: None, off_word: None, since: "2026-08-04" },
    E { group: Group::JIT, token: "ir-isel-verify", on_key: Some("CRATONVM_JIT_IR_ISEL_VERIFY"), off_key: None, off_word: None, since: "2026-08-04" },
    E { group: Group::JIT, token: "precise-field-ops", on_key: None, off_key: Some("CRATONVM_JIT_NO_PRECISE_FIELD_OPS"), off_word: None, since: "2026-08-02" },
    // Declared 2026-08-11 with the pre-push hook that would have caught it.
    // `jit::precise_getstatic_checkcast_enabled` withdraws the RBC.6 admission
    // of `getstatic`/`checkcast` so one binary can be A/B'd against its own
    // pre-change behaviour. Opt-out spelling only, same as its
    // `precise-field-ops` neighbour, so the token is stated positively and
    // enabling it means removing the key. The read site already goes through
    // `runtime_var_os`, which serves a DECLARED name from the latched snapshot
    // and only falls through to a live `getenv` for an undeclared one — so
    // this row is the whole fix.
    E { group: Group::JIT, token: "precise-getstatic-checkcast", on_key: None, off_key: Some("CRATONVM_JIT_NO_PRECISE_GETSTATIC_CHECKCAST"), off_word: None, since: "2026-08-11" },
    E { group: Group::JIT, token: "precise-alloc-athrow", on_key: None, off_key: Some("CRATONVM_JIT_NO_PRECISE_ALLOC_ATHROW"), off_word: None, since: "2026-08-17" },
    // `invokedynamic` (0xba). Bookkeeping, not a new lowering: the bridged arm
    // already runs `emit_post_invoke_exception_check` and every other arm
    // deopts unconditionally before the call. It was keeping
    // `MVMap.flushAppendBuffer` -- 15.2% of CPU on a contended H2 workload --
    // permanently interpreted.
    E { group: Group::JIT, token: "precise-indy", on_key: None, off_key: Some("CRATONVM_JIT_NO_PRECISE_INDY"), off_word: None, since: "2026-09-06" },
    // Opt-in. The GP register file landed beside the FP one on 2026-09-02, but
    // the flip still wants a wall-clock measurement -- see
    // `ir_lower::linear_scan_enabled`. `since` stays 2026-08-01: the flag is the
    // same flag, and this column dates the KNOB, not its capability.
    E { group: Group::JIT, token: "ir-linear-scan", on_key: Some("CRATONVM_JIT_IR_LINEAR_SCAN"), off_key: None, off_word: Some("0"), since: "2026-08-01" },
    // Default-ON A/B levers: `ir_lower` reads `0`/`false` on both. The inline
    // TLAB bump is unreachable until `CRATONVM_JIT_C2_ALLOC_UPGRADE` opens the
    // optimizing tier to allocation-bearing methods, which is what having it
    // makes possible.
    E { group: Group::JIT, token: "ir-inline-tlab", on_key: Some("CRATONVM_JIT_IR_INLINE_TLAB"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::JIT, token: "tls-thread-fetch", on_key: Some("CRATONVM_JIT_TLS_THREAD_FETCH"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "ir-receiver-guard-cse", on_key: Some("CRATONVM_JIT_IR_RECEIVER_GUARD_CSE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "ir-residency-pays", on_key: Some("CRATONVM_JIT_IR_RESIDENCY_PAYS"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "ir-fused-branch", on_key: Some("CRATONVM_JIT_IR_FUSED_BRANCH"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "ir-const-imm", on_key: Some("CRATONVM_JIT_IR_CONST_IMM"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "ir-phi-residency", on_key: Some("CRATONVM_JIT_IR_PHI_RESIDENCY"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "ir-phi-copy-regs", on_key: Some("CRATONVM_JIT_IR_PHI_COPY_REGS"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    E { group: Group::JIT, token: "ir-phi-edge-interfere", on_key: Some("CRATONVM_JIT_IR_PHI_EDGE_INTERFERE"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-skip-republish", on_key: Some("CRATONVM_JIT_IR_SKIP_REPUBLISH"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    E { group: Group::JIT, token: "ir-deopt-regs", on_key: Some("CRATONVM_JIT_IR_DEOPT_REGS"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    E { group: Group::JIT, token: "ir-osr-entry", on_key: Some("CRATONVM_JIT_IR_OSR_ENTRY"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    E { group: Group::JIT, token: "ls-carry-relief", on_key: Some("CRATONVM_JIT_LS_CARRY_RELIEF"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    E { group: Group::JIT, token: "ir-reserve-carried", on_key: Some("CRATONVM_JIT_IR_RESERVE_CARRIED"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    E { group: Group::JIT, token: "osr-optimizing", on_key: Some("CRATONVM_JIT_OSR_OPTIMIZING"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    E { group: Group::JIT, token: "osr-optimizing-memo", on_key: Some("CRATONVM_JIT_OSR_OPTIMIZING_MEMO"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-drop-phi-home", on_key: Some("CRATONVM_JIT_IR_DROP_PHI_HOME"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    E { group: Group::JIT, token: "ir-publish-at-def", on_key: Some("CRATONVM_JIT_IR_PUBLISH_AT_DEF"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    E { group: Group::JIT, token: "ir-drop-home", on_key: Some("CRATONVM_JIT_IR_DROP_HOME"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    E { group: Group::JIT, token: "ir-drop-unreachable-homes", on_key: Some("CRATONVM_JIT_IR_DROP_UNREACHABLE_HOMES"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-carry-single-use", on_key: Some("CRATONVM_JIT_IR_CARRY_SINGLE_USE"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    E { group: Group::JIT, token: "ir-sink-late", on_key: Some("CRATONVM_JIT_IR_SINK_LATE"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    E { group: Group::JIT, token: "ir-alu-imm", on_key: Some("CRATONVM_JIT_IR_ALU_IMM"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    E { group: Group::JIT, token: "merged-call-sentinel", on_key: Some("CRATONVM_JIT_MERGED_CALL_SENTINEL"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "ir-cold-arg-stage", on_key: Some("CRATONVM_JIT_IR_COLD_ARG_STAGE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "ir-long", on_key: Some("CRATONVM_JIT_IR_LONG"), off_key: None, off_word: None, since: "2026-06-21" },
    // Default-ON A/B lever: `x64::gated_ref_store_enabled` reads `0`. Its
    // predecessor `CRATONVM_NO_JIT_INLINE_PUTFIELD` measured exactly zero under
    // the default collector, because the path it disabled was already
    // unreachable there.
    E { group: Group::JIT, token: "gated-ref-store", on_key: Some("CRATONVM_JIT_GATED_REF_STORE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    // The same gates on the OPTIMIZING tier, which had no arm to ask with until
    // 2026-09-02 and so reported neither gated nor declined. A separate lever
    // from the one above on purpose: one switch covering both emitters could not
    // separate "the plan is wrong" from "this emitter is wrong".
    E { group: Group::JIT, token: "ir-ref-store", on_key: Some("CRATONVM_JIT_IR_REF_STORE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    // Diagnostic-only: the DYNAMIC split between the gated arm's inline path
    // and its helper fallback. The compile-time census counts emitted
    // sequences, which is not the same fact.
    E { group: Group::JIT, token: "dbg-ir-ref-store-trace", on_key: Some("CRATONVM_DBG_IR_REF_STORE_TRACE"), off_key: None, off_word: None, since: "2026-09-02" },
    // The single-pass twin. Two switches rather than one so a run can trace one
    // tier at a time when both arms are engaged.
    E { group: Group::JIT, token: "dbg-sp-ref-store-trace", on_key: Some("CRATONVM_DBG_SP_REF_STORE_TRACE"), off_key: None, off_word: None, since: "2026-09-02" },
    // Seeds `this` non-null at method entry. Kept separate from
    // `receiver-null-elim` below because the blast radii differ: this one
    // widens a fact THREE consumers already read (array null-check elision,
    // ifnull/ifnonnull branch elision, and the getfield receiver guard), while
    // that one only adds the third consumer. One switch for both would have
    // made them indistinguishable in a bisect.
    E { group: Group::JIT, token: "this-nonnull", on_key: Some("CRATONVM_JIT_THIS_NONNULL"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    // The optimizing tier's half of the same fact, reached by a different
    // route (the graph's `receiver_param`, not a bytecode dataflow). Separate
    // so a bisect can say which tier moved.
    E { group: Group::JIT, token: "ir-this-nonnull", on_key: Some("CRATONVM_JIT_IR_THIS_NONNULL"), off_key: None, off_word: None, since: "2026-09-03" },
    // Drops the getfield receiver TEST/JZ where the dataflow proves it dead.
    E { group: Group::JIT, token: "receiver-null-elim", on_key: Some("CRATONVM_JIT_RECEIVER_NULL_ELIM"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    // DEFAULT-ON since its soak. Still the only switch in this backend whose
    // wrong arm is SILENT -- a stale or mis-shaped entry does not produce a
    // wrong answer, it resumes execution at an address the table chose -- which
    // is why the `0` opt-out stays: taking this mechanism out of the picture in
    // one run, on the same binary, is the first move when debugging an
    // unexplained crash in compiled code.
    E { group: Group::JIT, token: "implicit-null-check", on_key: Some("CRATONVM_JIT_IMPLICIT_NULL_CHECK"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    // OPT-IN, and known to miscompile until the ARG_REGS audit lands -- see
    // `x64::operand_cache_enabled`. Declared so the two arms are measurable in
    // one binary, which is what the previous shape (no flag at all) prevented.
    E { group: Group::JIT, token: "operand-cache", on_key: Some("CRATONVM_JIT_OPERAND_CACHE"), off_key: None, off_word: None, since: "2026-09-02" },
    // Default-ON A/B lever: `ir_lower::reloc_emit_enabled` reads `0`/`false`.
    E { group: Group::JIT, token: "ir-reloc-emit", on_key: Some("CRATONVM_JIT_IR_RELOC_EMIT"), off_key: None, off_word: Some("0"), since: "2026-07-31" },
    // Declared 2026-08-30 with the relocation-gate coupling. Default-ON, so a
    // KILL SWITCH: `=0` restores the pre-fix behaviour in which a safepoint map
    // `record_oop_map` had ALREADY judged short was still published as
    // `moving_young_coverage_complete`, so relocation rewrote the slots it
    // named and left the rest pointing into from-space. Kept because the fix
    // has a measured cost -- on String-heavy code every cycle meeting a live
    // compiled frame now declines to relocate -- and that cost reaches
    // `org.h2.test.store.TestMVStoreTool`, an already-open fragmentation OOM,
    // about 10x sooner. This is the same-binary A/B for both halves of that
    // trade. See `x64::safepoint::relocation_coverage_complete`.
    E { group: Group::JIT, token: "reloc-gate-map-incomplete", on_key: Some("CRATONVM_JIT_RELOC_GATE_ON_MAP_INCOMPLETE"), off_key: None, off_word: Some("0"), since: "2026-08-30" },
    E { group: Group::JIT, token: "ir-selfrec-direct", on_key: Some("CRATONVM_JIT_IR_SELFREC_DIRECT"), off_key: None, off_word: None, since: "2026-06-22" },
    // Default-ON: `conservative_roots::nested_trace_frames_enabled` treats the
    // key's PRESENCE as "restore the one-frame-per-chain-entry answer".
    E { group: Group::JIT, token: "nested-trace-frames", on_key: None, off_key: Some("CRATONVM_JIT_NO_NESTED_TRACE_FRAMES"), off_word: None, since: "2026-08-18" },
    E { group: Group::JIT, token: "npe-frame-snapshot", on_key: None, off_key: Some("CRATONVM_JIT_NO_NPE_FRAME_SNAPSHOT"), off_word: None, since: "2026-09-01" },
    // Default-ON: `stackwalker::osr_frame_dedupe_enabled` treats the key's
    // PRESENCE as "report the OSR continuation twice again".
    E { group: Group::JIT, token: "osr-frame-dedupe", on_key: None, off_key: Some("CRATONVM_JIT_NO_OSR_FRAME_DEDUPE"), off_word: None, since: "2026-08-20" },
    // Default-ON: an OSR entry pc must have an EMPTY abstract expression stack
    // (`x64::osr::osr_empty_stack_entry_enabled`, presence-parsed, so `=0`
    // still turns the rule OFF and `off_word` must stay `None`). A soundness
    // rule HotSpot also enforces — declared because it is default-on CODEGEN
    // that landed without a failure of its own, and the page that opened the
    // question named the missing switch as its last open item.
    E { group: Group::JIT, token: "osr-empty-stack-entry", on_key: None, off_key: Some("CRATONVM_JIT_NO_OSR_EMPTY_STACK_ENTRY"), off_word: None, since: "2026-09-05" },
    // Default-ON: `jit_bridge::osr_pc_refresh_enabled` treats the key's
    // PRESENCE as "stop publishing OSR continuations; report the back-edge".
    // Separate from the dedupe row above because the two answer different
    // questions -- which activation a frame belongs to, and where that
    // activation currently is -- and an OSR trace defect can be attributed to
    // one or the other only if they can be switched independently.
    E { group: Group::JIT, token: "osr-pc-refresh", on_key: None, off_key: Some("CRATONVM_JIT_NO_OSR_PC_REFRESH"), off_word: None, since: "2026-09-01" },
    // Default-ON: `conservative_roots::compiled_frame_bci_enabled` treats the
    // key's PRESENCE as "restore `byte_code_index: -1` / `(Unknown Source)`
    // for every JIT frame". A suspect line in a warmed-up stack trace can then
    // be attributed to this bci recovery or to the LineNumberTable inside ONE
    // binary, which a cross-binary comparison cannot do.
    E { group: Group::JIT, token: "compiled-frame-lines", on_key: None, off_key: Some("CRATONVM_JIT_NO_COMPILED_FRAME_LINES"), off_word: None, since: "2026-09-01" },
    // Default-ON, and the key's PRESENCE reverts BOTH halves: the emitter
    // (jit/src/x64/inlining.rs) stops recording the PC->inline-chain map, and
    // the walk (vm/src/jit/conservative_roots.rs) stops expanding it. One name
    // for two halves deliberately -- a half-switched feature is worse than
    // either state: metadata nobody reads, or a walk reading a map nobody
    // emitted. Sits beside `compiled-frame-lines`, the structurally identical
    // sibling added on the same branch for defect 1 of the same page.
    E { group: Group::JIT, token: "inline-frame-map", on_key: None, off_key: Some("CRATONVM_JIT_NO_INLINE_FRAME_MAP"), off_word: None, since: "2026-09-01" },
    // Default-ON. The OPTIMIZING tier's safepoint-id slot holds a monotonic
    // counter, not a bci, so `activation_bci` refused the whole backend and
    // every C2 frame printed `(Unknown Source)` -- the largest population of
    // line-less compiled frames left after 2026-09-01. `ir_lower` now records
    // the id->bci translation (`CompiledMethod::safepoint_bci_table`) and the
    // walk reads through it. Separate from `compiled-frame-lines`, which
    // reverts the recovery on BOTH backends and so cannot attribute a suspect
    // line to the translation rather than to the slot read.
    E { group: Group::JIT, token: "ir-frame-lines", on_key: None, off_key: Some("CRATONVM_JIT_NO_IR_FRAME_LINES"), off_word: None, since: "2026-09-02" },
    // Default-ON. An inline null check is not a GC-capable call, so it
    // publishes no safepoint id and the frame it raises from was the ONE frame
    // in an NPE snapshot with no line (`big:-1`). The emitter records the
    // trapping bci and the splice chain per site and gives a described site a
    // ten-byte COLD trampoline that passes its id to `jit_npe_with_action`; the
    // `TEST`/`JZ` fast path is unchanged. This name reverts the recording and
    // the trampoline together, so the retained metadata, the cold bytes and the
    // line disappear as one. Depends on `inline-frame-map`: the enclosing bci
    // of a trap inside a splice is only knowable from that session's scope
    // stack.
    E { group: Group::JIT, token: "npe-trap-lines", on_key: None, off_key: Some("CRATONVM_JIT_NO_NPE_TRAP_LINES"), off_word: None, since: "2026-09-02" },
    // Default-ON. `stackwalker::frame_class_ids_with_compiled` -- the walk the
    // JEP 403 deep-reflection gate and `Class.forName`'s caller loader read --
    // reported ONE class per compiled artifact and so could not see a method
    // the JIT had inlined. It answers in `ClassId` and resolving a JIT label by
    // name would have been a guess in a security path; an inlined level now
    // carries the id the RESOLVER used, and only a chain keyed on the exact
    // return address is expanded (the coarse safepoint-id key is shared with
    // the inline cache's miss edge, where the spliced body did not run).
    // Separate from `inline-frame-map`, which kills the producer and takes the
    // DISPLAY frames with it; this is the half a caller-attribution change has
    // to be attributable to on its own.
    E { group: Group::JIT, token: "inline-caller-frames", on_key: None, off_key: Some("CRATONVM_JIT_NO_INLINE_CALLER_FRAMES"), off_word: None, since: "2026-09-02" },
    // D2: a guarded-virtual site emits guard, splice AND miss edge under ONE
    // safepoint bci, and the miss edge records no inline-frame row -- so that
    // bci held exactly one chain, was never poisoned, and the innermost frame
    // could be handed the chain of a splice THAT DID NOT RUN. A fabricated
    // frame is worse than a missing one, because a reader cannot tell. The fix
    // emits an empty-chain row at the miss edge so the EXISTING disagreement
    // rule poisons the bci. Needs its own name rather than riding
    // `inline-frame-map`: that switch kills the whole producer and so cannot
    // separate "the poison took this frame" from "the map never had it".
    E { group: Group::JIT, token: "inline-miss-edge-poison", on_key: None, off_key: Some("CRATONVM_JIT_NO_INLINE_MISS_EDGE_POISON"), off_word: None, since: "2026-09-01" },
    // B1: the IR tier grew a String-access expander (length/isEmpty/charAt),
    // but the invoke-planning gate still refused EVERY `java/lang/String`
    // invoke, so the expander was unreachable and its documented A/B measured
    // a different program. The carve-out asks `try_resolve_string_intrinsic`
    // whether THIS site is one the expander handles; every other String method
    // (hashCode, equals, compareTo, indexOf, and every CharSequence receiver)
    // still bails exactly as before. Presence restores the blanket refusal.
    E { group: Group::JIT, token: "ir-string-access-admit", on_key: None, off_key: Some("CRATONVM_JIT_NO_IR_STRING_ACCESS_ADMIT"), off_word: None, since: "2026-09-01" },
    // B5: a spliced direct call emitted its oop map AFTER the RBP republish, so
    // the map was filed under an offset 9-25 bytes past the return address and
    // `compiled_frame_bci`'s exact lookup missed EVERY time it was attempted,
    // silently falling back to the safepoint-id slot. Re-stamps the key; the
    // emitted bytes are byte-identical either way, so this is a pure metadata
    // A/B. Reordering the emission instead would be WRONG -- the map emitter
    // also emits shadow-reload code, and the republish can clobber RSI/RDI.
    E { group: Group::JIT, token: "inline-call-map-at-return", on_key: None, off_key: Some("CRATONVM_JIT_NO_INLINE_CALL_MAP_AT_RETURN"), off_word: None, since: "2026-09-01" },
    // B6: the aastore emitter handled compressed oops itself but had no ZGC
    // coverage, so an armed cycle would read a colored word with no colour test
    // and write a plain pointer the classifier then truncates to 42 bits.
    // Routes to the `jit_aastore` helper only when the barrier is armed, which
    // nothing does today -- the gate is expected to fire ZERO times, which is
    // exactly why its census carries a denominator.
    E { group: Group::JIT, token: "aastore-barrier-gate", on_key: None, off_key: Some("CRATONVM_JIT_NO_AASTORE_BARRIER_GATE"), off_word: None, since: "2026-09-01" },
    // Default-ON: `stackwalker::call_frame_dedupe_enabled` treats the key's
    // PRESENCE as "report the duplicate frame again". This is the ORDINARY
    // compiled-activation half of `drop_osr_continuations`; the
    // `osr-frame-dedupe` row above still switches off BOTH halves, so the two
    // rules are separable in one binary only because this row exists.
    E { group: Group::JIT, token: "call-frame-dedupe", on_key: None, off_key: Some("CRATONVM_JIT_NO_CALL_FRAME_DEDUPE"), off_word: None, since: "2026-09-01" },
    // Default-ON, `"0"` turns it off — same polarity as `ir-reloc-emit`, and
    // the row must spell it that way round. The consumer refuses to install a
    // compilation stamped older than the last cache flush; setting this to `0`
    // restores publish-anyway for bisection, which is an unsafe-on-purpose
    // lever. Added a wave after the declaration sweep closed at zero
    // offenders, which is exactly how the count creeps back up.
    E { group: Group::JIT, token: "strict-install-epoch", on_key: Some("CRATONVM_JIT_STRICT_INSTALL_EPOCH"), off_key: None, off_word: Some("0"), since: "2026-08-01" },
    E { group: Group::JIT, token: "deferred-new-retry-blind", on_key: Some("CRATONVM_JIT_DEFERRED_NEW_RETRY_BLIND"), off_key: None, off_word: None, since: "2026-09-03" },
    E { group: Group::JIT, token: "deferred-new-looks", on_key: Some("CRATONVM_JIT_DEFERRED_NEW_LOOKS"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-site-trap", on_key: Some("CRATONVM_JIT_IR_SITE_TRAP"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-scalar-intrinsics", on_key: Some("CRATONVM_JIT_IR_SCALAR_INTRINSICS"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-aastore", on_key: Some("CRATONVM_JIT_IR_AASTORE"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-check-elim", on_key: Some("CRATONVM_JIT_IR_CHECK_ELIM"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-bce-range", on_key: Some("CRATONVM_JIT_IR_BCE_RANGE"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-hot-layout", on_key: Some("CRATONVM_JIT_IR_HOT_LAYOUT"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-list-sched", on_key: Some("CRATONVM_JIT_IR_LIST_SCHED"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-unroll-unreachable-frames", on_key: Some("CRATONVM_JIT_IR_UNROLL_UNREACHABLE_FRAMES"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "c2-accept", on_key: Some("CRATONVM_C2_ACCEPT"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "c2-accept-memo", on_key: Some("CRATONVM_C2_ACCEPT_MEMO"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-unresolved-class-trap", on_key: Some("CRATONVM_JIT_IR_UNRESOLVED_CLASS_TRAP"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-ls-loop-weight", on_key: Some("CRATONVM_JIT_IR_LS_LOOP_WEIGHT"), off_key: None, off_word: None, since: "2026-09-03" },
    E { group: Group::JIT, token: "ir-param-copy", on_key: Some("CRATONVM_JIT_IR_PARAM_COPY"), off_key: None, off_word: None, since: "2026-09-04" },
    E { group: Group::JIT, token: "ir-rpo-layout", on_key: Some("CRATONVM_JIT_IR_RPO_LAYOUT"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    E { group: Group::JIT, token: "ir-call-anewarray", on_key: Some("CRATONVM_JIT_IR_CALL_ANEWARRAY"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    E { group: Group::JIT, token: "supersede-epoch-skip-useless", on_key: Some("CRATONVM_JIT_SUPERSEDE_EPOCH_SKIP_USELESS"), off_key: None, off_word: None, since: "2026-09-03" },
    E { group: Group::JIT, token: "kernel-reg-locals", on_key: Some("CRATONVM_JIT_KERNEL_REG_LOCALS"), off_key: None, off_word: None, since: "2026-07-14" },
    E { group: Group::JIT, token: "kernel-reg-osr", on_key: Some("CRATONVM_JIT_KERNEL_REG_OSR"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::JIT, token: "leak-code", on_key: Some("CRATONVM_JIT_LEAK_CODE"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::JIT, token: "licm", on_key: Some("CRATONVM_JIT_LICM"), off_key: None, off_word: None, since: "2026-06-18" },
    E { group: Group::JIT, token: "local-liveness", on_key: None, off_key: Some("CRATONVM_NO_LOCAL_LIVENESS"), off_word: None, since: "2026-07-03" },
    E { group: Group::JIT, token: "local-regs", on_key: Some("CRATONVM_JIT_LOCAL_REGS"), off_key: None, off_word: None, since: "2026-08-06" },
    E { group: Group::JIT, token: "long-intrinsics", on_key: None, off_key: Some("CRATONVM_JIT_NO_LONG_INTRINSICS"), off_word: None, since: "2026-06-03" },
    E { group: Group::JIT, token: "long-box-direct-helpers", on_key: Some("CRATONVM_JIT_LONG_BOX_DIRECT_HELPERS"), off_key: None, off_word: Some("0"), since: "2026-08-19" },
    E { group: Group::JIT, token: "varhandle-read-direct-helpers", on_key: Some("CRATONVM_JIT_VARHANDLE_READ_DIRECT_HELPERS"), off_key: None, off_word: Some("0"), since: "2026-08-20" },
    E { group: Group::JIT, token: "varhandle-cas-direct-helpers", on_key: Some("CRATONVM_JIT_VARHANDLE_CAS_DIRECT_HELPERS"), off_key: None, off_word: Some("0"), since: "2026-08-28" },
    E { group: Group::JIT, token: "varhandle-ref-read-direct", on_key: Some("CRATONVM_JIT_VARHANDLE_REF_READ_DIRECT"), off_key: None, off_word: Some("0"), since: "2026-09-01" },
    E { group: Group::JIT, token: "native-cf-complete", on_key: Some("CRATONVM_NATIVE_CF_COMPLETE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "varhandle-write-direct-helpers", on_key: Some("CRATONVM_JIT_VARHANDLE_WRITE_DIRECT_HELPERS"), off_key: None, off_word: Some("0"), since: "2026-08-24" },
    E { group: Group::JIT, token: "varhandle-cas-funnel-fast", on_key: Some("CRATONVM_JIT_VARHANDLE_CAS_FUNNEL_FAST"), off_key: None, off_word: Some("0"), since: "2026-08-26" },
    E { group: Group::JIT, token: "longroot-strict", on_key: Some("CRATONVM_LONGROOT_STRICT"), off_key: None, off_word: None, since: "2026-06-09" },
    E { group: Group::JIT, token: "main-inline", on_key: Some("CRATONVM_JIT_MAIN_INLINE"), off_key: None, off_word: None, since: "2026-06-13" },
    // Default-ON kill switch for the `int[][]` matrix-dot emitter.
    E { group: Group::JIT, token: "matrix-dot", on_key: Some("CRATONVM_JIT_MATRIX_DOT"), off_key: None, off_word: Some("0"), since: "2026-07-30" },
    // Per-compilation metrics. `metrics` is the collection gate; the other two
    // carry values, so `CRATONVM_JIT=metrics,metrics-out=/tmp/jit.jsonl`.
    E { group: Group::JIT, token: "metrics", on_key: Some("CRATONVM_JIT_METRICS"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::JIT, token: "metrics-out", on_key: Some("CRATONVM_JIT_METRICS_OUT"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::JIT, token: "metrics-ring", on_key: Some("CRATONVM_JIT_METRICS_RING"), off_key: None, off_word: None, since: "2026-07-31" },
    // The two virtual-MIC levers in `vm::jit::helpers`. Both are default-ON.
    // `mic-exc-table-publish` was default-OFF until 2026-08-17: the ban's stated
    // reason had already expired (`emit_inline_callee_deopt_check` closed the
    // hole), and lifting it is 8.7x on the exception-table rung of
    // `probes/NativeFunnelFloorProbe.java` with the control rung unmoved. It is
    // INTERLOCKED with `sp-ic-deopt-check`: publishing is refused outright when
    // that check is not being emitted, so `=0` on either one is safe.
    E { group: Group::JIT, token: "mic-exc-table-publish", on_key: Some("CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH"), off_key: None, off_word: Some("0"), since: "2026-07-31" },
    // The statically-bound door's sibling of `mic-exc-table-publish`: may a
    // baked direct CALL target a callee that declares its own exception table?
    // Default-OFF pending its own measurement (16 refused binds on netty's
    // BigEndianHeapByteBufTest against 736 for the native shadow), and
    // interlocked with `sp-ic-deopt-check` the same way.
    E { group: Group::JIT, token: "direct-exc-table-publish", on_key: Some("CRATONVM_JIT_DIRECT_EXC_TABLE_PUBLISH"), off_key: None, off_word: Some("0"), since: "2026-08-17" },
    // Serve `new`'s JVMS 5.5 initialization check from `class_init_memo`
    // instead of `ensure_class_initialized_shared`, as getstatic/putstatic
    // already do. Default-ON; the opt-out is the same-binary A/B, and its OFF
    // position is the older, correct, slower path.
    E { group: Group::JIT, token: "new-class-init-memo", on_key: None, off_key: Some("CRATONVM_JIT_NO_NEW_CLASS_INIT_MEMO"), off_word: None, since: "2026-08-26" },
    E { group: Group::JIT, token: "mic-rust-entry-cache", on_key: None, off_key: Some("CRATONVM_JIT_NO_MIC_RUST_ENTRY_CACHE"), off_word: None, since: "2026-07-31" },
    // The three moving-young ("my-") bisect levers. All default-ON, all read
    // `0`/`false` as off, and `my-shadow-emission` is read identically by
    // `jit::x64::licm` and `vm::jit::conservative_roots` — two halves of one
    // agreement, so a token that reached only one of them would be a bug.
    E { group: Group::JIT, token: "my-scratch-flush", on_key: Some("CRATONVM_JIT_MY_SCRATCH_FLUSH"), off_key: None, off_word: Some("0"), since: "2026-07-30" },
    E { group: Group::JIT, token: "my-selfcall-proof", on_key: Some("CRATONVM_JIT_MY_SELFCALL_PROOF"), off_key: None, off_word: Some("0"), since: "2026-07-30" },
    E { group: Group::JIT, token: "my-shadow-emission", on_key: Some("CRATONVM_JIT_MY_SHADOW_EMISSION"), off_key: None, off_word: Some("0"), since: "2026-07-31" },
    E { group: Group::JIT, token: "native-ec-multiply", on_key: Some("CRATONVM_NATIVE_EC_MULTIPLY"), off_key: None, off_word: None, since: "2026-06-04" },
    E { group: Group::JIT, token: "native-matcher-find", on_key: Some("CRATONVM_NATIVE_MATCHER_FIND"), off_key: None, off_word: Some("0"), since: "2026-07-11" },
    E { group: Group::JIT, token: "native-pbe-keyfactory", on_key: Some("CRATONVM_NATIVE_PBE_KEYFACTORY"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "native-string-regex", on_key: Some("CRATONVM_NATIVE_STRING_REGEX"), off_key: None, off_word: Some("0"), since: "2026-06-22" },
    E { group: Group::JIT, token: "never-free-code", on_key: Some("CRATONVM_JIT_NEVER_FREE_CODE"), off_key: None, off_word: None, since: "2026-07-28" },
    E { group: Group::JIT, token: "old-sweep-jit", on_key: Some("CRATONVM_OLD_SWEEP_JIT"), off_key: None, off_word: Some("0"), since: "2026-07-15" },
    E { group: Group::JIT, token: "osr", on_key: Some("CRATONVM_JIT_OSR"), off_key: None, off_word: None, since: "2026-06-23" },
    // Default-ON (RBC.6 lift): `env_cache::osr_athrow_allowed` answers `true`
    // for `Err(_)` and reads `0`/`false` as the kill switch.
    E { group: Group::JIT, token: "osr-athrow", on_key: Some("CRATONVM_JIT_OSR_ATHROW"), off_key: None, off_word: Some("0"), since: "2026-08-17" },
    // Default-ON: `jit::osr_dead_local_entry_allowed` answers `true` for
    // `Err(_)` and reads `0`/`off`/`false`/`no` as the kill switch.
    E { group: Group::JIT, token: "osr-dead-locals", on_key: Some("CRATONVM_JIT_OSR_DEAD_LOCALS"), off_key: None, off_word: Some("0"), since: "2026-07-31" },
    // Declared 2026-08-19. An OPT-OUT: publishing a per-bci-`Ambiguous` local
    // as `Undefined` rather than `Unsupported` is the default, and this key
    // restores the re-run encoding. See
    // `jit/src/x64/deopt_stubs.rs::osr_ambiguous_dead_enabled`.
    E { group: Group::JIT, token: "osr-ambiguous-dead", on_key: None, off_key: Some("CRATONVM_JIT_NO_OSR_AMBIGUOUS_DEAD"), off_word: None, since: "2026-08-19" },
    // Declared 2026-08-19 beside `osr-ambiguous-dead`. Also an OPT-OUT: a
    // per-bci-`Ref` local at a bci where the oop mask has no opinion is
    // published as a reference by default. See
    // `jit/src/x64/deopt_stubs.rs::osr_refined_ref_enabled`.
    E { group: Group::JIT, token: "osr-refined-ref", on_key: None, off_key: Some("CRATONVM_JIT_NO_OSR_REFINED_REF"), off_word: None, since: "2026-08-19" },
    E { group: Group::JIT, token: "osr-dead-mask-blanket", on_key: Some("CRATONVM_JIT_OSR_DEAD_MASK_BLANKET"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::JIT, token: "osr-newarray", on_key: Some("CRATONVM_OSR_NEWARRAY"), off_key: None, off_word: None, since: "2026-07-10" },
    E { group: Group::JIT, token: "osr-exc-table", on_key: Some("CRATONVM_JIT_OSR_EXC_TABLE"), off_key: None, off_word: None, since: "2026-08-17" },
    // Declared 2026-08-24. An OPT-OUT: the optimizing tier republishes the
    // innermost-frame mirror after an inline-cache hit by default, and this key
    // restores the stale-mirror behaviour so one binary has both arms. See
    // `ir_lower::emit_call_cached_entry`.
    // Declared 2026-08-26. An OPT-OUT: the staged invoke-argument buffer is
    // published on the SHADOW stack by default, not merely named in the oop
    // map. The band verifier consults only the shadow stack. See
    // `x64::safepoint::collect_live_oop_homes`.
    E { group: Group::JIT, token: "staged-arg-shadow", on_key: None, off_key: Some("CRATONVM_JIT_NO_STAGED_ARG_SHADOW"), off_word: None, since: "2026-08-26" },
    E { group: Group::JIT, token: "ic-frame-republish", on_key: None, off_key: Some("CRATONVM_JIT_NO_IC_FRAME_REPUBLISH"), off_word: None, since: "2026-08-24" },
    E { group: Group::JIT, token: "zero-reserved-tail", on_key: None, off_key: Some("CRATONVM_JIT_NO_ZERO_RESERVED_TAIL"), off_word: None, since: "2026-09-04" },
    E { group: Group::JIT, token: "zero-unset-locals", on_key: None, off_key: Some("CRATONVM_JIT_NO_ZERO_UNSET_LOCALS"), off_word: None, since: "2026-09-04" },
    E { group: Group::JIT, token: "checkcast-inline", on_key: Some("CRATONVM_JIT_CHECKCAST_INLINE"), off_key: None, off_word: None, since: "2026-08-28" },
    E { group: Group::JIT, token: "final-devirt", on_key: Some("CRATONVM_JIT_FINAL_DEVIRT"), off_key: None, off_word: None, since: "2026-08-28" },
    E { group: Group::JIT, token: "final-devirt-native-screen", on_key: Some("CRATONVM_JIT_FINAL_DEVIRT_NATIVE_SCREEN"), off_key: None, off_word: None, since: "2026-09-05" },
    // Declared 2026-09-02. `final-devirt` above is `java/lang/String`'s
    // problem: String is final, so the rewrite it drives took EVERY String
    // access site away from the inline intrinsic. This is the opt-out for the
    // yield that gives them back -- default OFF, so the yield is on.
    E { group: Group::JIT, token: "devirt-intrinsic-yield", on_key: None, off_key: Some("CRATONVM_JIT_NO_DEVIRT_INTRINSIC_YIELD"), off_word: None, since: "2026-09-02" },
    E { group: Group::JIT, token: "inline-calls", on_key: Some("CRATONVM_JIT_INLINE_CALLS"), off_key: None, off_word: None, since: "2026-08-18" },
    E { group: Group::JIT, token: "inline-nest", on_key: Some("CRATONVM_JIT_INLINE_NEST"), off_key: None, off_word: None, since: "2026-08-18" },
    E { group: Group::JIT, token: "inline-call-dispatch", on_key: Some("CRATONVM_JIT_INLINE_CALL_DISPATCH"), off_key: None, off_word: None, since: "2026-08-18" },
    E { group: Group::JIT, token: "inline-splice-devirt", on_key: Some("CRATONVM_JIT_INLINE_SPLICE_DEVIRT"), off_key: None, off_word: None, since: "2026-08-18" },
    E { group: Group::JIT, token: "local-handlers", on_key: Some("CRATONVM_JIT_LOCAL_HANDLERS"), off_key: None, off_word: None, since: "2026-08-20" },
    // Default-**OFF**, unlike their neighbour `osr-dead-locals` four rows up —
    // the contrast is the reason these two carry a comment at all.
    // `jit::osr_always_seed_frame_slot` and `jit::osr_single_pc_entry_only`
    // both answer `false` for `Err(_)` and admit only `1`/`on`/`true`/`yes`, so
    // unsetting the key IS the off state and `off_word` stays `None`. Writing
    // `Some("0")` would mislabel them as default-ON kill switches — the one
    // thing this table exists to state unambiguously — and would make
    // `-osr-single-pc` expand to `=0`, which those consumers happen to read as
    // off only because `0` is absent from their truthy list, not because they
    // were written to accept an opt-out.
    E { group: Group::JIT, token: "osr-seed-frame-slots", on_key: Some("CRATONVM_JIT_OSR_SEED_FRAME_SLOTS"), off_key: None, off_word: None, since: "2026-08-04" },
    E { group: Group::JIT, token: "osr-strip-all-high-halves", on_key: Some("CRATONVM_JIT_OSR_STRIP_ALL_HIGH_HALVES"), off_key: None, off_word: None, since: "2026-08-06" },
    E { group: Group::JIT, token: "osr-single-pc", on_key: Some("CRATONVM_JIT_OSR_SINGLE_PC"), off_key: None, off_word: None, since: "2026-08-03" },
    E { group: Group::JIT, token: "poison-free", on_key: Some("CRATONVM_JIT_POISON_FREE"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::JIT, token: "post-tlab-hash-stamp", on_key: Some("CRATONVM_JIT_POST_TLAB_HASH_STAMP"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "precise-coverage-pin", on_key: Some("CRATONVM_PRECISE_COVERAGE_PIN"), off_key: None, off_word: None, since: "2026-06-21" },
    // Wrong-answer A/B lever, not a tuning knob: OFF restores the params-only
    // `run_jit_callee_handler` resume that zeroed a compiled callee's
    // non-parameter locals. See `params_only_callee_handler_frames`.
    E { group: Group::JIT, token: "callee-handler-precise-frame", on_key: None, off_key: Some("CRATONVM_NO_JIT_CALLEE_HANDLER_PRECISE_FRAME"), off_word: None, since: "2026-08-01" },
    E { group: Group::JIT, token: "precise-handler-frames", on_key: None, off_key: Some("CRATONVM_NO_JIT_PRECISE_HANDLER_FRAMES"), off_word: None, since: "2026-07-28" },
    E { group: Group::JIT, token: "precise-inline-frame-record", on_key: None, off_key: Some("CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD"), off_word: None, since: "2026-06-21" },
    E { group: Group::JIT, token: "precise-jit-maps", on_key: None, off_key: Some("CRATONVM_NO_PRECISE_JIT_MAPS"), off_word: None, since: "2026-06-17" },
    E { group: Group::JIT, token: "precise-reg-spill", on_key: None, off_key: Some("CRATONVM_NO_PRECISE_REG_SPILL"), off_word: None, since: "2026-07-11" },
    E { group: Group::JIT, token: "precise-virtual-invokes", on_key: None, off_key: Some("CRATONVM_JIT_NO_PRECISE_VIRTUAL_INVOKES"), off_word: None, since: "2026-07-31" },
    // Guard-dominated bounds-check elimination (`x64::bce::range_bce_enabled`).
    // Default-**OFF** and presence-parsed: the gate is `.is_some()`, so
    // `CRATONVM_JIT_RANGE_BCE=0` *enables* it. `off_word` must therefore stay
    // `None` — `Some("0")` would make `CRATONVM_JIT=-range-bce` write `"0"` and
    // switch the pass **on**, and a wrong elision here is an out-of-bounds heap
    // write. `-bce` (`CRATONVM_JIT_NO_BCE`) still kills every reason including
    // this one.
    E { group: Group::JIT, token: "range-bce", on_key: Some("CRATONVM_JIT_RANGE_BCE"), off_key: None, off_word: None, since: "2026-08-01" },
    E { group: Group::JIT, token: "range-scan-legacy", on_key: Some("CRATONVM_JIT_RANGE_SCAN_LEGACY"), off_key: None, off_word: None, since: "2026-06-23" },
    E { group: Group::JIT, token: "reassoc", on_key: Some("CRATONVM_JIT_REASSOC"), off_key: None, off_word: None, since: "2026-06-16" },
    // Default-ON kill switch, hence `off_key` only: `CRATONVM_JIT=-retpc-validate`
    // makes the A5 unregistered-JIT-frame stack scan treat EVERY in-range stack
    // word as a return address again, the way it did before 2026-08-04. Kept as
    // a one-flag bisect for the false-positive filter in
    // `conservative_roots::is_plausible_return_pc`.
    E { group: Group::JIT, token: "retpc-validate", on_key: None, off_key: Some("CRATONVM_JIT_NO_RETPC_VALIDATE"), off_word: None, since: "2026-08-04" },
    // Default-ON kill switch, hence `off_key` only:
    // `CRATONVM_JIT=-native-site-cache` takes the JIT's per-call-site native
    // fast path out of a run. It exists because that path spent 2026-08-05 as
    // the prime suspect for the Spring Boot corruption family — it reads a memo
    // keyed on a `JitInvokeInfo` ADDRESS, and while those were recyclable
    // (`383e7f5cf`) it was the loudest way that hazard surfaced — with no way to
    // remove it from a run short of a rebuild. See
    // `jit::helpers::native_site_cache_enabled` for the measured rates.
    E { group: Group::JIT, token: "native-site-cache", on_key: None, off_key: Some("CRATONVM_JIT_NO_NATIVE_SITE_CACHE"), off_word: None, since: "2026-08-05" },
    E { group: Group::JIT, token: "rootsnap-cache", on_key: Some("CRATONVM_ROOTSNAP_CACHE"), off_key: None, off_word: None, since: "2026-06-14" },
    E { group: Group::JIT, token: "rootsnap-cache-survive-gc", on_key: Some("CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC"), off_key: None, off_word: None, since: "2026-06-15" },
    E { group: Group::JIT, token: "safepoint-polls", on_key: Some("CRATONVM_JIT_SAFEPOINT_POLLS"), off_key: None, off_word: None, since: "2026-07-23" },
    E { group: Group::JIT, token: "safepoint-reg-spill", on_key: Some("CRATONVM_JIT_SAFEPOINT_REG_SPILL"), off_key: None, off_word: None, since: "2026-06-16" },
    E { group: Group::JIT, token: "scalar-deopt", on_key: Some("CRATONVM_SCALAR_DEOPT"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "scalar-new", on_key: Some("CRATONVM_JIT_SCALAR_NEW"), off_key: None, off_word: None, since: "2026-06-20" },
    E { group: Group::JIT, token: "scalar-replacement", on_key: None, off_key: Some("CRATONVM_DISABLE_SCALAR_REPLACEMENT"), off_word: None, since: "2026-05-20" },
    E { group: Group::JIT, token: "scan-cache", on_key: None, off_key: Some("CRATONVM_NO_JIT_SCAN_CACHE"), off_word: None, since: "2026-06-12" },
    E { group: Group::JIT, token: "self-cache-inherit", on_key: None, off_key: Some("CRATONVM_JIT_NO_SELF_CACHE_INHERIT"), off_word: None, since: "2026-07-14" },
    E { group: Group::JIT, token: "atomic-intrinsic", on_key: None, off_key: Some("CRATONVM_JIT_NO_ATOMIC_INTRINSIC"), off_word: None, since: "2026-08-13" },
    E { group: Group::JIT, token: "ffm-intrinsic", on_key: None, off_key: Some("CRATONVM_JIT_NO_FFM_INTRINSIC"), off_word: None, since: "2026-08-26" },
    E { group: Group::JIT, token: "c2-alloc-upgrade", on_key: Some("CRATONVM_JIT_C2_ALLOC_UPGRADE"), off_key: None, off_word: None, since: "2026-08-27" },
    E { group: Group::JIT, token: "ir-inline", on_key: Some("CRATONVM_JIT_IR_INLINE"), off_key: None, off_word: None, since: "2026-08-27" },
    E { group: Group::JIT, token: "field-site-cache", on_key: Some("CRATONVM_JIT_FIELD_SITE_CACHE"), off_key: None, off_word: Some("0"), since: "2026-08-04" },
    E { group: Group::JIT, token: "cast-site-cache", on_key: None, off_key: Some("CRATONVM_JIT_NO_CAST_SITE_CACHE"), off_word: None, since: "2026-08-18" },
    // Default-ON kill switch, hence `off_key` only:
    // `CRATONVM_JIT=-code-ptr-memo` puts `jit::validate_code_ptr` back on the
    // global `Mutex` it used to take on EVERY compiled call. It exists so the
    // memo can be A/B-ed inside one binary -- a 1.5%-of-CPU symbol cannot be
    // measured across two builds on a host whose run-to-run spread is 20%.
    // See `cratonvm_jit::code_ptr_memo_enabled`.
    E { group: Group::JIT, token: "code-ptr-memo", on_key: None, off_key: Some("CRATONVM_JIT_NO_CODE_PTR_MEMO"), off_word: None, since: "2026-08-21" },
    E { group: Group::DBG, token: "invoke-phases", on_key: Some("CRATONVM_DBG_INVOKE_PHASES"), off_key: None, off_word: None, since: "2026-08-19" },
    // `field-phases` — the same cycle breakdown for a quickened `getfield`.
    // Written after four structural explanations for the field ratio were
    // proposed from reading the code and refuted by measurement.
    E { group: Group::DBG, token: "field-phases", on_key: Some("CRATONVM_DBG_FIELD_PHASES"), off_key: None, off_word: None, since: "2026-09-05" },
    E { group: Group::JIT, token: "param-tag-scan", on_key: None, off_key: Some("CRATONVM_JIT_NO_PARAM_TAG_SCAN"), off_word: None, since: "2026-08-18" },
    // ── Interpreter hot-path memoizations, 2026-09-02 ────────────────────
    // Each of the five below removes work that was being repeated per
    // operation but is fixed per method, per call site or per process. Each
    // ships with an off switch for the same reason `code-ptr-memo` above
    // does: a change worth single-digit nanoseconds, on a host whose
    // run-to-run spread is 20%, can only be measured by A/B-ing ONE binary.
    // The `iface-select-memo` switch had to be widened once already -- it
    // gated the memo but not the short-circuit beside it, so its "off" arm
    // was not the pre-change path and the first A/B separated nothing.
    //
    // `descriptor-facts` — off routes `ParamTags::for_method` and
    // `Frame::return_tag` back through the per-call descriptor scans they
    // replaced (`ParamTags::of`, `cratonvm_jit::return_type`).
    E { group: Group::JIT, token: "descriptor-facts", on_key: None, off_key: Some("CRATONVM_JIT_NO_DESCRIPTOR_FACTS"), off_word: None, since: "2026-09-02" },
    // `backedge-poll-gate` — off restores the unconditional
    // `safepoint_check` call on every backward branch.
    E { group: Group::JIT, token: "backedge-poll-gate", on_key: None, off_key: Some("CRATONVM_JIT_NO_BACKEDGE_POLL_GATE"), off_word: None, since: "2026-09-02" },
    // `field-fast-path` — off restores the full `op_getfield` / `op_putfield`
    // handler on every instance field access.
    E { group: Group::JIT, token: "field-fast-path", on_key: None, off_key: Some("CRATONVM_JIT_NO_FIELD_FAST_PATH"), off_word: None, since: "2026-09-02" },
    // `field-addr-elide` — off restores the object-start registry probe the
    // quickened field and array arms ran on every receiver. The handler those
    // arms replace (`ZgcRealHeap::get_field`) never made that test, and the
    // header comparison beside it is what actually validates the site.
    E { group: Group::JIT, token: "field-addr-elide", on_key: None, off_key: Some("CRATONVM_JIT_NO_FIELD_ADDR_ELIDE"), off_word: None, since: "2026-09-05" },
    // `arraylength-fast` — off routes `arraylength` back through the `Value`
    // round trip and the `VmHeap` enum dispatch, for one header read.
    E { group: Group::JIT, token: "arraylength-fast", on_key: None, off_key: Some("CRATONVM_JIT_NO_ARRAYLENGTH_FAST"), off_word: None, since: "2026-09-05" },
    // `ref-array-fast` — off routes `aaload` back through
    // `VmHeap::get_array_element`; the `0x2e..=0x35` arm's primitive half
    // declines reference elements by construction.
    E { group: Group::JIT, token: "ref-array-fast", on_key: None, off_key: Some("CRATONVM_JIT_NO_REF_ARRAY_FAST"), off_word: None, since: "2026-09-05" },
    // `system-class-latch` — off restores the class-manager read lock and name
    // comparison `op_getstatic` performed on every getstatic.
    E { group: Group::JIT, token: "system-class-latch", on_key: None, off_key: Some("CRATONVM_JIT_NO_SYSTEM_CLASS_LATCH"), off_word: None, since: "2026-09-05" },
    // `osr-inline-gate` — off calls `try_osr_with_backoff` on every backward
    // branch instead of only past the smallest OSR threshold.
    E { group: Group::JIT, token: "osr-inline-gate", on_key: None, off_key: Some("CRATONVM_JIT_NO_OSR_INLINE_GATE"), off_word: None, since: "2026-09-02" },
    // `invoke-fast-door` — off routes every monomorphic virtual cache hit
    // through the general `execute_invokevirtual_cached` dispatcher.
    E { group: Group::JIT, token: "invoke-fast-door", on_key: Some("CRATONVM_JIT_INVOKE_FAST_DOOR"), off_key: Some("CRATONVM_JIT_NO_INVOKE_FAST_DOOR"), off_word: None, since: "2026-09-02" },
    E { group: Group::JIT, token: "door-receiver-record", on_key: None, off_key: Some("CRATONVM_JIT_NO_DOOR_RECEIVER_RECORD"), off_word: None, since: "2026-09-03" },
    E { group: Group::JIT, token: "door-recv-memo", on_key: None, off_key: Some("CRATONVM_JIT_NO_DOOR_RECV_MEMO"), off_word: None, since: "2026-09-04" },
    // `nonvirtual-fast-door` — off routes every monomorphic `invokestatic`
    // and `invokespecial` cache hit through the general dispatcher.
    E { group: Group::JIT, token: "nonvirtual-fast-door", on_key: None, off_key: Some("CRATONVM_JIT_NO_NONVIRTUAL_FAST_DOOR"), off_word: None, since: "2026-09-02" },
    // `frame-slot-reuse` — off routes every frame's buffers back through
    // the thread pools on return instead of retiring the frame in place.
    E { group: Group::JIT, token: "frame-slot-reuse", on_key: None, off_key: Some("CRATONVM_JIT_NO_FRAME_SLOT_REUSE"), off_word: None, since: "2026-09-02" },
    // `frame-emplace` — off builds the frame on the Rust stack and moves it
    // into the slot instead of constructing it there.
    E { group: Group::JIT, token: "frame-emplace", on_key: None, off_key: Some("CRATONVM_JIT_NO_FRAME_EMPLACE"), off_word: None, since: "2026-09-03" },
    // `iface-select-memo` — off makes every `invokeinterface` cache hit
    // retake the class-manager read lock and rewalk the receiver hierarchy
    // to re-verify maximally-specific selection.
    E { group: Group::JIT, token: "iface-select-memo", on_key: None, off_key: Some("CRATONVM_JIT_NO_IFACE_SELECT_MEMO"), off_word: None, since: "2026-09-02" },
    // `dup-name-field-gate` — off makes every instance field access walk
    // `retarget_instance_field_to_receiver` in full, whether or not any
    // binary name in this process resolves to two `ClassId`s.
    E { group: Group::LOADER, token: "dup-name-field-gate", on_key: None, off_key: Some("CRATONVM_LOADER_NO_DUP_NAME_FIELD_GATE"), off_word: None, since: "2026-09-02" },
    // `ann-proxy-latch` — off restores the epoch-keyed negative in
    // `ClassRealm::is_annotation_proxy_class`.
    E { group: Group::LOADER, token: "ann-proxy-latch", on_key: None, off_key: Some("CRATONVM_LOADER_NO_ANN_PROXY_LATCH"), off_word: None, since: "2026-09-02" },
    E { group: Group::JIT, token: "ldc-const-cache", on_key: None, off_key: Some("CRATONVM_JIT_NO_LDC_CONST_CACHE"), off_word: None, since: "2026-08-18" },
    E { group: Group::JIT, token: "ir-unresumable-trap-guard", on_key: Some("CRATONVM_JIT_IR_UNRESUMABLE_TRAP_GUARD"), off_key: None, off_word: Some("0"), since: "2026-08-21" },
    E { group: Group::JIT, token: "compiled-ldc-const-cache", on_key: Some("CRATONVM_JIT_COMPILED_LDC_CONST_CACHE"), off_key: None, off_word: Some("0"), since: "2026-08-20" },
    E { group: Group::JIT, token: "new-site-cache", on_key: None, off_key: Some("CRATONVM_JIT_NO_NEW_SITE_CACHE"), off_word: None, since: "2026-08-17" },
    E { group: Group::JIT, token: "site-cache", on_key: Some("CRATONVM_JIT_SITE_CACHE"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::JIT, token: "unreg-memo-hiwater", on_key: Some("CRATONVM_JIT_UNREG_MEMO_HIWATER"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::JIT, token: "field-site-cache-loader", on_key: Some("CRATONVM_JIT_FIELD_SITE_CACHE_LOADER"), off_key: None, off_word: None, since: "2026-08-04" },
    E { group: Group::JIT, token: "field-site-slots", on_key: Some("CRATONVM_JIT_FIELD_SITE_SLOTS"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::JIT, token: "method-site-cache", on_key: Some("CRATONVM_JIT_METHOD_SITE_CACHE"), off_key: None, off_word: None, since: "2026-08-04" },
    E { group: Group::JIT, token: "loop-work-tierup", on_key: Some("CRATONVM_JIT_LOOP_WORK_TIERUP"), off_key: None, off_word: None, since: "2026-08-04" },
    E { group: Group::JIT, token: "shadow-nopush", on_key: Some("CRATONVM_SHADOW_NOPUSH"), off_key: None, off_word: None, since: "2026-06-04" },
    E { group: Group::JIT, token: "sync-methods", on_key: Some("CRATONVM_JIT_SYNC_METHODS"), off_key: None, off_word: None, since: "2026-08-04" },
    E { group: Group::JIT, token: "shadow-noreload", on_key: Some("CRATONVM_SHADOW_NORELOAD"), off_key: None, off_word: None, since: "2026-06-04" },
    E { group: Group::JIT, token: "shadow-pin", on_key: Some("CRATONVM_SHADOW_PIN"), off_key: None, off_word: None, since: "2026-06-16" },
    E { group: Group::JIT, token: "shadow-raw-reload", on_key: Some("CRATONVM_SHADOW_RAW_RELOAD"), off_key: None, off_word: None, since: "2026-06-16" },
    // Default-ON overflow bound on the shadow-stack push both backends emit.
    // `-shadow-end-guard` restores the pre-guard behaviour in which an
    // overrunning push stores straight on through the allocator arena — an
    // out-of-bounds write, so this is a bisection-only switch. It was read by a
    // live `getenv` until this row existed, which is precisely how a stale
    // export in a parent shell disarms a memory-safety bound with no way for
    // the launcher, `-XX:`, or a test override to see it or say otherwise.
    E { group: Group::JIT, token: "shadow-end-guard", on_key: None, off_key: Some("CRATONVM_SHADOW_NO_END_GUARD"), off_word: None, since: "2026-07-31" },
    E { group: Group::JIT, token: "shadow-overflow-diag", on_key: Some("CRATONVM_SHADOW_OVERFLOW_DIAG"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::JIT, token: "shadow-savebase", on_key: None, off_key: Some("CRATONVM_SHADOW_NO_SAVEBASE"), off_word: None, since: "2026-06-16" },
    E { group: Group::JIT, token: "shadow-stack", on_key: Some("CRATONVM_SHADOW_STACK"), off_key: None, off_word: None, since: "2026-06-04" },
    E { group: Group::JIT, token: "slot-mirror", on_key: None, off_key: Some("CRATONVM_JIT_NO_SLOT_MIRROR"), off_word: None, since: "2026-07-14" },
    E { group: Group::JIT, token: "sp-coalesce", on_key: None, off_key: Some("CRATONVM_SP_NO_COALESCE"), off_word: None, since: "2026-06-04" },
    // Both default-ON and both parsed by an exact `Ok("0")` match — no other
    // word turns them off, so `off_word` must be exactly `"0"`. `sp-tailcall`
    // is the SIBLING tail-call (a JMP into another method's entry); the
    // self-recursive form is `self-tailcall` above, which is opt-in.
    E { group: Group::JIT, token: "sp-inline-ic", on_key: Some("CRATONVM_JIT_SP_INLINE_IC"), off_key: None, off_word: Some("0"), since: "2026-07-31" },
    // Opt-in since 2026-08-20: the `JMP`-back form reuses one native frame
    // per activation, which no other execution mode here does.
    E { group: Group::JIT, token: "self-tailcall", on_key: Some("CRATONVM_JIT_SELF_TAILCALL"), off_key: None, off_word: None, since: "2026-08-20" },
    E { group: Group::JIT, token: "sp-tailcall", on_key: Some("CRATONVM_JIT_SP_TAILCALL"), off_key: None, off_word: Some("0"), since: "2026-07-31" },
    E { group: Group::JIT, token: "spec-bce", on_key: None, off_key: Some("CRATONVM_JIT_NO_SPEC_BCE"), off_word: None, since: "2026-06-03" },
    E { group: Group::JIT, token: "stack-bang", on_key: Some("CRATONVM_JIT_STACK_BANG"), off_key: Some("CRATONVM_JIT_NO_STACK_BANG"), off_word: None, since: "2026-07-01" },
    // Interpreter-side like `trivial-getter`: the lock-free static-field read
    // path in `vm::vm_object`, default-ON with an opt-out-only spelling.
    E { group: Group::JIT, token: "static-bytecode-callee", on_key: Some("CRATONVM_JIT_STATIC_BYTECODE_CALLEE"), off_key: None, off_word: Some("0"), since: "2026-08-22" },
    E { group: Group::JIT, token: "indy-bridge", on_key: Some("CRATONVM_JIT_INDY_BRIDGE"), off_key: None, off_word: Some("0"), since: "2026-08-23" },
    // Its invokevirtual/invokeinterface twin. Same shape, same default-ON
    // opt-out-only spelling; see `env_cache::jit_virtual_bytecode_callee`.
    E { group: Group::JIT, token: "virtual-bytecode-callee", on_key: Some("CRATONVM_JIT_VIRTUAL_BYTECODE_CALLEE"), off_key: None, off_word: Some("0"), since: "2026-08-23" },
    E { group: Group::JIT, token: "statics-index", on_key: None, off_key: Some("CRATONVM_NO_STATICS_INDEX"), off_word: None, since: "2026-08-01" },
    E { group: Group::JIT, token: "vector-intrinsics", on_key: Some("CRATONVM_VECTOR_INTRINSICS"), off_key: None, off_word: Some("0"), since: "2026-08-22" },
    // The dispatch-layer half of the same feature, switched separately so the
    // templates can be priced against the kernels they sit on rather than only
    // against an un-intercepted VM.
    E { group: Group::JIT, token: "vector-templates", on_key: Some("CRATONVM_VECTOR_TEMPLATES"), off_key: None, off_word: Some("0"), since: "2026-08-23" },
    // `FileChannelImpl.read/write(ByteBuffer)` as one native call instead of
    // twenty JDK frames (`native-io::file_channel_fast_read`). Default-ON,
    // opt-out-only, same shape as `vector-intrinsics` above: the switch gates
    // REGISTRATION so the off arm is the un-intercepted VM.
    E { group: Group::JIT, token: "fc-fast-io", on_key: Some("CRATONVM_FC_FAST_IO"), off_key: None, off_word: Some("0"), since: "2026-08-23" },
    // The socket path's per-call cost removals, each separately switchable so
    // its A/B is one binary and one variable. All default-ON and opt-out-only,
    // the same shape as `fc-fast-io` above.
    //
    // `sc-scratch`      — reuse the thread's transfer buffer instead of
    //                     allocating and zeroing one sized to the destination's
    //                     remaining capacity on every call.
    // `sc-bb-slots`     — memoize `ByteBuffer` field slots per (VM, class)
    //                     instead of resolving them by name on every transfer.
    // `sel-ready-cache` — mirror a selector's readiness into the process-global
    //                     side table only when it CHANGED, not every key every
    //                     tick.
    // `sel-fast-keys`   — append ready keys straight into netty's own key set
    //                     instead of re-entering the interpreter once per key.
    //
    // The last two are separate tokens rather than one: they are independent
    // cuts to the same function, and `sel-ready-cache`'s failure mode (a latched
    // cache suppressing a real readiness change) is silent, so an A/B that could
    // only turn both off together could not attribute it.
    E { group: Group::JIT, token: "sc-scratch", on_key: Some("CRATONVM_SC_SCRATCH"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    E { group: Group::JIT, token: "sc-bb-slots", on_key: Some("CRATONVM_SC_BB_SLOTS"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    E { group: Group::JIT, token: "sel-ready-cache", on_key: Some("CRATONVM_SEL_READY_CACHE"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    E { group: Group::JIT, token: "sel-fast-keys", on_key: Some("CRATONVM_SEL_FAST_KEYS"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    // `sc-preresolved` — dial the address an `InetSocketAddress` ALREADY holds
    // instead of re-resolving its hostname on every connect. Default-ON and
    // opt-out-only; `=0` restores the per-dial `getaddrinfo`, which is the
    // "off" arm for
    // `internal/fixed-suite-bugs/netty/blocking-connect-re-resolves-the-destination-hostname-FIXED-20260905.md`.
    E { group: Group::JIT, token: "sc-preresolved", on_key: Some("CRATONVM_SC_PRERESOLVED"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    // Default-**ON** (`unwrap_or(true)` in `jit::strict_callee_roots_enabled`),
    // despite the prose on that function calling it an opt-in.
    E { group: Group::JIT, token: "strict-callee-roots", on_key: Some("CRATONVM_JIT_STRICT_CALLEE_ROOTS"), off_key: None, off_word: Some("0"), since: "2026-07-27" },
    E { group: Group::JIT, token: "strict-jit-roots", on_key: Some("CRATONVM_STRICT_JIT_ROOTS"), off_key: None, off_word: None, since: "2026-06-17" },
    E { group: Group::JIT, token: "threshold", on_key: Some("CRATONVM_JIT_THRESHOLD"), off_key: None, off_word: None, since: "2026-06-12" },
    // The CharSequence->String intrinsic. DEFAULT-OFF as of 2026-08-24, so a
    // plain opt-in row rather than a kill switch.
    E { group: Group::JIT, token: "charseq-string-intrinsic", on_key: Some("CRATONVM_JIT_CHARSEQ_STRING_INTRINSIC"), off_key: None, off_word: None, since: "2026-08-25" },
    // Receiver de-speculation. DEFAULT-ON, so a KILL SWITCH: `=0` restores the
    // old behaviour. Same `off_word` shape as `osr-coverage-shadow` and
    // `xt-jit-coverage-handshake`, which are the other two default-ON rows.
    E { group: Group::JIT, token: "receiver-despec", on_key: Some("CRATONVM_JIT_RECEIVER_DESPEC"), off_key: None, off_word: Some("0"), since: "2026-08-25" },
    // Numeric: the de-speculation spare factor, default 2. A VALUE knob, like
    // `threshold` above -- the token carries a number, not an on/off.
    E { group: Group::JIT, token: "despec-spare-factor", on_key: Some("CRATONVM_JIT_DESPEC_SPARE_FACTOR"), off_key: None, off_word: None, since: "2026-08-25" },
    // Elide the SB-CRASH-04 full-GPR blind spill at a direct call whose caller
    // frame is provably oop-clean. Multi-valued, not a boolean: `0` never
    // elides (the pre-2026-08-26 behaviour), `args`/`2` and the default `3`
    // widen it -- so `off_word` is `0` and the token carries the mode.
    E { group: Group::JIT, token: "call-spill-elision", on_key: Some("CRATONVM_JIT_CALL_SPILL_ELISION"), off_key: None, off_word: Some("0"), since: "2026-08-26" },
    // Narrow the safepoint blind spill to the registers that can hold an oop.
    // DEFAULT-ON, so a KILL SWITCH: `=0` restores the full-GPR spill. Same
    // off_word shape as `call-spill-elision` beside it.
    E { group: Group::JIT, token: "spill-narrow", on_key: Some("CRATONVM_JIT_SPILL_NARROW"), off_key: None, off_word: Some("0"), since: "2026-08-26" },
    // A call site's argument staging IS the publication of its argument oops,
    // so the pre-safepoint spill of those registers is a duplicate. DEFAULT-ON,
    // hence a KILL SWITCH: `=0` restores the full selection at the staged sites.
    // Opt-in PER SITE in the compiler, never globally -- an allocation site, a
    // safepoint poll, or an invoke shape that did not stage keeps the full spill
    // because it has not published anything. Declared here after
    // `flag_declaration_guard` caught it reading through a live `getenv`.
    E { group: Group::JIT, token: "spill-args-published", on_key: Some("CRATONVM_JIT_SPILL_ARGS_PUBLISHED"), off_key: None, off_word: Some("0"), since: "2026-08-27" },
    // Default-ON kill switches from the same wave: each reads
    // `runtime_var(..).map(|v| v != "0").unwrap_or(true)` or the `0`/`false`/
    // `off` spelling of it, so `=0` is the off word and there is no on_key
    // semantics beyond presence.
    E { group: Group::JIT, token: "callee-identity", on_key: Some("CRATONVM_JIT_CALLEE_IDENTITY"), off_key: None, off_word: Some("0"), since: "2026-08-28" },
    E { group: Group::JIT, token: "elide-trivial-ctor", on_key: Some("CRATONVM_JIT_ELIDE_TRIVIAL_CTOR"), off_key: None, off_word: Some("0"), since: "2026-08-28" },
    E { group: Group::JIT, token: "site-cache-stubs", on_key: Some("CRATONVM_JIT_SITE_CACHE_STUBS"), off_key: None, off_word: Some("0"), since: "2026-08-27" },
    // Presence-only, and named as a NEGATIVE, so it is an off_key with no on
    // spelling -- the same shape as `no-atomic-intrinsic` above it.
    E { group: Group::JIT, token: "atomic-long-intrinsic", on_key: None, off_key: Some("CRATONVM_JIT_NO_ATOMIC_LONG_INTRINSIC"), off_word: None, since: "2026-08-27" },
    E { group: Group::JIT, token: "box-unbox-intrinsic", on_key: None, off_key: Some("CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC"), off_word: None, since: "2026-09-02" },
    E { group: Group::JIT, token: "tier-c1-threshold", on_key: Some("CRATONVM_TIER_C1_THRESHOLD"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "tier-c2-min-invocations", on_key: Some("CRATONVM_TIER_C2_MIN_INVOCATIONS"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "tier-c2-threshold", on_key: Some("CRATONVM_TIER_C2_THRESHOLD"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "tier-osr-backedge", on_key: Some("CRATONVM_TIER_OSR_BACKEDGE"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "tier-osr-threshold", on_key: Some("CRATONVM_TIER_OSR_THRESHOLD"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "tier-pgo", on_key: Some("CRATONVM_TIER_PGO"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "tier-pgo-receivers", on_key: Some("CRATONVM_TIER_PGO_RECEIVERS"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "tier-pgo-c2-window", on_key: Some("CRATONVM_TIER_PGO_C2_WINDOW"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "tiered", on_key: Some("CRATONVM_TIER_ENABLED"), off_key: None, off_word: Some("0"), since: "2026-06-22" },
    E { group: Group::JIT, token: "tlab-zero-elision", on_key: None, off_key: Some("CRATONVM_NO_JIT_TLAB_ZERO_ELISION"), off_word: None, since: "2026-07-30" },
    // Interpreter-side, but it lives with the execution-engine knobs like
    // `rootsnap-cache`. Default-ON; `0`/`off`/`false`/`no` is the kill switch.
    E { group: Group::JIT, token: "trivial-getter", on_key: Some("CRATONVM_TRIVIAL_GETTER"), off_key: None, off_word: Some("0"), since: "2026-07-30" },
    E { group: Group::JIT, token: "unroll", on_key: Some("CRATONVM_JIT_UNROLL"), off_key: Some("CRATONVM_DISABLE_UNROLL"), off_word: None, since: "2026-05-22" },
    // Vectorized emission (`x64::vec_emit::VecEmitPolicy::from_flags`).
    // Default-**OFF**: unset answers `Disabled`, so unsetting the key is the off
    // state and `off_word` stays `None`. The parser does also read
    // `0`/`false`/`off`/`no`/empty as off, so `Some("0")` would behave
    // identically — it is left out because `off_word` is how a reader tells a
    // default-ON knob from a default-OFF one.
    E { group: Group::JIT, token: "vectorize", on_key: Some("CRATONVM_JIT_VECTORIZE"), off_key: None, off_word: None, since: "2026-08-01" },
    // IR verifier lanes. `verify-ir` is the only *tri-state* knob in the table:
    // unset means "follow the build profile" (on under `debug_assertions`), an
    // explicit `1` forces it on in a release build, and an explicit `0` is also
    // the kill switch for the unconditional pre-lowering check. `off_word: "0"`
    // is what makes `CRATONVM_JIT=-verify-ir` reach that third state instead of
    // merely unsetting the variable back to the profile default.
    E { group: Group::JIT, token: "verify-arena-order", on_key: Some("CRATONVM_JIT_VERIFY_ARENA_ORDER"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::JIT, token: "verify-frame-states", on_key: Some("CRATONVM_JIT_VERIFY_FRAME_STATES"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::JIT, token: "verify-ir", on_key: Some("CRATONVM_JIT_VERIFY_IR"), off_key: None, off_word: Some("0"), since: "2026-07-31" },
    E { group: Group::JIT, token: "verify-memory-chain", on_key: Some("CRATONVM_JIT_VERIFY_MEMORY_CHAIN"), off_key: None, off_word: None, since: "2026-07-31" },
    // Compatibility alias: `check_schedule` was split into the memory-chain and
    // arena-order lanes, and this token still seeds both when neither is set.
    E { group: Group::JIT, token: "lambda-adapter", on_key: Some("CRATONVM_JIT_LAMBDA_ADAPTER"), off_key: None, off_word: None, since: "2026-08-18" },
    E { group: Group::JIT, token: "lambda-capture-adapter", on_key: Some("CRATONVM_JIT_LAMBDA_CAPTURE_ADAPTER"), off_key: None, off_word: None, since: "2026-08-18" },
    E { group: Group::JIT, token: "lambda-const-probe", on_key: Some("CRATONVM_JIT_LAMBDA_CONST_PROBE"), off_key: None, off_word: None, since: "2026-08-23" },
    E { group: Group::JIT, token: "fjp-subclass-blocklist", on_key: Some("CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST"), off_key: None, off_word: None, since: "2026-08-26" },
    E { group: Group::JIT, token: "lambda-site", on_key: Some("CRATONVM_JIT_LAMBDA_SITE"), off_key: None, off_word: None, since: "2026-08-17" },
    E { group: Group::JIT, token: "lambda-tierup", on_key: Some("CRATONVM_JIT_LAMBDA_TIERUP"), off_key: None, off_word: None, since: "2026-08-17" },
    E { group: Group::JIT, token: "verify-schedule", on_key: Some("CRATONVM_JIT_VERIFY_SCHEDULE"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::JIT, token: "verify-types", on_key: Some("CRATONVM_JIT_VERIFY_TYPES"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::JIT, token: "virtual-tierup", on_key: Some("CRATONVM_JIT_VIRTUAL_TIERUP"), off_key: None, off_word: None, since: "2026-06-14" },
    E { group: Group::JIT, token: "xt-helper-window-discharge", on_key: Some("CRATONVM_XT_HELPER_WINDOW_DISCHARGE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "xt-pinned-peer-depth", on_key: Some("CRATONVM_XT_PINNED_PEER_DEPTH"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "xt-pinned-peer-publish-only", on_key: Some("CRATONVM_XT_PINNED_PEER_PUBLISH_ONLY"), off_key: None, off_word: None, since: "2026-09-02" },
    // Credit the pinned-peer depth even on a collector that cannot honour the
    // pin. Default OFF, which is the corrected behaviour; setting it restores
    // the ten-second H2 SIGSEGV, so the fix has a positive control rather than
    // only an absence of crashes.
    E { group: Group::JIT, token: "xt-pinned-peer-unpinnable", on_key: Some("CRATONVM_XT_PINNED_PEER_UNPINNABLE"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "xt-peer-shadow-scan", on_key: Some("CRATONVM_XT_PEER_SHADOW_SCAN"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "dbg-stale-frame-words", on_key: Some("CRATONVM_DBG_STALE_FRAME_WORDS"), off_key: None, off_word: None, since: "2026-09-03" },
    E { group: Group::JIT, token: "pin-unnamed-frame-refs", on_key: Some("CRATONVM_JIT_PIN_UNNAMED_FRAME_REFS"), off_key: None, off_word: None, since: "2026-09-03" },
    E { group: Group::JIT, token: "remap-unmapped-dupes", on_key: Some("CRATONVM_JIT_REMAP_UNMAPPED_DUPES"), off_key: None, off_word: None, since: "2026-09-03" },
    E { group: Group::GC, token: "zgc-jit-blanket-refusal", on_key: Some("CRATONVM_ZGC_JIT_BLANKET_REFUSAL"), off_key: None, off_word: None, since: "2026-09-03" },
    // Default-ON since 2026-09-05, so it takes an `off_word`: presence alone no
    // longer decides it and `=0` has to be able to turn it off.
    E { group: Group::JIT, token: "local-mask-unreached-fail-closed", on_key: Some("CRATONVM_JIT_LOCAL_MASK_UNREACHED_FAIL_CLOSED"), off_key: None, off_word: Some("0"), since: "2026-09-03" },
    E { group: Group::GC, token: "blocked-wake-jit-remap", on_key: Some("CRATONVM_BLOCKED_WAKE_JIT_REMAP"), off_key: None, off_word: None, since: "2026-09-03" },
    E { group: Group::JIT, token: "xt-keep-unrewritable-on-discharge", on_key: Some("CRATONVM_XT_KEEP_UNREWRITABLE_ON_DISCHARGE"), off_key: None, off_word: None, since: "2026-09-04" },
    E { group: Group::GC, token: "zgc-unrewritable-peer-refuses", on_key: Some("CRATONVM_ZGC_UNREWRITABLE_PEER_REFUSES"), off_key: None, off_word: None, since: "2026-09-04" },
    E { group: Group::JIT, token: "xt-helper-window-pin-resolve", on_key: Some("CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE"), off_key: None, off_word: None, since: "2026-09-04" },
    E { group: Group::JIT, token: "xt-helper-window-interior", on_key: Some("CRATONVM_XT_HELPER_WINDOW_INTERIOR"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::JIT, token: "xt-helper-window-pin", on_key: Some("CRATONVM_XT_HELPER_WINDOW_PIN"), off_key: None, off_word: None, since: "2026-09-01" },
    E { group: Group::JIT, token: "xt-helper-window-scan", on_key: Some("CRATONVM_XT_HELPER_WINDOW_SCAN"), off_key: None, off_word: None, since: "2026-07-02" },
    E { group: Group::JIT, token: "xt-jit-root-scan", on_key: Some("CRATONVM_XT_JIT_ROOT_SCAN"), off_key: None, off_word: None, since: "2026-06-23" },
    // Value token, milliseconds: `CRATONVM_JIT=xt-peer-deadline-ms=50`. Unset
    // means the built-in 20 ms, and `0` is rejected by the parser's own filter,
    // so there is no off state to spell.
    E { group: Group::JIT, token: "xt-peer-deadline-ms", on_key: Some("CRATONVM_XT_PEER_DEADLINE_MS"), off_key: None, off_word: None, since: "2026-08-01" },
    E { group: Group::JIT, token: "xt-peer-total-ms", on_key: Some("CRATONVM_XT_PEER_TOTAL_MS"), off_key: None, off_word: None, since: "2026-08-05" },
    // `zero-spid` is default ON and `CRATONVM_JIT_ZERO_SPID=0` restores the
    // pre-fix behaviour, so `off_word` is exactly `"0"` -- the same shape as
    // `verify-ir` above. `zero_sp_id_slot_enabled` reads the key and treats
    // `0`/`false`/`FALSE` as off; only `"0"` is spellable as a group token, and
    // the other two spellings keep working through the key itself.
    // `ir-gc-point-maps` is default ON and `CRATONVM_JIT_IR_GC_POINT_MAPS=0`
    // restores the two IR GC-capable sites that recorded no safepoint (the
    // cooperative poll and `Op::New`), so `off_word` is exactly `"0"` -- the
    // same shape as `zero-spid` below.
    E { group: Group::JIT, token: "direct-call-arg-maps", on_key: Some("CRATONVM_JIT_DIRECT_CALL_ARG_MAPS"), off_key: None, off_word: Some("0"), since: "2026-08-30" },
    E { group: Group::JIT, token: "self-call-arg-maps", on_key: Some("CRATONVM_JIT_SELF_CALL_ARG_MAPS"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "merge-marks-exact", on_key: Some("CRATONVM_JIT_MERGE_MARKS_EXACT"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "inline-oop-coverage", on_key: Some("CRATONVM_JIT_INLINE_OOP_COVERAGE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "loader-blind-cp-resolve", on_key: Some("CRATONVM_JIT_LOADER_BLIND_CP_RESOLVE"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "local-mask-fail-closed", on_key: Some("CRATONVM_JIT_LOCAL_MASK_FAIL_CLOSED"), off_key: None, off_word: Some("0"), since: "2026-09-03" },
    E { group: Group::JIT, token: "wide-local-oop-maps", on_key: Some("CRATONVM_JIT_WIDE_LOCAL_OOP_MAPS"), off_key: None, off_word: Some("0"), since: "2026-09-03" },
    E { group: Group::JIT, token: "arm64-safepoints", on_key: Some("CRATONVM_JIT_ARM64_SAFEPOINTS"), off_key: None, off_word: Some("0"), since: "2026-09-03" },
    E { group: Group::JIT, token: "ir-gc-point-maps", on_key: Some("CRATONVM_JIT_IR_GC_POINT_MAPS"), off_key: None, off_word: Some("0"), since: "2026-08-30" },
    E { group: Group::JIT, token: "zero-spid", on_key: Some("CRATONVM_JIT_ZERO_SPID"), off_key: None, off_word: Some("0"), since: "2026-08-30" },
    E { group: Group::GC, token: "card-metrics", on_key: Some("CRATONVM_GC_CARD_METRICS"), off_key: None, off_word: None, since: "2026-07-31" },
    // NOTE: `CRATONVM_DBG_MAPGEN` and `CRATONVM_DBG_VACATED_FRAMES` are declared
    // in the DBG group, which is where their names say they belong and which is
    // the spelling `the_20260817_gc_diagnostics_expand_from_their_group_spelling`
    // asserts. They were declared here as well on 2026-08-17, which made
    // `every_legacy_key_is_claimed_once` red: two tokens claiming one variable
    // leaves its canonical spelling ambiguous, which is the whole point of that
    // invariant. Nothing referenced the `CRATONVM_GC=dbg-*` spellings.
    E { group: Group::GC, token: "card-table-only", on_key: Some("CRATONVM_CARD_TABLE_ONLY"), off_key: None, off_word: None, since: "2026-07-16" },
    E { group: Group::GC, token: "full-rset-scan", on_key: Some("CRATONVM_GC_FULL_RSET_SCAN"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::GC, token: "verify-rset", on_key: Some("CRATONVM_GC_VERIFY_RSET"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::GC, token: "compact-ref-fields", on_key: Some("CRATONVM_COMPACT_REF_FIELDS"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::GC, token: "pack-fields-by-width", on_key: Some("CRATONVM_PACK_FIELDS_BY_WIDTH"), off_key: None, off_word: None, since: "2026-08-06" },
    E { group: Group::GC, token: "layout-scan-cache", on_key: Some("CRATONVM_GC_LAYOUT_SCAN_CACHE"), off_key: None, off_word: None, since: "2026-09-03" },
    E { group: Group::GC, token: "compressed-oops", on_key: Some("CRATONVM_COMPRESSED_OOPS"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::GC, token: "default-heap-ergonomics", on_key: Some("CRATONVM_DEFAULT_HEAP_ERGONOMICS"), off_key: None, off_word: None, since: "2026-06-17" },
    E { group: Group::GC, token: "default-heap-max-mb", on_key: Some("CRATONVM_DEFAULT_HEAP_MAX_MB"), off_key: None, off_word: None, since: "2026-06-17" },
    // Default-ON kill switch, hence `off_key` only: `CRATONVM_GC=-defrag-promote`
    // restores the pre-2026-08-04 non-moving young sweep, which promoted only
    // objects that had reached `PROMOTION_AGE` and therefore had no way out of
    // a free list fragmented below any usable block size.
    E { group: Group::GC, token: "defrag-promote", on_key: None, off_key: Some("CRATONVM_NO_DEFRAG_PROMOTE"), off_word: None, since: "2026-08-04" },
    // Bisection escape hatches for two GC fixes, both opt-out-only. Enabling
    // either *reinstates a known defect* (stale-address writes in post-GC
    // reference processing; monotonic old-gen fragmentation) — they exist for
    // A/B isolation, which is exactly why they belong inside the declared
    // surface rather than behind a live `getenv` nobody can enumerate.
    E { group: Group::GC, token: "exact-refproc-survival", on_key: None, off_key: Some("CRATONVM_NO_EXACT_REFPROC_SURVIVAL"), off_word: None, since: "2026-07-31" },
    E { group: Group::GC, token: "referent-identity-screen", on_key: None, off_key: Some("CRATONVM_NO_REFERENT_IDENTITY_SCREEN"), off_word: None, since: "2026-08-23" },
    E { group: Group::GC, token: "g1-coverage-pin", on_key: Some("CRATONVM_G1_COVERAGE_PIN"), off_key: None, off_word: None, since: "2026-08-07" },
    E { group: Group::GC, token: "g1-pin-empty-publication", on_key: Some("CRATONVM_G1_PIN_EMPTY_PUBLICATION"), off_key: None, off_word: None, since: "2026-08-20" },
    E { group: Group::GC, token: "g1-precise-only-roots", on_key: Some("CRATONVM_G1_PRECISE_ONLY_ROOTS"), off_key: None, off_word: None, since: "2026-08-20" },
    E { group: Group::GC, token: "precise-only-roots", on_key: Some("CRATONVM_GC_PRECISE_ONLY_ROOTS"), off_key: None, off_word: None, since: "2026-08-22" },
    // --- composition residuals, 2026-09-02 ----------------------------------
    // Three default-off diagnostics and four A/B switches from
    // `completablefuture-composition-is-20x-and-5-percent-compiled-CLOSED-20260902.md`.
    // Every one of them exists so a claim on that page can be re-priced in ONE
    // binary; two of them were built specifically to refute a hypothesis, and
    // one of those did.
    E { group: Group::DBG, token: "interp-frames", on_key: Some("CRATONVM_DBG_INTERP_FRAMES"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::DBG, token: "tierup-decline", on_key: Some("CRATONVM_DBG_TIERUP_DECLINE"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::DBG, token: "direct-binds", on_key: Some("CRATONVM_DBG_DIRECT_BINDS"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::JIT, token: "int-value-direct", on_key: Some("CRATONVM_JIT_INT_VALUE_DIRECT"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "indy-lambda-fast", on_key: Some("CRATONVM_JIT_INDY_LAMBDA_FAST"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "hot-lookup-cache", on_key: Some("CRATONVM_JIT_HOT_LOOKUP_CACHE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "virtual-nominate-always", on_key: Some("CRATONVM_JIT_VIRTUAL_NOMINATE_ALWAYS"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::JIT, token: "virtual-promote-java-util", on_key: Some("CRATONVM_JIT_VIRTUAL_PROMOTE_JAVA_UTIL"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::JIT, token: "native-cf-postcomplete-skip", on_key: Some("CRATONVM_NATIVE_CF_POSTCOMPLETE_SKIP"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "native-cf-postcomplete-direct", on_key: Some("CRATONVM_NATIVE_CF_POSTCOMPLETE_DIRECT"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    // `CRATONVM_JIT_IR_COLD_ARG_STAGE` was declared here too, as a courtesy,
    // and dev declared it concurrently -- both rows merged with no conflict,
    // which is the append-anywhere hazard. Dev's row is kept; this note is the
    // tombstone so the next session does not re-add a third.
    E { group: Group::GC, token: "g1-evac-retry", on_key: None, off_key: Some("CRATONVM_G1_NO_EVAC_RETRY"), off_word: None, since: "2026-07-03" },
    E { group: Group::GC, token: "g1-retire-forwards-late", on_key: Some("CRATONVM_G1_RETIRE_FORWARDS_LATE"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::GC, token: "g1-reevac-guard", on_key: Some("CRATONVM_G1_REEVAC_GUARD"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::GC, token: "g1-live-region-memo", on_key: None, off_key: Some("CRATONVM_G1_NO_LIVE_REGION_MEMO"), off_word: None, since: "2026-08-17" },
    E { group: Group::GC, token: "g1-parallel-evac", on_key: Some("CRATONVM_G1_PARALLEL_EVAC"), off_key: None, off_word: Some("0"), since: "2026-06-21" },
    E { group: Group::GC, token: "g1-eager-humongous", on_key: Some("CRATONVM_G1_EAGER_HUMONGOUS"), off_key: None, off_word: Some("0"), since: "2026-08-13" },
    E { group: Group::GC, token: "g1-young-pause-target", on_key: Some("CRATONVM_G1_YOUNG_PAUSE_TARGET"), off_key: None, off_word: Some("0"), since: "2026-08-18" },
    E { group: Group::GC, token: "g1-humongous-marks", on_key: Some("CRATONVM_G1_HUMONGOUS_MARKS"), off_key: None, off_word: None, since: "2026-09-05" },
    E { group: Group::GC, token: "g1-ihop-counts-regions", on_key: Some("CRATONVM_G1_IHOP_COUNTS_REGIONS"), off_key: None, off_word: None, since: "2026-09-05" },
    E { group: Group::GC, token: "g1-jit-mark-driver", on_key: Some("CRATONVM_G1_JIT_MARK_DRIVER"), off_key: None, off_word: None, since: "2026-09-05" },
    E { group: Group::GC, token: "g1-verify-holders", on_key: Some("CRATONVM_G1_VERIFY_HOLDERS"), off_key: None, off_word: None, since: "2026-09-05" },
    E { group: Group::GC, token: "g1-scrub-free", on_key: Some("CRATONVM_G1_SCRUB_FREE"), off_key: None, off_word: None, since: "2026-08-18" },
    E { group: Group::GC, token: "g1-narrow-fixup", on_key: Some("CRATONVM_G1_NARROW_FIXUP"), off_key: None, off_word: Some("0"), since: "2026-08-18" },
    E { group: Group::GC, token: "g1-parallel-evac-in-jit", on_key: Some("CRATONVM_G1_PARALLEL_EVAC_IN_JIT"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "g1-parallel-evac-screen", on_key: Some("CRATONVM_G1_PARALLEL_EVAC_SCREEN"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    E { group: Group::GC, token: "g1-evac-copy-watch", on_key: Some("CRATONVM_G1_EVAC_COPY_WATCH"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::GC, token: "g1-parallel-evac-shared-dest", on_key: Some("CRATONVM_G1_PARALLEL_EVAC_SHARED_DEST"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::GC, token: "g1-parallel-evac-resume-dest", on_key: Some("CRATONVM_G1_PARALLEL_EVAC_RESUME_DEST"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::GC, token: "g1-evac-ref-implausible-refuse", on_key: Some("CRATONVM_G1_EVAC_REF_IMPLAUSIBLE_REFUSE"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::GC, token: "g1-cleanup-walk", on_key: Some("CRATONVM_G1_CLEANUP_WALK"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::GC, token: "g1-adaptive-ihop", on_key: Some("CRATONVM_G1_ADAPTIVE_IHOP"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "g1-adaptive-tenuring", on_key: Some("CRATONVM_G1_ADAPTIVE_TENURING"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "gc-reserve", on_key: Some("CRATONVM_GC_RESERVE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "zgc-markbits", on_key: Some("CRATONVM_ZGC_MARKBITS"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "zgc-page-pinned-relocate", on_key: Some("CRATONVM_ZGC_PAGE_PINNED_RELOCATE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "zgc-parsweep", on_key: Some("CRATONVM_ZGC_PARSWEEP"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::GC, token: "zgc-bitmap-sweep", on_key: Some("CRATONVM_ZGC_BITMAP_SWEEP"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "zgc-mark-root-filter", on_key: Some("CRATONVM_ZGC_MARK_ROOT_FILTER"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "zgc-jit-tlab", on_key: Some("CRATONVM_ZGC_JIT_TLAB"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::GC, token: "tlab-flag-publish-false", on_key: Some("CRATONVM_ZGC_TLAB_FLAG_PUBLISH_FALSE"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::GC, token: "zgc-tlab-tail-sink", on_key: Some("CRATONVM_ZGC_TLAB_TAIL_SINK"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "zgc-mark-pool-persistent", on_key: Some("CRATONVM_ZGC_MARK_POOL_PERSISTENT"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "g1-reserve-heap", on_key: Some("CRATONVM_G1_RESERVE_HEAP"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "g1-uncommit", on_key: Some("CRATONVM_G1_UNCOMMIT"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::GC, token: "g1-card-rset", on_key: Some("CRATONVM_G1_CARD_RSET"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "g1-card-clean", on_key: Some("CRATONVM_G1_CARD_CLEAN"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::GC, token: "g1-card-screen-jit-pinned", on_key: Some("CRATONVM_G1_CARD_SCREEN_JIT_PINNED"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "g1-inline-barrier", on_key: Some("CRATONVM_G1_INLINE_BARRIER"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "g1-mark-lock-yield", on_key: Some("CRATONVM_G1_MARK_LOCK_YIELD"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "g1-shared-alloc", on_key: Some("CRATONVM_G1_SHARED_ALLOC"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "g1-eden-stripes", on_key: Some("CRATONVM_G1_EDEN_STRIPES"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::GC, token: "g1-parallel-mark", on_key: Some("CRATONVM_G1_PARALLEL_MARK"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::DBG, token: "g1-dbg-rset", on_key: Some("CRATONVM_G1_DBG_RSET"), off_key: None, off_word: None, since: "2026-08-18" },
    E { group: Group::GC, token: "g1-workers", on_key: Some("CRATONVM_G1_WORKERS"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::GC, token: "g1-rset-source-cap", on_key: Some("CRATONVM_G1_RSET_SOURCE_CAP"), off_key: None, off_word: None, since: "2026-08-13" },
    E { group: Group::GC, token: "g1-verify-budget", on_key: Some("CRATONVM_G1_VERIFY_BUDGET"), off_key: None, off_word: None, since: "2026-08-13" },
    // `cuda_bridge::critical` — millisecond budgets, trimmed `u64`, latched in
    // an `AtomicU64` on first read. Value tokens:
    // `CRATONVM_GC=gpu-critical-wait-ms=250`.
    E { group: Group::GC, token: "gpu-chunk-streams", on_key: Some("CRATONVM_GPU_CHUNK_STREAMS"), off_key: None, off_word: None, since: "2026-08-22" },
    E { group: Group::GC, token: "gpu-chunks", on_key: Some("CRATONVM_GPU_CHUNKS"), off_key: None, off_word: None, since: "2026-08-22" },
    // AUDIT 2026-09-02. `gpu-dispatch-streams` sizes the round-robin pool the
    // handle-less dispatch path takes a stream from instead of creating and
    // destroying one per submission; set it to 1 for the strictest ordering.
    // `gpu-jit-array-writers=allow` picks the other side of the
    // JIT-versus-residency-cache trade for a method that stores into a
    // primitive array — see `vm::runtime::offload_jit_gate::ArrayWriterPolicy`
    // for the measurement that made blocking the JIT the default.
    // The fitted admission cost model of
    // docs/gpu/offload-crossover-and-min-work-20260904.md, opt-in. `=1`
    // replaces nothing -- it runs BESIDE `--gpu-min-work`, refusing work
    // the scalar threshold admits at a loss. Off by default because the
    // four fitted constants are one device's.
    E { group: Group::GC, token: "gpu-admit-model", on_key: Some("CRATONVM_GPU_ADMIT_MODEL"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::GC, token: "gpu-dispatch-streams", on_key: Some("CRATONVM_GPU_DISPATCH_STREAMS"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::GC, token: "gpu-jit-array-writers", on_key: Some("CRATONVM_GPU_JIT_ARRAY_WRITERS"), off_key: None, off_word: None, since: "2026-09-02" },
    // The compiled-tier GPU input-residency barrier (2026-09-04). ON by
    // default on x86_64 under `--gpu`; `=0` refuses to arm it, which
    // puts `offload_jit_gate` back to refusing JIT admission to every
    // method that writes a primitive array. See `jit::gpu_barrier`.
    E { group: Group::GC, token: "jit-gpu-array-barrier", on_key: Some("CRATONVM_JIT_GPU_ARRAY_BARRIER"), off_key: None, off_word: None, since: "2026-09-04" },
    // `=0` puts `offload_jit_gate` back to blocking the caller of ANY
    // analyzer-Eligible `invokestatic`, instead of only one the
    // DISPATCHER could actually launch. The control arm for that
    // narrowing; see `offload_jit_gate::target_can_ever_dispatch`.
    E { group: Group::GC, token: "gpu-jit-gate-dispatchable", on_key: Some("CRATONVM_GPU_JIT_GATE_DISPATCHABLE"), off_key: None, off_word: None, since: "2026-09-04" },
    // `=0` restores the pre-2026-09-06 compiled site memo: a site whose
    // target is not in the offload registry the first time it executes is
    // written off as NotKernel forever, instead of asking the gate once the
    // class exists. The CONTROL ARM for the forward-reference fix -- with it
    // off, `GpuForwardRef forward` goes dark and `GpuForwardRef preload` does
    // not, on one binary. See `offload_jit_gate`'s module docs.
    E { group: Group::GC, token: "gpu-jit-gate-late-register", on_key: Some("CRATONVM_GPU_JIT_GATE_LATE_REGISTER"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    // `=0` drops the caller-blocking half of `offload_jit_gate`: a caller
    // of an eligible kernel compiles, and offload silently ends there.
    // A CONTROL ARM, not a production setting -- it isolates the cost of
    // the refusal from everything else `--gpu` changes.
    E { group: Group::GC, token: "gpu-jit-gate-callers", on_key: Some("CRATONVM_GPU_JIT_GATE_CALLERS"), off_key: None, off_word: None, since: "2026-09-04" },
    E { group: Group::GC, token: "gpu-critical-lease-ms", on_key: Some("CRATONVM_GPU_CRITICAL_LEASE_MS"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::GC, token: "gpu-critical-wait-ms", on_key: Some("CRATONVM_GPU_CRITICAL_WAIT_MS"), off_key: None, off_word: None, since: "2026-07-31" },
    // A/B levers declared 2026-09-02 with the four GPU-subsystem fixes.
    // `gpu-host-callback` restores the per-launch `cuLaunchHostFunc` the
    // completion reaper no longer registers (a host function blocks the
    // launches queued behind it on its stream). `gpu-device-pool` is
    // DEFAULT-ON with a "0" off-word: the bridge's device-allocation pool has
    // no observable semantics, so the only honest way to price it is one
    // binary both ways.
    E { group: Group::GC, token: "gpu-host-callback", on_key: Some("CRATONVM_GPU_HOST_CALLBACK"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::GC, token: "gpu-device-pool", on_key: Some("CRATONVM_GPU_DEVICE_POOL"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    // How many carriers the FFM element fast path remembers per thread. `1`
    // is the single slot it replaced, which is the control arm: kfusion's
    // integration alternates the TSDF volume with the images it reads, and
    // one slot published 38.3M native verdicts for 129M consults.
    E { group: Group::JIT, token: "ffm-verdict-ways", on_key: Some("CRATONVM_FFM_VERDICT_WAYS"), off_key: None, off_word: None, since: "2026-09-02" },
    // `gpu-wait-latch` is DEFAULT-ON with a "0" off-word, same argument as
    // `gpu-device-pool`: skipping a `cuStreamWaitEvent` on an event that has
    // already fired has no observable semantics, so the only honest way to
    // price it is one binary both ways.
    E { group: Group::GC, token: "gpu-wait-latch", on_key: Some("CRATONVM_GPU_WAIT_LATCH"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "gpu-zerocopy", on_key: None, off_key: Some("CRATONVM_GPU_NO_ZEROCOPY"), off_word: None, since: "2026-06-16" },
    E { group: Group::GC, token: "gpu-submission-drain", on_key: None, off_key: Some("CRATONVM_GPU_NO_SUBMISSION_DRAIN"), off_word: None, since: "2026-09-02" },
    // Measurement lever: root every heap-backed LinkedHashMap overlay entry
    // again, restoring the unbounded young-gen pinning the skip-set removed.
    E { group: Group::GC, token: "lhm-root-all", on_key: Some("CRATONVM_LHM_ROOT_ALL"), off_key: None, off_word: None, since: "2026-08-01" },
    E { group: Group::GC, token: "max-inflated-bytes", on_key: Some("CRATONVM_MAX_INFLATED_BYTES"), off_key: None, off_word: None, since: "2026-06-17" },
    E { group: Group::GC, token: "mirror-pin-young-defer", on_key: None, off_key: Some("CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER"), off_word: None, since: "2026-07-27" },
    E { group: Group::GC, token: "moving-young", on_key: Some("CRATONVM_MOVING_YOUNG"), off_key: Some("CRATONVM_NO_MOVING_YOUNG"), off_word: None, since: "2026-06-17" },
    E { group: Group::GC, token: "moving-young-jit-frames", on_key: None, off_key: Some("CRATONVM_MOVING_YOUNG_NO_JIT"), off_word: None, since: "2026-07-30" },
    E { group: Group::GC, token: "format-arg-pin", on_key: None, off_key: Some("CRATONVM_NO_FORMAT_ARG_PIN"), off_word: None, since: "2026-08-22" },
    // Declared 2026-08-24. An OPT-OUT: the JIT entry chain moves the compile-id
    // mirror together with the RBP mirror by default. This key restores the
    // behaviour where only the RBP half was reset on push and restored on pop,
    // which left the pair naming two different frames. See
    // `conservative_roots::reload_top_rbp_cache`.
    // Declared 2026-08-26. An OPT-OUT: in the regions the abstract interpreter
    // MODELS (java locals, operand spill), the band verifier treats a slot the
    // ACTIVE safepoint map does not name as DEAD rather than demanding it be
    // published. This key restores the stricter reading. See
    // `conservative_roots::band_slot_is_verifiable_with_map`.
    // Declared 2026-08-28. Opt-IN: an allocation failure names the window
    // the next collection should empty, and pages in it bypass the
    // profitability ranking. This key restores the pure ranking. See
    // `zgc::ZRelocationPolicy::target_pages`.
    E { group: Group::GC, token: "targeted-compaction", on_key: Some("CRATONVM_ZGC_TARGETED_COMPACTION"), off_key: None, off_word: None, since: "2026-08-28" },
    E { group: Group::GC, token: "band-map-liveness", on_key: None, off_key: Some("CRATONVM_GC_NO_BAND_MAP_LIVENESS"), off_word: None, since: "2026-08-26" },
    E { group: Group::GC, token: "cm-id-pairing", on_key: None, off_key: Some("CRATONVM_GC_NO_CM_ID_PAIRING"), off_word: None, since: "2026-08-26" },
    E { group: Group::GC, token: "register-image-remap", on_key: Some("CRATONVM_REGISTER_IMAGE_REMAP"), off_key: None, off_word: None, since: "2026-08-22" },
    // Resolving the innermost JIT frame's own method from the direct CALL that
    // built it, instead of giving up whenever that method is not the chain
    // entry's. Presence-parsed opt-out, so `=0` still disables it and
    // `off_word` must stay `None`.
    E { group: Group::GC, token: "innermost-callee-resolve", on_key: None, off_key: Some("CRATONVM_GC_NO_CALLEE_RESOLVE"), off_word: None, since: "2026-08-06" },
    E { group: Group::GC, token: "old-interior-pins", on_key: None, off_key: Some("CRATONVM_GC_NO_OLD_INTERIOR_PINS"), off_word: None, since: "2026-08-02" },
    E { group: Group::GC, token: "empty-object-run", on_key: None, off_key: Some("CRATONVM_GC_NO_EMPTY_OBJECT_RUN"), off_word: None, since: "2026-08-13" },
    E { group: Group::GC, token: "oldgen-coalesce", on_key: None, off_key: Some("CRATONVM_NO_OLDGEN_COALESCE"), off_word: None, since: "2026-08-01" },
    E { group: Group::GC, token: "oldgen-compact", on_key: Some("CRATONVM_OLDGEN_COMPACT"), off_key: None, off_word: None, since: "2026-08-03" },
    E { group: Group::GC, token: "overhead-limit", on_key: Some("CRATONVM_GC_OVERHEAD_LIMIT"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::GC, token: "owner-class-filter", on_key: Some("CRATONVM_OWNER_CLASS_FILTER"), off_key: None, off_word: None, since: "2026-08-01" },
    E { group: Group::GC, token: "par-min-bytes", on_key: Some("CRATONVM_GC_PAR_MIN_BYTES"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::GC, token: "par-evac", on_key: Some("CRATONVM_GC_PAR_EVAC"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "par-threads", on_key: Some("CRATONVM_GC_PAR_THREADS"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::GC, token: "sync-young-wipe", on_key: Some("CRATONVM_GC_SYNC_YOUNG_WIPE"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::GC, token: "jit-ref-store-gates", on_key: Some("CRATONVM_GC_JIT_REF_STORE_GATES"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    // Opt-IN: the interpreter's TLAB fast path plans the COMPACT body shape,
    // the one the JIT's inline `new` and the TLAB-miss path already use. Off by
    // default because the single previous attempt at this unification
    // miscompiled `probes/FjpProbe.java`; see `compact_tlab_alloc_enabled`.
    // Default-ON opt-out since 2026-09-03: `compact_tlab_alloc_enabled` reads
    // `0`. It shipped opt-in the same day and earned the default with a
    // 228-program differential soak per collector, 89/89 on the
    // HotSpot-differential regression suite with the shape enabled, and a real
    // application allocating 87.5 MB less.
    E { group: Group::GC, token: "compact-tlab-alloc", on_key: Some("CRATONVM_COMPACT_TLAB_ALLOC"), off_key: None, off_word: Some("0"), since: "2026-09-03" },
    // Bisection levers for the shape above: which sites may plan compact, and
    // which classes actually did. Both exist because the first miscompile it
    // exposed cost a rebuild per hypothesis until they did not.
    E { group: Group::GC, token: "compact-tlab-sites", on_key: Some("CRATONVM_COMPACT_TLAB_SITES"), off_key: None, off_word: None, since: "2026-09-03" },
    // Default-ON opt-out: `collect_roots` records the ADDRESS of every static
    // slot it finds holding an object, and `update_all_roots` walks that list
    // instead of re-sweeping every slot of every class under the `statics`
    // write lock. `=0` restores the full walk, so the two are one binary apart
    // rather than one build apart. Statics are the first root class in this VM
    // carried as a SLOT rather than a value -- see `memory::roots::
    // STATIC_REF_SLOTS` for why they are the ones that can be.
    // Default-ON opt-out: hand the evacuated young semi-space back to the OS at
    // the end of each young collection instead of only zeroing it. The
    // generational collector was the one backend that never gave memory back at
    // all -- ZGC does it by default and G1 on request.
    //
    // Off for one day (2026-09-06) while the fault it makes loud was open: a
    // decommitted granule faults on touch, so any stale young reference in a
    // compiled frame becomes a SIGSEGV rather than a silent stale read, and one
    // was reachable in ten seconds on the H2 JDBC corpus. That defect is fixed
    // (`VmHeap::honours_conservative_pins`) and the default is back. `=0`
    // remains the first thing to set if a compiled frame faults on a young
    // address -- see `Flags::gen_uncommit` for the whole arc.
    // Opt-in: maintain an EXACT object-start bitmap
    // on the arenas whose owner asks for one (a bit per 8
    // bytes, set in `hand_out`, cleared in `add_free_block` and on reset) and
    // let `is_object_address` answer from it instead of deducing the answer
    // from header bytes. Consulted in the ACCEPT direction only -- a miss falls
    // through to the deduction, because the bitmap is knowably incomplete for
    // TLAB-allocated objects and using it to REJECT would drop live roots.
    //
    // It WAS default-ON from 2026-09-05 to 2026-09-06. The flip was reverted
    // because its 7% came from a sequential ABBA on a shared box, and concurrent
    // paired arms reproduce no win on either shape -- see
    // `arena::object_starts_enabled` for the two tables.
    E { group: Group::GC, token: "object-starts", on_key: Some("CRATONVM_GC_OBJECT_STARTS"), off_key: None, off_word: None, since: "2026-09-05" },
    E { group: Group::GC, token: "gen-uncommit", on_key: Some("CRATONVM_GEN_UNCOMMIT"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    E { group: Group::GC, token: "static-root-slots", on_key: Some("CRATONVM_GC_STATIC_ROOT_SLOTS"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    // Default-ON opt-out: the young-GC trigger predicate reads `used`,
    // `free_list_bytes` and `capacity` from a triple republished by the
    // `young_from` mutex guard's `Drop`, instead of taking that mutex on the
    // ALLOCATION path. `=0` takes the lock, so the two are one binary apart.
    E { group: Group::GC, token: "trigger-lockfree", on_key: Some("CRATONVM_GC_TRIGGER_LOCKFREE"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::GC, token: "dbg-compact-tlab", on_key: Some("CRATONVM_DBG_COMPACT_TLAB"), off_key: None, off_word: None, since: "2026-09-03" },
    E { group: Group::GC, token: "promotion-guard", on_key: None, off_key: Some("CRATONVM_NO_GC_PROMOTION_GUARD"), off_word: None, since: "2026-06-21" },
    E { group: Group::GC, token: "promotion-oom-guard-broad", on_key: Some("CRATONVM_PROMOTION_OOM_GUARD_BROAD"), off_key: None, off_word: None, since: "2026-06-23" },
    E { group: Group::GC, token: "selective-promote", on_key: None, off_key: Some("CRATONVM_NO_SELECTIVE_PROMOTE"), off_word: None, since: "2026-06-05" },
    E { group: Group::GC, token: "stress", on_key: Some("CRATONVM_GC_STRESS"), off_key: None, off_word: None, since: "2026-06-18" },
    E { group: Group::GC, token: "young-trigger-percent", on_key: Some("CRATONVM_GC_YOUNG_TRIGGER_PERCENT"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::GC, token: "sweep-anchor-stride", on_key: Some("CRATONVM_GC_SWEEP_ANCHOR_STRIDE"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::GC, token: "tlab-gc-trigger", on_key: Some("CRATONVM_TLAB_GC_TRIGGER"), off_key: None, off_word: None, since: "2026-07-24" },
    E { group: Group::GC, token: "weakref-clear", on_key: Some("CRATONVM_WEAKREF_CLEAR"), off_key: None, off_word: None, since: "2026-06-29" },
    E { group: Group::GC, token: "youngscan-stride", on_key: Some("CRATONVM_YOUNGSCAN_STRIDE"), off_key: None, off_word: None, since: "2026-06-05" },
    // Both ZGC rows are **kill switches for default-ON machinery**, not opt-ins,
    // and both were declared as opt-ins until 2026-08-13. `zgc-startbits` gates
    // the object-start bitmap (`zgc_start_bits_enabled_by_default`) and
    // `zgc-tlab` gates the ZGC thread-local buffers
    // (`zgc_tlab_enabled_by_default`); each returns `true` on an unset key and
    // reads `0`/`off`/`false`/`no` as false. With `off_word: None`,
    // `CRATONVM_GC=-zgc-tlab` expanded to *unsetting* the key — which leaves the
    // feature ON, so the documented opt-out was silently inert, and the
    // generated `flag-inventory.md` row said `opt-in | off` about a default-ON
    // knob. Same defect, same fix and the same reasoning as
    // `young-pause-goal-ms` two rows below.
    // Phase 3/4 of the ZGC maturity plan. Both are default-OFF opt-ins and so
    // carry NO `off_word` -- unlike their two neighbours below, which gate
    // default-ON machinery. `parmark` is a WORKER COUNT parsed with
    // `parse::<usize>()`, so unsetting it is the off state; `relocate` is
    // presence-and-value (`1`/`on`/`true`/`yes`), and an `off_word` of "0"
    // would be read as false by that parser anyway -- it stays `None` because
    // that field is how a reader tells a default-ON knob from a default-OFF
    // one, which is exactly the confusion that made both ZGC rows below wrong
    // until 2026-08-13.
    // BOTH became DEFAULT-ON on 2026-08-13, so both grew an `off_word` -- the
    // same correction the two rows below needed, for the same reason: without
    // it `CRATONVM_GC=-zgc-parmark` expands to UNSETTING the key, and an unset
    // key now means ON. `parmark` is a worker count whose parser reads 0 as
    // serial; `relocate` reads 0/off/false/no as off.
    E { group: Group::GC, token: "zgc-parmark", on_key: Some("CRATONVM_ZGC_PARMARK"), off_key: None, off_word: Some("0"), since: "2026-08-13" },
    E { group: Group::GC, token: "zgc-relocate", on_key: Some("CRATONVM_ZGC_RELOCATE"), off_key: None, off_word: Some("0"), since: "2026-08-13" },
    // Declared 2026-08-21 with the per-cycle relocation proof. Default-ON,
    // so a KILL SWITCH, with the same `off_word: Some("0")` correction as
    // its neighbours: ZGC may compact while a compiled frame is live
    // whenever the collection PROVED that frame's oops rewritable, and `=0`
    // restores the older refusal that fired on the mere existence of a
    // compiled frame -- which is why the default configuration never
    // defragmented. See `gc/src/zgc.rs::zgc_relocate_under_proven_jit`.
    E { group: Group::GC, token: "zgc-assume-rewritable", on_key: Some("CRATONVM_ZGC_ASSUME_REWRITABLE"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::GC, token: "zgc-relocate-proven-jit", on_key: Some("CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT"), off_key: None, off_word: Some("0"), since: "2026-08-21" },
    // Declared 2026-08-29 with the LARGE-OBJECT end's compactor. Default-ON,
    // so a KILL SWITCH with the same `off_word: Some("0")` as its neighbours:
    // `=0` leaves the arena's high end exactly as it was before, which is the
    // same-binary A/B for a repair whose whole claim is that the requests
    // failing at 97 % free came from an end nothing could relocate. See
    // `gc/src/zgc.rs::zgc_high_compaction_enabled`.
    E { group: Group::GC, token: "zgc-high-compaction", on_key: Some("CRATONVM_ZGC_HIGH_COMPACTION"), off_key: None, off_word: Some("0"), since: "2026-08-29" },
    // Declared 2026-08-29 with the starved TLAB-refill floor. Default-ON, so a
    // KILL SWITCH: `=0` restores the unconditional `want / 8` floor, which is
    // what let a starved bump spend the large-object reserve on TLAB churn.
    // See `gc/src/zgc.rs::starved_recycle_enabled`.
    E { group: Group::GC, token: "zgc-tlab-starved-recycle", on_key: Some("CRATONVM_ZGC_TLAB_STARVED_RECYCLE"), off_key: None, off_word: Some("0"), since: "2026-08-29" },
    // Declared 2026-08-29 with the vacated-span publication. Default-ON, so a
    // KILL SWITCH: `=0` restores reclaim-is-the-cursor-drop-and-nothing-else,
    // which lost every byte a slide emptied below a cursor it could not move.
    // See `gc/src/arena.rs::compact_low_to`.
    E { group: Group::GC, token: "zgc-publish-vacated", on_key: Some("CRATONVM_ZGC_PUBLISH_VACATED"), off_key: None, off_word: Some("0"), since: "2026-08-29" },
    // Declared 2026-08-23 with the cross-thread JIT coverage handshake.
    // Default-ON, so a KILL SWITCH, with the same `off_word: Some("0")` as its
    // neighbours: `=0` restores the blanket "any peer inside compiled code
    // makes this cycle unprovable" refusal, which is what left a many-threaded
    // workload with no defragmentation at all. See
    // `vm/src/jit/conservative_roots.rs::xt_jit_coverage_handshake_enabled`.
    // A measurement instrument, default-OFF and read with `is_some()`, so any
    // value turns it on and there is no off word to spell.
    E { group: Group::GC, token: "xt-jit-coverage-assume", on_key: Some("CRATONVM_XT_JIT_COVERAGE_ASSUME"), off_key: None, off_word: None, since: "2026-08-31" },
    E { group: Group::GC, token: "xt-jit-coverage-handshake", on_key: Some("CRATONVM_XT_JIT_COVERAGE_HANDSHAKE"), off_key: None, off_word: Some("0"), since: "2026-08-23" },
    // Declared 2026-08-23 with the OSR coverage-question correction.
    // Default-ON, so a KILL SWITCH: `=0` makes the OSR fallback read
    // `fully_oop_covered` (the frame-slot subset) again instead of
    // `fully_shadow_covered`, which is what refused relocation on 725 of 759
    // collections. See
    // `vm/src/jit/conservative_roots.rs::osr_coverage_uses_shadow_aggregate`.
    E { group: Group::GC, token: "osr-coverage-shadow", on_key: Some("CRATONVM_OSR_COVERAGE_SHADOW"), off_key: None, off_word: Some("0"), since: "2026-08-23" },
    // Declared 2026-08-19 with the ZGC read-bounds publish. An OPT-OUT, not an
    // opt-in: ZGC publishing its arena envelope into `JIT_READ_BOUNDS` is the
    // default, and this key is the kill switch that restores helper-only
    // reference reads. See `gc/src/zgc.rs::zgc_jit_read_bounds_enabled`.
    E { group: Group::GC, token: "zgc-jit-read-bounds", on_key: None, off_key: Some("CRATONVM_ZGC_NO_JIT_READ_BOUNDS"), off_word: None, since: "2026-08-19" },
    // Default-ON, same presence-means-revert shape as the bounds row above:
    // `helpers::zgc_jit_load_barrier_suppressed` treats the key's PRESENCE as
    // "stop panicking on a colored word that reached a JIT read helper; count
    // it and degrade it to null instead". Degrading re-arms the silent heap
    // corruption the tripwire exists to catch, so this is a barrier bring-up
    // lever -- it buys a census from a run that would otherwise die on the
    // first colored word -- and never a production setting.
    E { group: Group::GC, token: "zgc-jit-load-barrier", on_key: None, off_key: Some("CRATONVM_ZGC_NO_JIT_LOAD_BARRIER"), off_word: None, since: "2026-09-01" },
    // Declared 2026-08-16 with genuine concurrent marking. `conc-start` is the
    // percentage of the collection threshold at which a CONCURRENT mark cycle
    // opens, and its parser reads `0` as "never". It is DEFAULT-OFF (the
    // default IS 0), so unlike the four rows below it the `off_word` is not
    // load-bearing -- it is here because the token would otherwise have no way
    // to be spelled negatively at all, and `CRATONVM_GC=-zgc-conc-start`
    // should stay meaningful if the default ever flips. `conc-workers` is a
    // count with no meaningful off state: a concurrent cycle with zero workers
    // would open and never trace, so `0` is clamped up to 1 and the token
    // carries no `off_word`.
    E { group: Group::GC, token: "zgc-conc-start", on_key: Some("CRATONVM_ZGC_CONC_START"), off_key: None, off_word: Some("0"), since: "2026-08-16" },
    E { group: Group::GC, token: "zgc-conc-workers", on_key: Some("CRATONVM_ZGC_CONC_WORKERS"), off_key: None, off_word: None, since: "2026-08-16" },
    // Phase G. `zgc-generational` is a boolean with a real `off` word; the other
    // two are numeric tunables with no negative spelling, like `zgc-parmark`.
    E { group: Group::GC, token: "zgc-generational", on_key: Some("CRATONVM_ZGC_GENERATIONAL"), off_key: None, off_word: Some("0"), since: "2026-08-17" },
    E { group: Group::GC, token: "zgc-gen-promotion-age", on_key: Some("CRATONVM_ZGC_GEN_PROMOTION_AGE"), off_key: None, off_word: None, since: "2026-08-17" },
    E { group: Group::GC, token: "zgc-gen-minors-per-major", on_key: Some("CRATONVM_ZGC_GEN_MINORS_PER_MAJOR"), off_key: None, off_word: None, since: "2026-08-17" },
    E { group: Group::GC, token: "zgc-gen-nursery-percent", on_key: Some("CRATONVM_ZGC_GEN_NURSERY_PERCENT"), off_key: None, off_word: Some("0"), since: "2026-08-17" },
    E { group: Group::GC, token: "zgc-alloc-trigger", on_key: Some("CRATONVM_ZGC_ALLOC_TRIGGER"), off_key: None, off_word: Some("0"), since: "2026-09-03" },
    E { group: Group::GC, token: "zgc-pause-target-ms", on_key: Some("CRATONVM_ZGC_PAUSE_TARGET_MS"), off_key: None, off_word: Some("0"), since: "2026-09-03" },
    E { group: Group::GC, token: "zgc-bitmap-bounds", on_key: Some("CRATONVM_ZGC_BITMAP_BOUNDS"), off_key: None, off_word: Some("0"), since: "2026-09-03" },
    // G2e/G2f (2026-08-17, widened to every cycle 2026-08-18). Default-ON kill
    // switches over the sweep's two per-dead-object costs, in the shape `zgc-relocate` established:
    // `0` restores the previous behaviour byte for byte, so the A/B is a re-run
    // and not a rebuild.
    E { group: Group::GC, token: "zgc-sweep-header-zero", on_key: Some("CRATONVM_ZGC_SWEEP_HEADER_ZERO"), off_key: None, off_word: Some("0"), since: "2026-08-17" },
    E { group: Group::GC, token: "zgc-sweep-dead-runs", on_key: Some("CRATONVM_ZGC_SWEEP_DEAD_RUNS"), off_key: None, off_word: Some("0"), since: "2026-08-17" },
    // C5 (2026-08-18). Hand the mark coordinator the heap's own `Arc` instead of
    // a forwarding wrapper -- one fewer indirect call per marked object on the
    // parallel path. Default-on and `0` restores the wrapper, so the A/B is one
    // binary; the whole path is already opt-in behind `zgc-parmark`.
    E { group: Group::GC, token: "zgc-mark-ctx-direct", on_key: Some("CRATONVM_ZGC_MARK_CTX_DIRECT"), off_key: None, off_word: Some("0"), since: "2026-08-17" },
    E { group: Group::GC, token: "zgc-startbits", on_key: Some("CRATONVM_ZGC_STARTBITS"), off_key: None, off_word: Some("0"), since: "2026-08-07" },
    E { group: Group::GC, token: "zgc-tlab", on_key: Some("CRATONVM_ZGC_TLAB"), off_key: None, off_word: Some("0"), since: "2026-08-07" },
    // Declared 2026-08-06 with the DBG/JIT block: a millisecond goal that
    // `adapt_young_trigger_to_pause` reads. Default 200 since 2026-08-11
    // (`gen_heap::DEFAULT_YOUNG_PAUSE_GOAL_MS`), so it is a default-ON knob
    // whose parser reads `0` as false — which is exactly what `off_word` is
    // for. Without it the generated inventory row says `opt-in | off`, which
    // has been untrue since the default flipped, and
    // `CRATONVM_GC=-young-pause-goal-ms` has no way to turn it off.
    E { group: Group::GC, token: "young-pause-goal-ms", on_key: Some("CRATONVM_GC_YOUNG_PAUSE_MS"), off_key: None, off_word: Some("0"), since: "2026-08-06" },
    // Default-ON: the key's PRESENCE makes the trusted `*_validated` header
    // accessors re-validate, which is the pre-"validate once" behaviour.
    E { group: Group::GC, token: "validate-once", on_key: None, off_key: Some("CRATONVM_GC_NO_VALIDATE_ONCE"), off_word: None, since: "2026-08-20" },
    E { group: Group::REAL, token: "bytebuffer-intrinsic", on_key: Some("CRATONVM_BYTEBUFFER_INTRINSIC"), off_key: None, off_word: None, since: "2026-08-10" },
    // `ArrayList$Itr.hasNext`/`next` are registered `SyntheticStub` rather than
    // `Bridge` so the yield predicate can reach them and the REAL JDK cursor
    // runs -- which is what lets the JIT compile it at all, since a registered
    // native pins its method out of tier-up entirely. DEFAULT-ON, hence a KILL
    // SWITCH: `=0` restores `Bridge` and is the one-binary A/B for the 6.8x.
    E { group: Group::REAL, token: "itr-bytecode", on_key: Some("CRATONVM_ITR_BYTECODE"), off_key: None, off_word: Some("0"), since: "2026-08-26" },
    E { group: Group::REAL, token: "agroal", on_key: Some("CRATONVM_REAL_AGROAL"), off_key: Some("CRATONVM_SYNTHETIC_AGROAL"), off_word: None, since: "2026-06-17" },
    // `CRATONVM_REAL` itself is the group variable, so it is not a row here.
    // It already was a comma-separated token list before this refactor —
    // `vm::runtime::env_cache::RealSelector::parse` reads `all`, `jca` and bare
    // internal class names straight out of it — which is where the grouped
    // variable syntax came from. That parse still runs on the raw value.
    //
    // `off_word` is unset on the four twin rows below because `off_key` already
    // expresses "off": the synthetic variable beats the default-ON real one at
    // the single call site that reads both.
    E { group: Group::REAL, token: "annotations", on_key: Some("CRATONVM_REAL_ANNOTATIONS"), off_key: Some("CRATONVM_SYNTHETIC_ANNOTATIONS"), off_word: None, since: "2026-06-11" },
    E { group: Group::REAL, token: "aqs", on_key: Some("CRATONVM_REAL_AQS"), off_key: Some("CRATONVM_SYNTHETIC_AQS"), off_word: None, since: "2026-06-12" },
    E { group: Group::REAL, token: "dsa", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_DSA"), off_word: None, since: "2026-07-13" },
    E { group: Group::REAL, token: "ec", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_EC"), off_word: None, since: "2026-06-04" },
    E { group: Group::REAL, token: "eqe", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_EQE"), off_word: None, since: "2026-06-11" },
    E { group: Group::REAL, token: "filewriter", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_FILEWRITER"), off_word: None, since: "2026-06-19" },
    E { group: Group::REAL, token: "forkjoinpool", on_key: Some("CRATONVM_REAL_FORKJOINPOOL"), off_key: Some("CRATONVM_SYNTHETIC_FORKJOINPOOL"), off_word: None, since: "2026-06-16" },
    E { group: Group::REAL, token: "jca", on_key: Some("CRATONVM_REAL_JCA"), off_key: None, off_word: None, since: "2026-06-02" },
    // Declared 2026-08-11 alongside `mxbean-mapping`, same shape and same
    // reason: `MemoryUsage.toString()` is answered by the real JDK bytecode by
    // default (`jmx::memoryusage_tostring_shim_enabled`) and this restores the
    // shim.
    E { group: Group::REAL, token: "memoryusage-tostring", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_MEMORYUSAGE_TOSTRING"), off_word: None, since: "2026-08-11" },
    E { group: Group::REAL, token: "msc-real-start", on_key: Some("CRATONVM_MSC_REAL_START"), off_key: None, off_word: Some("off"), since: "2026-06-05" },
    // Declared 2026-08-11. The real JDK MXBean type-mapping machinery became
    // the default that day (`jmx_openmbean::real_mxbean_mapping_enabled`);
    // this is its opt-out, and it has no `on_key` for the same reason `raf`
    // and `pqc` do not — the ON side is the default, not a variable.
    E { group: Group::REAL, token: "mxbean-mapping", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_MXBEAN_MAPPING"), off_word: None, since: "2026-08-11" },
    E { group: Group::REAL, token: "net-sockets", on_key: Some("CRATONVM_REAL_NET_SOCKETS"), off_key: Some("CRATONVM_SYNTHETIC_NET_SOCKETS"), off_word: None, since: "2026-06-03" },
    // Declared 2026-08-13. Netty's real `netty_tcnative` library became the
    // default that day (its `JNI_OnLoad` runs and the
    // `io/netty/internal/tcnative` stubs stand down); this is its opt-out, so
    // it has no `on_key` for the same reason `raf` and `pqc` do not.
    E { group: Group::REAL, token: "netty-tcnative", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_NETTY_TCNATIVE"), off_word: None, since: "2026-08-13" },
    E { group: Group::REAL, token: "pqc", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_PQC"), off_word: None, since: "2026-06-10" },
    E { group: Group::REAL, token: "proxy", on_key: Some("CRATONVM_REAL_PROXY"), off_key: None, off_word: Some("0"), since: "2026-06-17" },
    E { group: Group::REAL, token: "proxy-strict", on_key: Some("CRATONVM_REAL_PROXY_STRICT"), off_key: None, off_word: None, since: "2026-06-18" },
    E { group: Group::REAL, token: "proxy-super", on_key: Some("CRATONVM_REAL_PROXY_SUPER"), off_key: None, off_word: Some("0"), since: "2026-06-19" },
    E { group: Group::REAL, token: "quarkus-arc", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_QUARKUS_ARC"), off_word: None, since: "2026-06-21" },
    E { group: Group::REAL, token: "quarkus-start", on_key: Some("CRATONVM_REAL_QUARKUS_START"), off_key: Some("CRATONVM_SYNTHETIC_QUARKUS_START"), off_word: None, since: "2026-06-21" },
    E { group: Group::REAL, token: "raf", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_RAF"), off_word: None, since: "2026-06-17" },
    E { group: Group::REAL, token: "rsa", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_RSA"), off_word: None, since: "2026-06-14" },
    E { group: Group::REAL, token: "stax-factory", on_key: Some("CRATONVM_REAL_STAX_FACTORY"), off_key: None, off_word: Some("0"), since: "2026-06-30" },
    E { group: Group::REAL, token: "stubs", on_key: None, off_key: Some("CRATONVM_NO_STUBS"), off_word: None, since: "2026-06-02" },
    E { group: Group::REAL, token: "use-wildfly-reflect-shim", on_key: Some("CRATONVM_USE_WILDFLY_REFLECT_SHIM"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::REAL, token: "use-wildfly-synth-bytecode", on_key: Some("CRATONVM_USE_WILDFLY_SYNTH_BYTECODE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::REAL, token: "vertx", on_key: Some("CRATONVM_REAL_VERTX"), off_key: Some("CRATONVM_SYNTHETIC_VERTX"), off_word: None, since: "2026-06-21" },
    E { group: Group::LOADER, token: "allow-jsr-ret", on_key: Some("CRATONVM_ALLOW_JSR_RET"), off_key: None, off_word: None, since: "2026-06-20" },
    // Declared 2026-08-06, and RENAMED to be declarable. It was
    // `CRATONVM_JDK_ONLY_ENFORCE_SHADOW`, which `jdk_only_adds_no_environment_variable`
    // forbids in this table for a good reason: `--jdk-only` is a policy chosen
    // per invocation (contract §9), and a second env-var spelling of it would be
    // a way to half-enable strict mode from a parent shell. This knob is not
    // that -- its caller tests `is_jdk_only()` first and it only decides whether
    // §1.4's native-shadows-bytecode rule is ENFORCED or merely counted, which
    // is what its new name says. Undeclared, it was served by a live `getenv`,
    // so `CRATONVM_LOADER=enforce-native-shadow` could not reach it and no test
    // could arrange it.
    E { group: Group::LOADER, token: "enforce-native-shadow", on_key: Some("CRATONVM_ENFORCE_NATIVE_SHADOW"), off_key: None, off_word: None, since: "2026-08-06" },
    // Default ON. `-cf-delegating-yield` keeps the pure-delegation
    // `CompletableFuture` natives in front of the real JDK bytecode, so the
    // yield can be A/B'd on one binary. See
    // `native_override::delegating_native_yields_to_real_bytecode`.
    E { group: Group::LOADER, token: "cf-delegating-yield", on_key: Some("CRATONVM_CF_DELEGATING_YIELD"), off_key: None, off_word: Some("0"), since: "2026-08-22" },
    E { group: Group::LOADER, token: "aware-resolution", on_key: Some("CRATONVM_LOADER_AWARE_RESOLUTION"), off_key: None, off_word: None, since: "2026-06-24" },
    E { group: Group::LOADER, token: "boot-module-registry", on_key: Some("CRATONVM_BOOT_MODULE_REGISTRY"), off_key: None, off_word: Some("off"), since: "2026-06-23" },
    E { group: Group::LOADER, token: "classpath-jar-unnamed-module", on_key: Some("CRATONVM_CLASSPATH_JAR_UNNAMED_MODULE"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    E { group: Group::LOADER, token: "cl-bootstrap-scoped", on_key: Some("CRATONVM_CL_BOOTSTRAP_SCOPED"), off_key: None, off_word: Some("0"), since: "2026-06-23" },
    E { group: Group::LOADER, token: "fwd-resolve-strict", on_key: Some("CRATONVM_FWD_RESOLVE_STRICT"), off_key: None, off_word: None, since: "2026-06-09" },
    E { group: Group::LOADER, token: "jar-mmap", on_key: None, off_key: Some("CRATONVM_DISABLE_JAR_MMAP"), off_word: None, since: "2026-07-24" },
    E { group: Group::LOADER, token: "lenient-clinit", on_key: Some("CRATONVM_LENIENT_CLINIT"), off_key: None, off_word: None, since: "2026-06-10" },
    E { group: Group::LOADER, token: "longrewrite-loose", on_key: Some("CRATONVM_LONGREWRITE_LOOSE"), off_key: None, off_word: None, since: "2026-07-03" },
    // Default-ON: `classloading::loaders::loader_parent_chain_enabled` treats
    // the empty string and `0` as off, everything else as on.
    E { group: Group::LOADER, token: "parent-chain", on_key: Some("CRATONVM_LOADER_PARENT_CHAIN"), off_key: None, off_word: Some("0"), since: "2026-07-30" },
    E { group: Group::LOADER, token: "resolve-cache-cap", on_key: Some("CRATONVM_RESOLVE_CACHE_CAP"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::LOADER, token: "stub-delegation", on_key: Some("CRATONVM_CL_STUB_DELEGATION"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::LOADER, token: "unload", on_key: Some("CRATONVM_LOADER_UNLOAD"), off_key: None, off_word: Some("0"), since: "2026-06-29" },
    E { group: Group::IO, token: "canon-openfile", on_key: Some("CRATONVM_CANON_OPENFILE"), off_key: None, off_word: None, since: "2026-06-30" },
    E { group: Group::IO, token: "http-max-body", on_key: Some("CRATONVM_HTTP_MAX_BODY"), off_key: None, off_word: None, since: "2026-06-17" },
    E { group: Group::IO, token: "netty-queue-bridge", on_key: Some("CRATONVM_NETTY_QUEUE_BRIDGE"), off_key: None, off_word: Some("0"), since: "2026-07-08" },
    E { group: Group::IO, token: "resolve-outbound-host", on_key: Some("CRATONVM_RESOLVE_OUTBOUND_HOST"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::IO, token: "select-max-block-ms", on_key: Some("CRATONVM_SELECT_MAX_BLOCK_MS"), off_key: None, off_word: None, since: "2026-06-18" },
    E { group: Group::IO, token: "selector-connect-probe", on_key: None, off_key: Some("CRATONVM_NO_SELECTOR_CONNECT_PROBE"), off_word: None, since: "2026-06-24" },
    E { group: Group::IO, token: "socket-capture", on_key: Some("CRATONVM_SOCKET_CAPTURE"), off_key: None, off_word: None, since: "2026-06-12" },
    E { group: Group::IO, token: "uri-strict-chars", on_key: Some("CRATONVM_URI_STRICT_CHARS"), off_key: None, off_word: Some("0"), since: "2026-06-24" },
    E { group: Group::IO, token: "zip-max-entry-bytes", on_key: Some("CRATONVM_ZIP_MAX_ENTRY_BYTES"), off_key: None, off_word: None, since: "2026-05-22" },
    // `ThreadMXBean.getLockedSynchronizers` support; DEFAULT-ON, so a kill
    // switch. THREADS rather than a JMX group because the group vocabulary
    // has no JMX and this is a threading capability the bean exposes.
    E { group: Group::THREADS, token: "jmx-owned-synchronizers", on_key: Some("CRATONVM_JMX_OWNED_SYNCHRONIZERS"), off_key: None, off_word: Some("0"), since: "2026-08-27" },
    E { group: Group::THREADS, token: "assert-single-os-thread", on_key: Some("CRATONVM_ASSERT_SINGLE_OS_THREAD"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::THREADS, token: "async-handoff-sleep-floor-ms", on_key: Some("CRATONVM_ASYNC_HANDOFF_SLEEP_FLOOR_MS"), off_key: None, off_word: None, since: "2026-07-04" },
    E { group: Group::THREADS, token: "async-submit-grace-ms", on_key: Some("CRATONVM_ASYNC_SUBMIT_GRACE_MS"), off_key: None, off_word: None, since: "2026-07-04" },
    E { group: Group::THREADS, token: "async-worker-sleep-floor-ms", on_key: Some("CRATONVM_ASYNC_WORKER_SLEEP_FLOOR_MS"), off_key: None, off_word: None, since: "2026-07-04" },
    E { group: Group::THREADS, token: "await-shortcircuit", on_key: None, off_key: Some("CRATONVM_AWAIT_NO_SHORTCIRCUIT"), off_word: None, since: "2026-05-20" },
    E { group: Group::THREADS, token: "default-watchdog", on_key: None, off_key: Some("CRATONVM_DISABLE_DEFAULT_WATCHDOG"), off_word: None, since: "2026-05-20" },
    E { group: Group::THREADS, token: "default-watchdog-sec", on_key: Some("CRATONVM_DEFAULT_WATCHDOG_SEC"), off_key: None, off_word: None, since: "2026-05-20" },
    // W7-23/W7-27: the A/B for the thread-container pair. Registration (adding a
    // thread to its ThreadContainer on start) and de-registration (Thread.exit
    // removing it) are two halves that MUST ship together -- the add alone turns
    // "join() waits for nothing" into "join() waits forever", measured. This
    // selects both halves at once so the pair can be A/B'd in one binary.
    E { group: Group::THREADS, token: "thread-containers", on_key: Some("CRATONVM_THREAD_CONTAINERS"), off_key: None, off_word: Some("0"), since: "2026-08-11" },
    E { group: Group::THREADS, token: "eqe-sync-execute", on_key: Some("CRATONVM_EQE_SYNC_EXECUTE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::THREADS, token: "exec-depth-ceiling", on_key: Some("CRATONVM_EXEC_DEPTH_CEILING"), off_key: None, off_word: None, since: "2026-06-13" },
    // L19 — `ForkJoinTask.fork()` runs the body inline instead of only marking
    // the task queued. `1`/`cc`/`counted` = eager for a `CountedCompleter`
    // receiver only (the family with no intercepted consumer); `all` = eager
    // for every task. Unset or unrecognised keeps today's lazy fork.
    E { group: Group::THREADS, token: "fjp-eager-fork", on_key: Some("CRATONVM_FJP_EAGER_FORK"), off_key: None, off_word: None, since: "2026-08-06" },
    E { group: Group::THREADS, token: "inherit-thread-ccl", on_key: Some("CRATONVM_INHERIT_THREAD_CCL"), off_key: None, off_word: Some("0"), since: "2026-06-23" },
    E { group: Group::THREADS, token: "inherit-tl-workaround", on_key: Some("CRATONVM_INHERIT_TL_WORKAROUND"), off_key: None, off_word: Some("0"), since: "2026-07-04" },
    E { group: Group::THREADS, token: "lock-order-check", on_key: Some("CRATONVM_LOCK_ORDER_CHECK"), off_key: None, off_word: None, since: "2026-06-21" },
    // Tri-state, like `jit/verify-ir`: unset follows the build profile (armed
    // under `debug_assertions`), any other value arms it, and `0`/`false`/`off`
    // stands it down even in a debug build.
    // The bound on joining shutdown hooks. Carries a value in milliseconds;
    // `=0` selects HotSpot's unbounded wait rather than disabling the knob,
    // which is why there is no `off_word`.
    E { group: Group::THREADS, token: "shutdown-hook-timeout-ms", on_key: Some("CRATONVM_SHUTDOWN_HOOK_TIMEOUT_MS"), off_key: None, off_word: None, since: "2026-08-13" },
    E { group: Group::THREADS, token: "stress-thread-states", on_key: Some("CRATONVM_STRESS_THREAD_STATES"), off_key: None, off_word: Some("0"), since: "2026-07-31" },
    // `types::striped_counter` — per-thread stripes instead of one shared
    // counter, the sibling A/B of `jit/activation-global-mutex`. THREADS rather
    // than GC or JIT because the knob is about contention, not about what the
    // counters mean; its three users are in `jit`, `vm` and `gc`. Presence-
    // parsed opt-out, so the token is stated positively and `-striped-counters`
    // is what sets `CRATONVM_STRIPED_COUNTERS_OFF`.
    E { group: Group::THREADS, token: "striped-counters", on_key: None, off_key: Some("CRATONVM_STRIPED_COUNTERS_OFF"), off_word: None, since: "2026-07-31" },
    E { group: Group::THREADS, token: "thread-start-grace-ms", on_key: Some("CRATONVM_THREAD_START_GRACE_MS"), off_key: None, off_word: None, since: "2026-07-04" },
    E { group: Group::THREADS, token: "wait-spurious-ms", on_key: Some("CRATONVM_WAIT_SPURIOUS_MS"), off_key: None, off_word: None, since: "2026-08-21" },
    // The condition half of `Object.wait()` -- `MonitorState::pending_notifies`.
    // Default ON; `0` restores the condvar-only wait that lost a delivered
    // `notifyAll()`, so the fix can be interleaved against itself on ONE binary.
    E { group: Group::THREADS, token: "monitor-pending-notify", on_key: Some("CRATONVM_MONITOR_PENDING_NOTIFY"), off_key: None, off_word: Some("0"), since: "2026-08-23" },
    // Windows only: bound a TIMED `LockSupport.park` with a high-resolution
    // waitable timer instead of the condvar, whose timeout is rounded up to
    // the 15.625 ms system tick. Default ON; `0` restores the condvar wait,
    // so the netty scheduled-task cadence can be A/B'd on ONE binary.
    E { group: Group::THREADS, token: "win-hires-park", on_key: Some("CRATONVM_WIN_HIRES_PARK"), off_key: None, off_word: Some("0"), since: "2026-08-29" },
    E { group: Group::SECURITY, token: "aot-hmac-key", on_key: Some("CRATONVM_AOT_HMAC_KEY"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::SECURITY, token: "jca-lenient-getinstance", on_key: Some("CRATONVM_JCA_LENIENT_GETINSTANCE"), off_key: None, off_word: None, since: "2026-08-14" },
    E { group: Group::SECURITY, token: "block-private-nets", on_key: Some("CRATONVM_BLOCK_PRIVATE_NETS"), off_key: None, off_word: None, since: "2026-06-10" },
    // The per-VM capability model. `capability-mode` carries a word
    // (`permissive` | `audit` | `enforce`) and `capability-grants` a
    // `;`-separated grant list, so both are value tokens:
    // `CRATONVM_SECURITY=capability-mode=audit`. Note that a bare
    // `CRATONVM_SECURITY=all` writes `1` into every token in this group, and
    // `CapabilityMode::parse` reads `1` as `enforce` — `all` is not a safe way
    // to "turn on diagnostics" here.
    E { group: Group::SECURITY, token: "capability-grants", on_key: Some("CRATONVM_CAPABILITY_GRANTS"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::SECURITY, token: "capability-log", on_key: Some("CRATONVM_CAPABILITY_LOG"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::SECURITY, token: "capability-mode", on_key: Some("CRATONVM_CAPABILITY_MODE"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::SECURITY, token: "confine-io", on_key: Some("CRATONVM_CONFINE_IO"), off_key: None, off_word: None, since: "2026-06-01" },
    E { group: Group::SECURITY, token: "harden-manifest-classpath", on_key: Some("CRATONVM_HARDEN_MANIFEST_CLASSPATH"), off_key: None, off_word: None, since: "2026-06-17" },
    // Opt back in to the pre-hardening no-op `SSLEngine`. Stated positively
    // because the legacy name already spells the permissive direction.
    E { group: Group::SECURITY, token: "noncrypto-sslengine", on_key: Some("CRATONVM_ALLOW_NONCRYPTO_SSLENGINE"), off_key: None, off_word: None, since: "2026-07-31" },
    // The JPMS `exports` gate on reflective access, covering all three arms at
    // once (`Field.get`/`set`, `Method.invoke`, `Constructor.newInstance`) —
    // `lang_class::check_reflection_export_access_with_target_id` is the single
    // helper all three ask. The token is stated positively and the ONLY spelling
    // is the opt-out, so the gate is enforced by default and setting the key at
    // all (any value — the consumer tests `.is_ok()`, not the value) disables
    // it. SECURITY rather than DBG: this is access-control policy, the same
    // class as `noncrypto-sslengine` and `untrusted-code` above, and filing it
    // here is what makes `render-inventory.py` label it `behaviour` instead of
    // `diag`.
    E { group: Group::SECURITY, token: "reflect-export-gate", on_key: None, off_key: Some("CRATONVM_REFLECT_NO_EXPORT_GATE"), off_word: None, since: "2026-08-07" },
    E { group: Group::SECURITY, token: "require-policy", on_key: Some("CRATONVM_REQUIRE_POLICY"), off_key: None, off_word: None, since: "2026-06-01" },
    E { group: Group::SECURITY, token: "trust-pem", on_key: Some("CRATONVM_TRUST_PEM"), off_key: None, off_word: None, since: "2026-05-24" },
    // Default-ON kill switch for the raw-OpenSSL client connector on the
    // default `SSLSocket` path (Unix only — `openssl` is a Unix-scoped
    // dependency). `0` reverts that path to `native_tls::TlsConnector`, which
    // captures ONLY the peer's leaf certificate and cannot set the
    // certificate security level. SECURITY rather than IO: what the switch
    // selects is which verifier sees which chain, and at what strength floor.
    E { group: Group::SECURITY, token: "tls-openssl-client", on_key: Some("CRATONVM_TLS_OPENSSL_CLIENT"), off_key: None, off_word: Some("0"), since: "2026-08-17" },
    E { group: Group::SECURITY, token: "untrusted-code", on_key: Some("CRATONVM_UNTRUSTED_CODE"), off_key: None, off_word: None, since: "2026-06-01" },
    // A/B lever, never a supported configuration: restore the pre-2026-08-28
    // LENIENT field resolution, which answered a fieldref whose (name,
    // descriptor) pair is absent anywhere in the hierarchy with a same-named
    // field of another type instead of raising NoSuchFieldError
    // (JVMS 5.4.3.2). COMPAT because the only thing it can buy is letting an
    // application whose classpath has that shape keep running.
    E { group: Group::COMPAT, token: "field-resolution-name-only", on_key: Some("CRATONVM_FIELD_RESOLUTION_NAME_ONLY"), off_key: None, off_word: None, since: "2026-08-27" },
    E { group: Group::COMPAT, token: "map-iterator-failfast", on_key: None, off_key: Some("CRATONVM_NO_MAP_ITERATOR_FAILFAST"), off_word: None, since: "2026-08-22" },
    // The keySet-view rebuild elision, default-ON. `0` restores the
    // unconditional per-read rebuild, which is the A/B a same-binary
    // bisection needs.
    E { group: Group::COMPAT, token: "map-view-cache", on_key: Some("CRATONVM_MAP_VIEW_CACHE"), off_key: None, off_word: Some("0"), since: "2026-08-23" },
    // `ClassLoader.getResource` / `Class.getResource` stopping at the first
    // matching classpath entry, default-ON. `0` restores the whole-list walk
    // that built every matching URL and returned element 0. The two are
    // required to answer identically, so the flag can only change how much of
    // the classpath was touched — which makes it the same-binary A/B for that
    // cost.
    E { group: Group::COMPAT, token: "getresource-first-hit", on_key: Some("CRATONVM_GETRESOURCE_FIRST_HIT"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    // Take the elision decision, then rebuild anyway and compare, panicking
    // on divergence. Turns the soundness claim into something measured
    // rather than argued; expensive, so default-OFF.
    E { group: Group::COMPAT, token: "verify-map-view-cache", on_key: Some("CRATONVM_VERIFY_MAP_VIEW_CACHE"), off_key: None, off_word: None, since: "2026-08-22" },
    E { group: Group::COMPAT, token: "eager-streams", on_key: Some("CRATONVM_EAGER_STREAMS"), off_key: None, off_word: None, since: "2026-06-19" },
    E { group: Group::COMPAT, token: "foreign-attach", on_key: Some("CRATONVM_FOREIGN_ATTACH"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::COMPAT, token: "jboss-boot-log-file", on_key: Some("CRATONVM_JBOSS_BOOT_LOG_FILE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::COMPAT, token: "jboss-brute-force-jars", on_key: Some("CRATONVM_JBOSS_BRUTE_FORCE_JARS"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::COMPAT, token: "jboss-logger-base-emit", on_key: Some("CRATONVM_JBOSS_LOGGER_BASE_EMIT"), off_key: None, off_word: None, since: "2026-06-19" },
    // `org.jboss.logmanager.LogContextInitializer` consultation at JBoss
    // logger-node creation, default-ON. `0` restores the previous behaviour:
    // no provider is ever asked, so every node is born with no initial
    // handlers and no initial level. It is the one place in the logging
    // natives that runs APPLICATION bytecode from inside a logger allocator,
    // and WildFly, Keycloak and Quarkus all reach it — the switch is what
    // makes "is this the initializer?" a same-binary question.
    E { group: Group::COMPAT, token: "jboss-log-context-initializer", on_key: Some("CRATONVM_JBOSS_LOG_CONTEXT_INITIALIZER"), off_key: None, off_word: Some("0"), since: "2026-09-01" },
    // Default-ON, and off for the *exact* untrimmed string `0` only — the
    // `!matches!(…, Ok("0"))` at `logmanager::jboss_logger_level_filter`.
    E { group: Group::COMPAT, token: "jboss-logger-level-filter", on_key: Some("CRATONVM_JBOSS_LOGGER_LEVEL_FILTER"), off_key: None, off_word: Some("0"), since: "2026-07-27" },
    E { group: Group::COMPAT, token: "jboss-mp-root", on_key: Some("CRATONVM_JBOSS_MP_ROOT"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::COMPAT, token: "lazy-streams", on_key: Some("CRATONVM_LAZY_STREAMS"), off_key: None, off_word: None, since: "2026-06-19" },
    E { group: Group::COMPAT, token: "mh-strict-invokeexact", on_key: Some("CRATONVM_MH_STRICT_INVOKEEXACT"), off_key: None, off_word: Some("0"), since: "2026-08-07" },
    E { group: Group::COMPAT, token: "mockito-legacy-selectors", on_key: Some("CRATONVM_MOCKITO_LEGACY_SELECTORS"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::COMPAT, token: "stackwalker-jdk-walk", on_key: Some("CRATONVM_SW_JDK_WALK"), off_key: None, off_word: None, since: "2026-08-24" },
    E { group: Group::COMPAT, token: "jdk-random", on_key: Some("CRATONVM_JDK_RANDOM"), off_key: None, off_word: None, since: "2026-08-30" },
    E { group: Group::COMPAT, token: "jdk-scanner", on_key: Some("CRATONVM_JDK_SCANNER"), off_key: None, off_word: None, since: "2026-09-01" },
    E { group: Group::GC, token: "stream-refresh-each", on_key: Some("CRATONVM_GC_STREAM_REFRESH_EACH"), off_key: None, off_word: None, since: "2026-08-24" },
    E { group: Group::GC, token: "noflag-deposit-skip-jit-scan", on_key: Some("CRATONVM_GC_NOFLAG_DEPOSIT_SKIP_JIT_SCAN"), off_key: None, off_word: None, since: "2026-08-26" },
    E { group: Group::GC, token: "identity-hash-evict", on_key: Some("CRATONVM_IDENTITY_HASH_EVICT"), off_key: None, off_word: Some("0"), since: "2026-08-30" },
    // `array-autobox-latch` — off restores the unconditional `autobox_payload`
    // probe on every non-null reference-ARRAY element read. That probe opens
    // with `is_object_address` and can only answer `Some` when a wrapper
    // exists, which is what the latch records; the FIELD read paths have been
    // latched since the latch was introduced and the array ones were not.
    E { group: Group::GC, token: "array-autobox-latch", on_key: None, off_key: Some("CRATONVM_GC_NO_ARRAY_AUTOBOX_LATCH"), off_word: None, since: "2026-09-05" },
    E { group: Group::COMPAT, token: "strict-swallows", on_key: Some("CRATONVM_STRICT_SWALLOWS"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::COMPAT, token: "tomcat-mapper-natives", on_key: Some("CRATONVM_TOMCAT_MAPPER_NATIVES"), off_key: None, off_word: Some("0"), since: "2026-07-27" },
    // Default-ON, off for the exact untrimmed string `0` only — the
    // `!matches!(…, Ok("0"))` at `vm_exec::vh_strict_reference_return`. Same
    // shape as `mh-strict-invokeexact` above and deliberately a SEPARATE knob:
    // the two rules fire on disjoint method names and share only their funnel,
    // so one going wrong in the field must not force the other off.
    E { group: Group::COMPAT, token: "vh-strict-reference-return", on_key: Some("CRATONVM_VH_STRICT_REFERENCE_RETURN"), off_key: None, off_word: Some("0"), since: "2026-08-07" },
    E { group: Group::COMPAT, token: "vh-null-coordinate-npe", on_key: Some("CRATONVM_VH_NULL_COORDINATE_NPE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::COMPAT, token: "vh-unsupported-mode-uoe", on_key: Some("CRATONVM_VH_UNSUPPORTED_MODE_UOE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::COMPAT, token: "vh-read-only-handle-uoe", on_key: Some("CRATONVM_VH_READ_ONLY_HANDLE_UOE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::TEST, token: "force-win-build", on_key: Some("CRATONVM_FORCE_WIN_BUILD"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::TEST, token: "jdk", on_key: Some("CRATONVM_TEST_JDK"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::TEST, token: "segv", on_key: Some("CRATONVM_TEST_SEGV"), off_key: None, off_word: None, since: "2026-06-01" },
    E { group: Group::TEST, token: "soak-iters", on_key: Some("CRATONVM_SOAK_ITERS"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::TEST, token: "soak-k", on_key: Some("CRATONVM_SOAK_K"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::TEST, token: "soak-method", on_key: Some("CRATONVM_SOAK_METHOD"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::TEST, token: "soak-timeout-secs", on_key: Some("CRATONVM_SOAK_TIMEOUT_SECS"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::TEST, token: "soak-xmx", on_key: Some("CRATONVM_SOAK_XMX"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::TEST, token: "var", on_key: Some("CRATONVM_TEST_VAR"), off_key: None, off_word: None, since: "2026-05-20" },
];

// ───────────────────────────────────────────────────────────────────────────
// Lookup
// ───────────────────────────────────────────────────────────────────────────

/// The entry for `token` in `group`, if any.
pub fn lookup(group: Group, token: &str) -> Option<&'static E> {
    INVENTORY
        .iter()
        .find(|e| e.group == group && e.token == token)
}

/// Every entry belonging to `group`.
pub fn entries(group: Group) -> impl Iterator<Item = &'static E> {
    INVENTORY.iter().filter(move |e| e.group == group)
}

/// A hint for an unrecognised `token` in `group`, if one is obvious.
///
/// The motivating case is real: disabling a default-ON knob is spelled with a
/// LEADING MINUS (`CRATONVM_JIT=-self-cache-inherit`), and writing the
/// English-looking `no-self-cache-inherit` instead produces a token no entry
/// claims. The flag then never applies, and an A/B that used it measures
/// nothing while reading as a clean negative — which is exactly how one
/// hypothesis was wrongly "falsified" in
/// `docs/known-issues/jit/math-floormod-long-int-returns-minus-one-20260805.md`.
///
/// Checked in order of how badly each is likely to mislead:
///
/// 1. `no-x` / `disable-x` / `off-x` where `x` exists — the writer wanted `-x`
///    and got a silent no-op.
/// 2. an exact match in a DIFFERENT group — right token, wrong variable.
/// 3. a unique containment match — an ordinary typo or truncation.
pub fn suggest(group: Group, token: &str) -> Option<String> {
    let t = token.trim().to_ascii_lowercase();

    for prefix in ["no-", "no_", "disable-", "off-", "not-"] {
        if let Some(rest) = t.strip_prefix(prefix) {
            if lookup(group, rest).is_some() {
                return Some(format!(
                    "did you mean `{}=-{}`? a leading minus turns a knob OFF; \
                     `{}` is not a spelling this parser knows",
                    group.var(),
                    rest,
                    prefix
                ));
            }
        }
    }

    // A token can legitimately exist in SEVERAL groups (`licm` is both a
    // `CRATONVM_JIT` knob and a `CRATONVM_DBG` trace). Naming only the first
    // would send the reader to one arbitrary variable and read as authoritative,
    // so list every group that claims it and let them pick.
    let owners: Vec<&'static str> = Group::ALL
        .iter()
        .filter(|&&g| g != group && lookup(g, &t).is_some())
        .map(|g| g.var())
        .collect();
    if !owners.is_empty() {
        return Some(format!(
            "`{t}` is a token of {}, not {}",
            owners.join(" / "),
            group.var()
        ));
    }

    // A unique containment match in either direction catches truncations
    // ("field-site" for "field-site-cache") and extensions alike. An ambiguous
    // match is deliberately NOT guessed at: a wrong hint is worse than none,
    // because it invites a second inert run.
    let mut hits = entries(group).filter(|e| e.token.contains(&t) || t.contains(e.token));
    if let (Some(first), None) = (hits.next(), hits.next()) {
        return Some(format!("did you mean `{}={}`?", group.var(), first.token));
    }
    None
}

/// The group and token that own `legacy`, if the inventory claims it.
///
/// Used to turn "you set `CRATONVM_DBG_LOADER_TRACE`" into "say
/// `CRATONVM_DBG=loader-trace`" in the deprecation notice.
pub fn canonical_spelling(legacy: &str) -> Option<(Group, String)> {
    INVENTORY.iter().find_map(|e| {
        if e.on_key == Some(legacy) {
            Some((e.group, e.token.to_string()))
        } else if e.off_key == Some(legacy) {
            Some((e.group, format!("-{}", e.token)))
        } else {
            None
        }
    })
}

// ───────────────────────────────────────────────────────────────────────────
// Superseded knobs
// ───────────────────────────────────────────────────────────────────────────

/// A token that still works, but that a command-line flag now expresses better.
///
/// This is a **side table**, not a field on [`E`], for two reasons. [`INVENTORY`]
/// is a `#[rustfmt::skip]` block of ~500 one-line rows whose whole value is that
/// `grep` can answer questions about it; adding a sixth field would widen every
/// one of those lines. And `cargo fmt` is banned repository-wide, so a
/// hand-maintained table cannot be re-flowed after the fact. Supersession is
/// also rare — one row today — so paying a field on 500 rows to describe one is
/// the wrong trade.
///
/// Note what this is *not*: a deprecation that changes behaviour. The token
/// keeps expanding to exactly what it always did. The only new thing is a note
/// the launcher can print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Superseded {
    /// The group the token belongs to.
    pub group: Group,
    /// The [`E::token`] this row is about.
    pub token: &'static str,
    /// Which polarity is superseded. `false` means the `-token` spelling is the
    /// superseded one, which is the case for `CRATONVM_REAL=-stubs`.
    pub on: bool,
    /// What to use instead, spelled as the user would type it.
    pub prefer: &'static str,
    /// Why the replacement is not merely a rename. Printed verbatim, so it has
    /// to read as a sentence fragment after "because".
    pub because: &'static str,
}

impl Superseded {
    /// How the superseded knob is spelled: `CRATONVM_REAL=-stubs`.
    pub fn spelling(&self) -> String {
        format!(
            "{}={}{}",
            self.group.var(),
            if self.on { "" } else { "-" },
            self.token
        )
    }

    /// The one-line note the launcher prints, once per run.
    pub fn note(&self) -> String {
        format!(
            "{} still works, but prefer {} because {}",
            self.spelling(),
            self.prefer,
            self.because
        )
    }
}

/// Every superseded knob, exactly once.
///
/// `CRATONVM_REAL=-stubs` (legacy `CRATONVM_NO_STUBS`) is a *native-registry*
/// filter: it drops synthetic-stub natives and nothing else. `--jdk-only` is
/// the same rule plus the two halves the environment token structurally cannot
/// reach — class fabrication and the native-vs-bytecode dispatch decision — and
/// it records structured provenance for each refusal instead of dropping
/// silently. Contract §9 asks for the note; the token itself is untouched.
pub const SUPERSEDED: &[Superseded] = &[Superseded {
    group: Group::REAL,
    token: "stubs",
    on: false,
    prefer: "--jdk-only",
    because: "the environment token only filters the native registry, and cannot express \
              the class-loading or dispatch half of the JDK-only contract",
}];

/// The supersession row for `group`/`token` in the given polarity, if any.
pub fn superseded_by(group: Group, token: &str, on: bool) -> Option<&'static Superseded> {
    SUPERSEDED
        .iter()
        .find(|s| s.group == group && s.token == token && s.on == on)
}

/// The supersession notes the *process* environment has earned, in
/// [`SUPERSEDED`] order.
///
/// Reads the process environment directly, so the launcher can print the notes
/// before it has a resolved source in hand. [`Resolved::superseded`] is the
/// same answer for an arbitrary [`FlagSource`].
pub fn process_env_supersessions() -> Vec<&'static Superseded> {
    supersessions_in(&MapSource::from_process_env())
}

/// Whether `spec` — the raw comma-separated value of a grouped variable — names
/// `token` in the `on` polarity.
///
/// Mirrors [`resolve`]'s tokenizer exactly (leading `-`/`+`, an optional
/// `=value`, case-folded), because a note that fires for `-stubs` but not for
/// ` -STUBS=1 ` would be worse than no note at all.
fn spec_names(spec: &str, token: &str, on: bool) -> bool {
    spec.split(',').any(|raw| {
        let t = raw.trim();
        if t.is_empty() {
            return false;
        }
        let (this_on, t) = match t.strip_prefix('-') {
            Some(rest) => (false, rest),
            None => (true, t.strip_prefix('+').unwrap_or(t)),
        };
        let name = match t.split_once('=') {
            Some((n, _)) => n.trim(),
            None => t,
        };
        this_on == on && name.eq_ignore_ascii_case(token)
    })
}

/// The supersession rows `src` has earned, in [`SUPERSEDED`] order.
///
/// Fires for the grouped spelling *and* for the legacy key set directly — a
/// runbook that exports `CRATONVM_NO_STUBS=1` is exactly the reader the note is
/// written for, and it would never see a note keyed only on the grouped form.
fn supersessions_in(src: &dyn FlagSource) -> Vec<&'static Superseded> {
    SUPERSEDED
        .iter()
        .filter(|s| {
            let grouped = src
                .get(s.group.var())
                .and_then(|v| v.into_string().ok())
                .is_some_and(|spec| spec_names(&spec, s.token, s.on));
            let legacy = lookup(s.group, s.token)
                .and_then(|e| if s.on { e.on_key } else { e.off_key })
                .is_some_and(|key| src.get(key).is_some());
            grouped || legacy
        })
        .collect()
}

// ───────────────────────────────────────────────────────────────────────────
// JDK-only command-line surface
// ───────────────────────────────────────────────────────────────────────────

/// One command-line flag, as the launcher's parser and its `--help` both need
/// it.
///
/// These are *not* environment variables and deliberately add none: the
/// fifteen-variable surface this module exists to hold down is a promise, and
/// `--jdk-only` is a runtime policy chosen per invocation, not a knob a parent
/// shell should be able to set behind an operator's back. The table lives here
/// because this is where the launcher already looks for "what may I be
/// passed", not because these are flag-group members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CliFlag {
    /// The flag as typed, including the leading dashes.
    pub flag: &'static str,
    /// Whether the next argument is its value.
    pub takes_value: bool,
    /// The `--help` line, verbatim from contract §9.
    pub help: &'static str,
}

/// The seven flags of contract §9, in the order §9 lists them.
///
/// Help text is copied verbatim from the contract: it is the user-visible
/// surface the design pinned, and re-wording it here would silently fork the
/// documentation from the binary.
pub const JDK_ONLY_CLI_FLAGS: &[CliFlag] = &[
    CliFlag {
        flag: "--jdk-only",
        takes_value: false,
        help: "Real JDK, reject compatibility stubs and fabricated classes.",
    },
    CliFlag {
        flag: "--real-jdk",
        takes_value: false,
        help: "Real JDK with current compatibility behaviour (default).",
    },
    CliFlag {
        flag: "--synthetic-jdk",
        takes_value: false,
        help: "Standalone synthetic library; conflicts with both.",
    },
    CliFlag {
        flag: "--jdk-only-report",
        takes_value: true,
        help: "Write the JSON violation/counter report.",
    },
    CliFlag {
        flag: "--dump-class-origins",
        takes_value: true,
        help: "Write the class-origin census.",
    },
    CliFlag {
        flag: "--trace-jdk-only",
        takes_value: false,
        help: "Log every violation as it happens.",
    },
    CliFlag {
        flag: "--explain-jdk-only",
        takes_value: false,
        help: "Print the long-form explanation for each violation.",
    },
];

/// Flags that `--jdk-only` cannot be combined with.
///
/// `--real-jdk` is *not* here: `--jdk-only` implies `JdkMode::Real`, so the two
/// agree about the image and differ only about substitutions. `--synthetic-jdk`
/// selects a class library that is nothing but substitutions, which is the one
/// combination that cannot mean anything (contract §1, §6).
pub const JDK_ONLY_CONFLICTS_WITH: &[&str] = &["--synthetic-jdk"];

/// The [`CliFlag`] named exactly `flag`, if it is one of §9's.
///
/// Exact match only. A caller handling the `--flag=value` spelling splits on
/// the first `=` before asking.
pub fn jdk_only_flag(flag: &str) -> Option<&'static CliFlag> {
    JDK_ONLY_CLI_FLAGS.iter().find(|f| f.flag == flag)
}

// ───────────────────────────────────────────────────────────────────────────
// Resolution
// ───────────────────────────────────────────────────────────────────────────

/// A [`FlagSource`] with the grouped variables expanded over it.
///
/// Lookups fall through to the wrapped source, so a key that no group owns —
/// including one added since this table was generated — still resolves.
pub struct Resolved<'a> {
    raw: &'a dyn FlagSource,
    /// `Some` = set to this value, `None` = masked as unset.
    overrides: BTreeMap<&'static str, Option<OsString>>,
    /// Legacy variables found set directly in `raw` that a group now owns,
    /// paired with the grouped spelling to use instead. Sorted, deduplicated.
    pub legacy_direct: Vec<(&'static str, String)>,
    /// Tokens named in a grouped variable that no entry claims. A typo here is
    /// the failure mode the old surface hid, so callers should print these.
    pub unknown_tokens: Vec<String>,
    /// Knobs this source set that a command-line flag now expresses better, in
    /// [`SUPERSEDED`] order. Advisory only — the tokens keep working, and
    /// nothing here changes what [`Self::overrides`] says.
    pub superseded: Vec<&'static Superseded>,
}

impl FlagSource for Resolved<'_> {
    fn get(&self, name: &str) -> Option<OsString> {
        match self.overrides.get(name) {
            Some(v) => v.clone(),
            None => self.raw.get(name),
        }
    }
}

impl Resolved<'_> {
    /// The expansions this resolution wants applied, as
    /// `(key, Some(value) | None)` where `None` means "unset it".
    pub fn overrides(&self) -> impl Iterator<Item = (&'static str, Option<&OsString>)> {
        self.overrides.iter().map(|(k, v)| (*k, v.as_ref()))
    }

    /// Whether any grouped variable actually said something.
    pub fn is_empty(&self) -> bool {
        self.overrides.is_empty()
    }
}

/// Apply `entry` in the given direction to `overrides`.
fn apply(overrides: &mut BTreeMap<&'static str, Option<OsString>>, e: &E, on: bool, value: &str) {
    if on {
        // Enabling means: set the positive key if there is one, and clear the
        // opt-out key so a stale `NO_X` in the environment cannot win.
        if let Some(k) = e.on_key {
            overrides.insert(k, Some(OsString::from(value)));
        }
        if let Some(k) = e.off_key {
            overrides.insert(k, None);
        }
    } else if let Some(k) = e.off_key {
        // A knob with a dedicated opt-out spelling: set it, clear the opt-in.
        overrides.insert(k, Some(OsString::from("1")));
        if let Some(on_key) = e.on_key {
            overrides.insert(on_key, None);
        }
    } else if let Some(k) = e.on_key {
        match e.off_word {
            // Default-ON knob: unsetting it would leave it on, so write the
            // value its own parser reads as false.
            Some(w) => overrides.insert(k, Some(OsString::from(w))),
            None => overrides.insert(k, None),
        };
    }
}

/// Expand the grouped variables in `src` into the per-knob keys the typed
/// configuration reads.
pub fn resolve<'a>(src: &'a dyn FlagSource) -> Resolved<'a> {
    let mut overrides: BTreeMap<&'static str, Option<OsString>> = BTreeMap::new();
    let mut unknown_tokens = Vec::new();

    for &group in Group::ALL {
        let Some(spec) = src.get(group.var()).and_then(|v| v.into_string().ok()) else {
            continue;
        };
        for raw_token in spec.split(',') {
            let t = raw_token.trim();
            if t.is_empty() {
                continue;
            }
            let (on, t) = match t.strip_prefix('-') {
                Some(rest) => (false, rest),
                None => (true, t.strip_prefix('+').unwrap_or(t)),
            };
            let (name, value) = match t.split_once('=') {
                Some((n, v)) => (n.trim(), v),
                None => (t, "1"),
            };
            let name_lc = name.to_ascii_lowercase();

            if name_lc == "all" {
                for e in entries(group) {
                    apply(&mut overrides, e, on, "1");
                }
                continue;
            }
            match lookup(group, &name_lc) {
                Some(e) => apply(&mut overrides, e, on, value),
                // `CRATONVM_REAL` also accepts bare internal-form class names
                // (`java/util/stream/Collectors`), parsed by
                // `vm::runtime::env_cache::RealSelector`. An unrecognised token
                // there is a class name, not a typo.
                None if group == Group::REAL => {}
                None => unknown_tokens.push(format!("{}={}", group.var(), raw_token.trim())),
            }
        }
    }

    // Report legacy names still being set directly. A name the grouped
    // variable already overrode is not reported — the user has migrated it.
    let mut legacy_direct: Vec<(&'static str, String)> = INVENTORY
        .iter()
        .flat_map(|e| [e.on_key, e.off_key].into_iter().flatten())
        .filter(|k| !overrides.contains_key(k) && src.get(k).is_some())
        .filter_map(|k| canonical_spelling(k).map(|(g, tok)| (k, format!("{}={}", g.var(), tok))))
        .collect();
    legacy_direct.sort_unstable();
    legacy_direct.dedup();

    Resolved {
        raw: src,
        overrides,
        legacy_direct,
        unknown_tokens,
        superseded: supersessions_in(src),
    }
}

/// Expand the grouped variables into the process environment.
///
/// **Transitional.** 431 read sites still call `std::env::var` directly rather
/// than reading a [`crate::flags::VmFlags`] field; those cannot see a resolved
/// source, so the expansion is written back to `environ` for them. Call it from
/// the launcher before any thread is started — it is the same single-threaded
/// startup window the seventeen pre-existing `set_var` calls in this tree
/// already rely on. Every site migrated to `flags()` makes this call less
/// necessary; when the count reaches zero it can be deleted outright.
///
/// Returns the diagnostics from [`resolve`] so the launcher can print them.
pub fn expand_process_env() -> (Vec<(&'static str, String)>, Vec<String>) {
    let raw = MapSource::from_process_env();
    let resolved = resolve(&raw);
    for (key, value) in resolved.overrides.iter() {
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
    (resolved.legacy_direct, resolved.unknown_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `resolve` borrows its source, so every test needs the source to outlive
    /// the resolution. This owns both.
    struct Case {
        src: MapSource,
    }

    impl Case {
        fn new(pairs: &[(&str, &str)]) -> Self {
            Self {
                src: MapSource::new(pairs.iter().copied()),
            }
        }
        fn resolve(&self) -> Resolved<'_> {
            resolve(&self.src)
        }
    }

    fn case(pairs: &[(&str, &str)]) -> Case {
        Case::new(pairs)
    }

    /// The two names declared on 2026-08-11 reach their consumers through the
    /// GROUPED spelling, which is the whole point of declaring them.
    ///
    /// Both were read by code and named nowhere, so each was served by a live
    /// `getenv` rather than the latched snapshot: `CRATONVM_DBG=overlay-gate`
    /// and `CRATONVM_REAL=-memoryusage-tostring` reached neither, and
    /// `flags::with_thread_overrides` could not arrange either in a test. A row
    /// in `INVENTORY` is what fixes that, so assert the expansion rather than
    /// the row's existence — `every_token_is_unique` and the surface guards
    /// already cover the row.
    ///
    /// `memoryusage-tostring` is an opt-OUT: the real JDK bytecode is the
    /// default, so the token has no `on_key` and turning it OFF is what sets
    /// the `CRATONVM_SYNTHETIC_*` key. Same shape as `mxbean-mapping` and
    /// `aqs` above.
    #[test]
    fn the_20260811_declarations_expand_from_their_group_spelling() {
        let c = case(&[("CRATONVM_DBG", "overlay-gate")]);
        assert_eq!(
            c.resolve().get("CRATONVM_DBG_OVERLAY_GATE"),
            Some(OsString::from("1")),
        );

        // Opt-out: `-token` sets the SYNTHETIC key...
        let c = case(&[("CRATONVM_REAL", "-memoryusage-tostring")]);
        assert_eq!(
            c.resolve().get("CRATONVM_SYNTHETIC_MEMORYUSAGE_TOSTRING"),
            Some(OsString::from("1")),
        );
        // ...and asking for the default explicitly clears it, without minting a
        // `CRATONVM_REAL_*` twin. That twin's absence is deliberately NOT
        // asserted by name: a whole-string `CRATONVM_*` literal anywhere in
        // Rust source is exactly what `flag_declaration_guard` scans for, so
        // naming a variable in order to say it does not exist would demand a
        // declaration for it. One override says the same thing.
        let c = case(&[("CRATONVM_REAL", "memoryusage-tostring")]);
        let on = c.resolve();
        assert_eq!(on.get("CRATONVM_SYNTHETIC_MEMORYUSAGE_TOSTRING"), None);
        assert_eq!(
            on.overrides().count(),
            1,
            "the ON form clears the opt-out key and touches nothing else",
        );
    }

    /// Same for the two GC diagnostics declared on 2026-08-17.
    ///
    /// `CRATONVM_DBG_MAPGEN` (`vm/src/threading/gc_barrier.rs`) and
    /// `CRATONVM_DBG_VACATED_FRAMES` (`gc/src/gc_quiescence.rs`) shipped as
    /// `runtime_var_os` reads with no row, so each was served by a live
    /// `getenv`. Both are plain opt-in diagnostics, so the assertion is just
    /// that the grouped spelling reaches the legacy key — which is the fact a
    /// consumer depends on and the row's existence alone does not establish.
    ///
    /// The wider cost of leaving them undeclared was not the two flags: while
    /// `flag_declaration_guard` was red it could not catch the NEXT one.
    #[test]
    fn the_20260817_gc_diagnostics_expand_from_their_group_spelling() {
        let c = case(&[("CRATONVM_DBG", "mapgen")]);
        assert_eq!(
            c.resolve().get("CRATONVM_DBG_MAPGEN"),
            Some(OsString::from("1")),
        );

        let c = case(&[("CRATONVM_DBG", "vacated-frames")]);
        assert_eq!(
            c.resolve().get("CRATONVM_DBG_VACATED_FRAMES"),
            Some(OsString::from("1")),
        );

        // And together, comma-separated, since that is how two diagnostics for
        // one investigation actually get set.
        let c = case(&[("CRATONVM_DBG", "mapgen,vacated-frames")]);
        let both = c.resolve();
        assert_eq!(both.get("CRATONVM_DBG_MAPGEN"), Some(OsString::from("1")));
        assert_eq!(
            both.get("CRATONVM_DBG_VACATED_FRAMES"),
            Some(OsString::from("1")),
        );
    }

    #[test]
    fn every_token_is_unique() {
        // Uniqueness is on the PAIR. The same token in two different groups is
        // deliberate and has its own test
        // (`a_token_shared_between_groups_stays_two_keys`), so a bare token
        // count would condemn 13 legitimate rows.
        let mut seen: Vec<(Group, &str)> = INVENTORY.iter().map(|e| (e.group, e.token)).collect();
        seen.sort_unstable();
        let dups: Vec<String> = seen
            .windows(2)
            .filter(|w| w[0] == w[1])
            .map(|w| format!("{:?}/{}", w[0].0, w[0].1))
            .collect();
        assert!(
            dups.is_empty(),
            "these (group, token) pairs are claimed twice: {}. Merge each into \
             ONE entry carrying both an on_key and an off_key, or delete the \
             duplicate — two sessions declaring the same knob independently is \
             how this happens, and the row is usually verbatim-identical.",
            dups.join(", ")
        );
    }

    #[test]
    fn every_legacy_key_is_claimed_once() {
        let mut keys: Vec<&str> = INVENTORY
            .iter()
            .flat_map(|e| [e.on_key, e.off_key].into_iter().flatten())
            .collect();
        keys.sort_unstable();
        let dups: Vec<&str> = keys
            .windows(2)
            .filter(|w| w[0] == w[1])
            .map(|w| w[0])
            .collect();
        assert!(
            dups.is_empty(),
            "these legacy variables are claimed by more than one token, so \
             their canonical spelling is ambiguous: {dups:?}"
        );
        for s in SCALARS {
            assert!(
                !keys.contains(s),
                "{s} is both a scalar and a grouped token"
            );
        }
        // A legacy key may not be spelled the same as a group variable: the
        // resolver would then both consume it as a token list and expand a
        // token into it. `CRATONVM_DBG` and `CRATONVM_REAL` were exactly this,
        // and both are handled at their entries above.
        for g in Group::ALL {
            assert!(
                !keys.contains(&g.var()),
                "{} is a group variable and also a legacy key",
                g.var()
            );
        }
    }

    #[test]
    fn every_entry_can_be_switched_both_ways() {
        for e in INVENTORY {
            assert!(
                e.on_key.is_some() || e.off_key.is_some(),
                "{}/{} expands to nothing",
                e.group.var(),
                e.token
            );
            if e.off_word.is_some() {
                assert!(
                    e.on_key.is_some() && e.off_key.is_none(),
                    "off_word only applies to a default-ON knob with no opt-out key"
                );
            }
        }
    }

    #[test]
    fn token_names_are_canonical() {
        for e in INVENTORY {
            assert!(
                !e.token.is_empty()
                    && e.token
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "token {:?} is not lower-kebab-case",
                e.token
            );
            assert!(
                !e.token.starts_with("no-") && !e.token.starts_with("disable-"),
                "token {:?} states a negative; invert it and put the legacy \
                 name in off_key, so switching it off is spelled with a leading -",
                e.token
            );
        }
    }

    #[test]
    fn grouped_token_sets_the_legacy_key() {
        let c = case(&[("CRATONVM_DBG", "loader-trace,gc-stress=65536")]);
        let r = c.resolve();
        assert_eq!(
            r.get("CRATONVM_DBG_LOADER_TRACE"),
            Some(OsString::from("1"))
        );
        assert_eq!(
            r.get("CRATONVM_DBG_GC_STRESS"),
            Some(OsString::from("65536"))
        );
        assert!(r.unknown_tokens.is_empty());
    }

    #[test]
    fn negated_token_sets_the_opt_out_key() {
        // `bce` only ever had an opt-out spelling. `-bce` must set it...
        let c = case(&[("CRATONVM_JIT", "-bce")]);
        assert_eq!(
            c.resolve().get("CRATONVM_JIT_NO_BCE"),
            Some(OsString::from("1"))
        );
        // ...and `bce` must clear it, even when the shell exported it.
        let c = case(&[("CRATONVM_JIT", "bce"), ("CRATONVM_JIT_NO_BCE", "1")]);
        assert_eq!(c.resolve().get("CRATONVM_JIT_NO_BCE"), None);
    }

    #[test]
    fn merged_twins_are_one_token() {
        let c = case(&[("CRATONVM_GC", "moving-young")]);
        let on = c.resolve();
        assert_eq!(on.get("CRATONVM_MOVING_YOUNG"), Some(OsString::from("1")));
        assert_eq!(on.get("CRATONVM_NO_MOVING_YOUNG"), None);

        let c = case(&[("CRATONVM_GC", "-moving-young")]);
        let off = c.resolve();
        assert_eq!(off.get("CRATONVM_MOVING_YOUNG"), None);
        assert_eq!(
            off.get("CRATONVM_NO_MOVING_YOUNG"),
            Some(OsString::from("1"))
        );

        // CRATONVM_REAL_AQS / CRATONVM_SYNTHETIC_AQS were one switch, twice.
        let c = case(&[("CRATONVM_REAL", "-aqs")]);
        let synth = c.resolve();
        assert_eq!(
            synth.get("CRATONVM_SYNTHETIC_AQS"),
            Some(OsString::from("1"))
        );
        assert_eq!(synth.get("CRATONVM_REAL_AQS"), None);
    }

    #[test]
    fn default_on_knob_is_switched_off_by_value_not_by_unsetting() {
        // CRATONVM_OLD_SWEEP_JIT is parsed `on_unless_zero`: removing it leaves
        // the feature ON, so `-old-sweep-jit` has to write "0".
        let c = case(&[("CRATONVM_JIT", "-old-sweep-jit")]);
        assert_eq!(
            c.resolve().get("CRATONVM_OLD_SWEEP_JIT"),
            Some(OsString::from("0"))
        );
    }

    /// **EVERY default-ON knob's off token must actually write the off word.**
    ///
    /// `default_on_knob_is_switched_off_by_value_not_by_unsetting` above pins
    /// this for one row, which is one row's worth of protection: a NEW row
    /// written in the wrong shape is not covered by it. Three were --
    /// `xt-helper-window-discharge`, `xt-pinned-peer-depth` and
    /// `xt-peer-shadow-scan` all named their own variable in `off_key` as well
    /// as `on_key`, so `apply` took the "dedicated opt-out spelling" branch,
    /// wrote `VAR=1`, then cleared `on_key` -- the same key -- and the knob came
    /// out UNSET. Their parsers read unset as ON, so `CRATONVM_JIT=-<token>`
    /// silently left three default-ON GC-root features running. Kill switches
    /// that do not kill, which is worse than none: an arm that reads as
    /// "the feature is not the cause" while the feature is still on.
    ///
    /// The structural gates caught the shape, and their message -- "off_word
    /// only applies to a default-ON knob with no opt-out key" -- describes the
    /// table, not the damage. This asserts the CONSEQUENCE, over every row that
    /// has an `off_word` now or later, so the next one is reported as what it
    /// costs rather than as a schema violation.
    #[test]
    fn every_default_on_knob_resolves_its_off_token_to_the_off_word() {
        for e in INVENTORY {
            let Some(word) = e.off_word else { continue };
            let key = e
                .on_key
                .expect("checked by every_entry_can_be_switched_both_ways");
            let c = case(&[(e.group.var(), &format!("-{}", e.token))]);
            assert_eq!(
                c.resolve().get(key),
                Some(OsString::from(word)),
                "{}=-{} must write {key}={word}. It resolved to {:?} instead, \
                 which this knob's parser reads as ON -- the off token is inert.",
                e.group.var(),
                e.token,
                c.resolve().get(key),
            );
        }
    }

    /// The three rows that were wrong, by name, because a table-driven test
    /// keeps passing if a row is deleted. Each is a default-ON cross-thread
    /// root-scan feature whose kill switch is the first thing an investigation
    /// reaches for.
    #[test]
    fn the_xt_root_scan_kill_switches_actually_switch_off() {
        for (group, token, key) in [
            (
                "CRATONVM_JIT",
                "xt-helper-window-discharge",
                "CRATONVM_XT_HELPER_WINDOW_DISCHARGE",
            ),
            (
                "CRATONVM_JIT",
                "xt-pinned-peer-depth",
                "CRATONVM_XT_PINNED_PEER_DEPTH",
            ),
            (
                "CRATONVM_JIT",
                "xt-peer-shadow-scan",
                "CRATONVM_XT_PEER_SHADOW_SCAN",
            ),
        ] {
            let off = case(&[(group, &format!("-{token}"))]);
            assert_eq!(
                off.resolve().get(key),
                Some(OsString::from("0")),
                "{group}=-{token} must write {key}=0; the readers in \
                 vm/src/jit/xt_root_scan.rs and conservative_roots.rs treat \
                 anything else -- UNSET included -- as on"
            );
            let on = case(&[(group, token)]);
            assert_eq!(
                on.resolve().get(key),
                Some(OsString::from("1")),
                "{group}={token} must still write {key}=1"
            );
        }
    }

    #[test]
    fn the_documented_no_ops_now_work() {
        // Each of these was documented for months and read by nothing: the
        // default was flipped and only the opt-out half was renamed.
        // flag-census.md section 3 has the full list.
        let cases: &[(&str, &str, &str)] = &[
            (
                "CRATONVM_JIT",
                "-precise-jit-maps",
                "CRATONVM_NO_PRECISE_JIT_MAPS",
            ),
            (
                "CRATONVM_JIT",
                "-inline-putfield",
                "CRATONVM_NO_JIT_INLINE_PUTFIELD",
            ),
            ("CRATONVM_JIT", "-scan-cache", "CRATONVM_NO_JIT_SCAN_CACHE"),
            (
                "CRATONVM_JIT",
                "-precise-inline-frame-record",
                "CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD",
            ),
            (
                "CRATONVM_GC",
                "-selective-promote",
                "CRATONVM_NO_SELECTIVE_PROMOTE",
            ),
        ];
        for (var, token, expected_key) in cases {
            let c = case(&[(var, token)]);
            assert_eq!(
                c.resolve().get(expected_key),
                Some(OsString::from("1")),
                "{var}={token} did not reach {expected_key}"
            );
        }
    }

    #[test]
    fn all_enables_every_token_in_the_group() {
        let c = case(&[("CRATONVM_DBG", "all")]);
        let r = c.resolve();
        for e in entries(Group::DBG) {
            if let Some(k) = e.on_key {
                assert_eq!(
                    r.get(k),
                    Some(OsString::from("1")),
                    "{k} not enabled by all"
                );
            }
        }
        assert_eq!(r.get("CRATONVM_JIT_LICM"), None);
    }

    #[test]
    fn unknown_tokens_are_reported_not_swallowed() {
        let c = case(&[("CRATONVM_JIT", "licm,definitely-not-a-flag")]);
        let r = c.resolve();
        assert_eq!(r.unknown_tokens, vec!["CRATONVM_JIT=definitely-not-a-flag"]);
        // The valid neighbour still applied.
        assert_eq!(r.get("CRATONVM_JIT_LICM"), Some(OsString::from("1")));
    }

    /// The exact mistake that produced a confident, wrong "hypothesis
    /// falsified" (see the `math-floormod-long-int-returns-minus-one` doc):
    /// `no-X` looks like English but is not a spelling this parser knows, so
    /// the knob silently never applies.
    #[test]
    fn no_prefix_is_unknown_and_suggests_the_minus_form() {
        let c = case(&[("CRATONVM_JIT", "no-self-cache-inherit")]);
        let r = c.resolve();
        assert_eq!(
            r.unknown_tokens,
            vec!["CRATONVM_JIT=no-self-cache-inherit"],
            "the `no-` form must NOT be silently treated as the disable spelling"
        );
        let hint = suggest(Group::JIT, "no-self-cache-inherit")
            .expect("a `no-X` token whose X exists must be hinted");
        assert!(
            hint.contains("-self-cache-inherit"),
            "hint should name the leading-minus form, got: {hint}"
        );

        // And the real spelling does apply, so the hint points somewhere true.
        let ok = case(&[("CRATONVM_JIT", "-self-cache-inherit")]);
        let ok = ok.resolve();
        assert!(ok.unknown_tokens.is_empty());
        assert_eq!(
            ok.get("CRATONVM_JIT_NO_SELF_CACHE_INHERIT"),
            Some(OsString::from("1"))
        );
    }

    #[test]
    fn suggest_names_every_group_that_claims_a_misfiled_token() {
        // `licm` is claimed by BOTH CRATONVM_JIT and CRATONVM_DBG. Naming only
        // one would point the reader at an arbitrary variable and read as
        // authoritative, so the hint must list them all.
        let hint = suggest(Group::GC, "licm").expect("cross-group hit must be hinted");
        assert!(hint.contains("CRATONVM_JIT"), "got: {hint}");
        assert!(hint.contains("CRATONVM_DBG"), "got: {hint}");
        assert!(hint.contains("not CRATONVM_GC"), "got: {hint}");
    }

    #[test]
    fn suggest_declines_when_the_match_is_ambiguous() {
        // A wrong hint invites a second inert run, so an ambiguous token gets
        // no guess. "s" is a substring of many tokens.
        assert_eq!(suggest(Group::JIT, "s"), None);
        // And a token resembling nothing at all gets none either.
        assert_eq!(suggest(Group::JIT, "zzzzzzzz-not-close-to-anything"), None);
    }

    #[test]
    fn legacy_names_still_work_and_are_reported() {
        let c = case(&[("CRATONVM_DBG_LOADER_TRACE", "1")]);
        let r = c.resolve();
        // Untouched: the legacy name *is* the key the typed config reads.
        assert_eq!(
            r.get("CRATONVM_DBG_LOADER_TRACE"),
            Some(OsString::from("1"))
        );
        assert_eq!(
            r.legacy_direct,
            vec![(
                "CRATONVM_DBG_LOADER_TRACE",
                "CRATONVM_DBG=loader-trace".to_string()
            )]
        );
    }

    #[test]
    fn a_grouped_variable_beats_a_legacy_one() {
        // The whole point of `-token`: switching off something a parent shell
        // exported. If legacy won, that would be inexpressible.
        let c = case(&[
            ("CRATONVM_DBG_HEAP_STALE", "1"),
            ("CRATONVM_DBG", "-heap-stale"),
        ]);
        let r = c.resolve();
        assert_eq!(r.get("CRATONVM_DBG_HEAP_STALE"), None);
        assert!(r.legacy_direct.is_empty(), "already migrated, do not nag");
    }

    #[test]
    fn unclaimed_names_fall_through() {
        let c = case(&[("CRATONVM_SOMETHING_BRAND_NEW", "7")]);
        assert_eq!(
            c.resolve().get("CRATONVM_SOMETHING_BRAND_NEW"),
            Some(OsString::from("7"))
        );
    }

    // ── Supersession (JDK-only mode, contract §9) ──────────────────────────

    /// The load-bearing constraint of the whole supersession change: the token
    /// keeps doing **exactly** what it did. `--jdk-only` is a superset, but the
    /// people already exporting `CRATONVM_REAL=-stubs` in runbooks get no
    /// behaviour change — only a note. If this row ever expanded to a second
    /// key, every one of those runbooks would quietly start doing something new.
    #[test]
    fn stubs_still_expands_to_exactly_one_key() {
        let c = case(&[("CRATONVM_REAL", "-stubs")]);
        let r = c.resolve();
        let one = OsString::from("1");
        let applied: Vec<(&str, Option<&OsString>)> = r.overrides().collect();
        assert_eq!(
            applied,
            vec![("CRATONVM_NO_STUBS", Some(&one))],
            "-stubs must set CRATONVM_NO_STUBS=1 and nothing else"
        );
    }

    /// This feature is a *command-line* policy. If it ever grows an environment
    /// variable, the fifteen-name promise is broken and
    /// `types/tests/flag-surface.txt` needs a matching edit — neither of which
    /// should happen by accident, so fail here first with a message that says
    /// what to do about it.
    #[test]
    fn jdk_only_adds_no_environment_variable() {
        let names = INVENTORY
            .iter()
            .flat_map(|e| [e.on_key, e.off_key].into_iter().flatten())
            .chain(SCALARS.iter().copied())
            .chain(Group::ALL.iter().map(|g| g.var()));
        for name in names {
            assert!(
                !name.contains("JDK_ONLY"),
                "{name} looks like a JDK-only environment variable. The feature \
                 is a runtime policy chosen per invocation (contract §9): add a \
                 CliFlag to JDK_ONLY_CLI_FLAGS instead. If a variable really is \
                 wanted, types/tests/flag-surface.txt has to gain it in the \
                 same commit."
            );
        }
        // Supersession is a note about an existing knob, never a new one.
        for s in SUPERSEDED {
            assert!(
                lookup(s.group, s.token).is_some(),
                "{} supersedes a token that is not in the inventory",
                s.spelling()
            );
        }
        // And the CLI table is flags, not variables.
        for f in JDK_ONLY_CLI_FLAGS {
            assert!(f.flag.starts_with("--"), "{} is not a flag", f.flag);
            assert!(!f.flag.contains("CRATONVM"), "{} is not a flag", f.flag);
        }
    }

    #[test]
    fn superseded_lookup_is_polarity_sensitive() {
        let row = superseded_by(Group::REAL, "stubs", false)
            .expect("CRATONVM_REAL=-stubs is the one superseded knob");
        assert_eq!(row.prefer, "--jdk-only");
        assert_eq!(row.spelling(), "CRATONVM_REAL=-stubs");
        assert!(row.note().contains("CRATONVM_REAL=-stubs"));
        assert!(row.note().contains("--jdk-only"));
        assert!(row.note().contains(row.because));

        // `stubs` (positive) asks for the default behaviour and is not
        // superseded by anything; neither is a token in another group.
        assert!(superseded_by(Group::REAL, "stubs", true).is_none());
        assert!(superseded_by(Group::JIT, "stubs", false).is_none());
        assert!(superseded_by(Group::REAL, "aqs", false).is_none());
    }

    #[test]
    fn resolution_reports_the_superseded_spelling_both_ways() {
        // Grouped spelling.
        let c = case(&[("CRATONVM_REAL", "aqs,-stubs")]);
        let r = c.resolve();
        assert_eq!(r.superseded.len(), 1);
        assert_eq!(r.superseded[0].prefer, "--jdk-only");
        // ...and the expansion is untouched by the note.
        assert_eq!(r.get("CRATONVM_NO_STUBS"), Some(OsString::from("1")));

        // Legacy key exported directly — the runbook case the note exists for.
        let c = case(&[("CRATONVM_NO_STUBS", "1")]);
        assert_eq!(c.resolve().superseded.len(), 1);

        // Tokenizer parity with `resolve`: spacing, `+`, `=value` and case.
        for spec in ["-stubs", " -stubs ", "-STUBS=1", "licm,-stubs"] {
            assert!(
                spec_names(spec, "stubs", false),
                "{spec:?} names -stubs but was not recognised"
            );
        }
        assert!(!spec_names("stubs", "stubs", false));
        assert!(!spec_names("+stubs", "stubs", false));
        assert!(!spec_names("-stubsx", "stubs", false));

        // Nothing set: no note. Nagging on a clean environment is how a
        // deprecation line gets filtered out before the one that matters.
        assert!(case(&[]).resolve().superseded.is_empty());
        assert!(case(&[("CRATONVM_REAL", "aqs")])
            .resolve()
            .superseded
            .is_empty());
    }

    // ── The §9 command-line surface ────────────────────────────────────────

    #[test]
    fn the_jdk_only_cli_surface_is_the_seven_flags_of_the_contract() {
        let flags: Vec<&str> = JDK_ONLY_CLI_FLAGS.iter().map(|f| f.flag).collect();
        assert_eq!(
            flags,
            vec![
                "--jdk-only",
                "--real-jdk",
                "--synthetic-jdk",
                "--jdk-only-report",
                "--dump-class-origins",
                "--trace-jdk-only",
                "--explain-jdk-only",
            ]
        );

        let mut unique = flags.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), flags.len(), "a flag is listed twice");

        for f in JDK_ONLY_CLI_FLAGS {
            assert!(!f.help.is_empty(), "{} has no --help line", f.flag);
            assert!(
                f.help.ends_with('.'),
                "{} help must read as a sentence",
                f.flag
            );
            assert_eq!(
                jdk_only_flag(f.flag),
                Some(f),
                "{} is not findable by its own name",
                f.flag
            );
        }

        // Only the two that name a file take one; a value-taking flag parsed as
        // a boolean silently swallows the next argument.
        let with_value: Vec<&str> = JDK_ONLY_CLI_FLAGS
            .iter()
            .filter(|f| f.takes_value)
            .map(|f| f.flag)
            .collect();
        assert_eq!(
            with_value,
            vec!["--jdk-only-report", "--dump-class-origins"]
        );

        assert!(jdk_only_flag("--jdk-onlyy").is_none());
        assert!(
            jdk_only_flag("--jdk-only-report=x").is_none(),
            "exact match"
        );
        assert!(jdk_only_flag("").is_none());
    }

    #[test]
    fn jdk_only_conflicts_only_with_the_synthetic_library() {
        assert_eq!(JDK_ONLY_CONFLICTS_WITH.to_vec(), vec!["--synthetic-jdk"]);
        for name in JDK_ONLY_CONFLICTS_WITH {
            assert!(
                jdk_only_flag(name).is_some(),
                "{name} is not a flag this table knows about"
            );
        }
        // `--jdk-only` implies JdkMode::Real, so it agrees with `--real-jdk`
        // about the image and differs only about substitutions.
        assert!(!JDK_ONLY_CONFLICTS_WITH.contains(&"--real-jdk"));
        assert!(!JDK_ONLY_CONFLICTS_WITH.contains(&"--jdk-only"));
    }

    // ── The 2026-08-01 declaration audit ───────────────────────────────────
    //
    // Four JIT capabilities landed in this wave with their consumers committed
    // and their declarations impossible, because this file was owned by another
    // lane. The tests below pin what was verified at each consumer, not merely
    // that a row exists — a row with the wrong polarity is worse than no row,
    // since it reads as coverage while doing the opposite of what it says.

    /// The four default-OFF JIT capability rows are switched off by **removing**
    /// their key, never by writing a value into it.
    ///
    /// Two distinct reasons converge on the same requirement, and the assertion
    /// messages keep them apart because only one of them is a correctness bug:
    ///
    /// * `range-bce` and `activation-global-mutex` are **presence-parsed**
    ///   (`runtime_var_os(..).is_some()`). Writing `"0"` leaves `is_some()`
    ///   answering `true`, so an `off_word` here makes `CRATONVM_JIT=-token`
    ///   turn the feature **on**. For `range-bce` that is not cosmetic: the gate
    ///   admits a new reason for *deleting* a bounds check, and a wrong elision
    ///   is an out-of-bounds heap write — which would then be reachable from the
    ///   documented surface by the very spelling that means "off".
    /// * `vectorize` does read `"0"` as false, so either
    ///   spelling would work. `off_word` stays `None` because it is the field a
    ///   reader uses to tell a default-ON knob from a default-OFF one; see
    ///   `every_entry_can_be_switched_both_ways`.
    #[test]
    fn default_off_capabilities_are_switched_off_by_unsetting_them() {
        // (token, the consumer tests presence only)
        let rows = [
            ("range-bce", true),
            ("activation-global-mutex", true),
            ("vectorize", false),
        ];
        for (token, presence_parsed) in rows {
            let e = lookup(Group::JIT, token)
                .unwrap_or_else(|| panic!("CRATONVM_JIT={token} is not declared"));
            let key = e
                .on_key
                .unwrap_or_else(|| panic!("{token} has no positive key"));
            assert!(e.off_key.is_none(), "{token} grew an opt-out spelling");
            if presence_parsed {
                assert!(
                    e.off_word.is_none(),
                    "{token} carries off_word {:?}, but its consumer only tests \
                     the key's *presence* — that value would switch the knob ON",
                    e.off_word
                );
            } else {
                assert!(
                    e.off_word.is_none(),
                    "{token} is default-OFF; off_word is how a reader spots a \
                     default-ON knob, so it must stay None even though {:?} \
                     would behave the same",
                    e.off_word
                );
            }

            // Off, even against a stale export from a parent shell.
            let spec = format!("-{token}");
            let c = case(&[("CRATONVM_JIT", spec.as_str()), (key, "1")]);
            assert_eq!(
                c.resolve().get(key),
                None,
                "CRATONVM_JIT=-{token} must clear {key}, not give it a value"
            );

            // On.
            let c = case(&[("CRATONVM_JIT", token)]);
            assert_eq!(c.resolve().get(key), Some(OsString::from("1")));
        }
    }

    /// The whole bounds-check-elimination family, in one place.
    ///
    /// `range-bce` arrived alongside three existing switches and the question
    /// asked was whether *any* of them were declared. Three already were; a gap
    /// in this family is how a bisection ends up unable to express the state it
    /// needs, so the set is asserted rather than assumed.
    #[test]
    fn the_bounds_check_family_is_declared_in_full() {
        let family = [
            ("bce", "CRATONVM_JIT_NO_BCE", false),
            ("spec-bce", "CRATONVM_JIT_NO_SPEC_BCE", false),
            ("inclusive-bce", "CRATONVM_JIT_INCLUSIVE_BCE", true),
            ("range-bce", "CRATONVM_JIT_RANGE_BCE", true),
        ];
        for (token, key, opt_in) in family {
            let e = lookup(Group::JIT, token).unwrap_or_else(|| panic!("{token} is undeclared"));
            if opt_in {
                assert_eq!(e.on_key, Some(key), "{token} should enable via {key}");
            } else {
                assert_eq!(e.off_key, Some(key), "{token} should disable via {key}");
            }
        }
        // `-bce` still kills every reason including the new one, which is what
        // lets `range-bce` stay a narrow opt-in without losing the wide switch.
        let c = case(&[("CRATONVM_JIT", "-bce,range-bce")]);
        let r = c.resolve();
        assert_eq!(r.get("CRATONVM_JIT_NO_BCE"), Some(OsString::from("1")));
        assert_eq!(r.get("CRATONVM_JIT_RANGE_BCE"), Some(OsString::from("1")));
    }

    /// Two knobs that were asked for and are deliberately **not** here.
    ///
    /// `CRATONVM_TIER_BROKER`: `jit/src/tiered.rs` reads no such key — the
    /// broker is additive and still unwired, and the row exists only as a
    /// proposal in `docs/jit/compilation-broker.md`.
    /// `CRATONVM_JIT_BYTECODE_UNROLL`: the loop rewriter is armed by a
    /// thread-local (`x64::set_bytecode_loop_rewriter_armed`) and reads no
    /// environment at all.
    ///
    /// A declared flag with no read site is worse than an absent one: it reads
    /// as coverage, `CRATONVM_JIT=<token>` reports no unknown token, and
    /// `docs/CONFIG.md` gains a knob that does nothing. That is exactly the
    /// defect `the_documented_no_ops_now_work` above exists to have fixed once.
    ///
    /// The two names are spelled with `concat!` on purpose. Written whole they
    /// would be exact `"CRATONVM_…"` literals in a scanned source file, and
    /// both `types/tests/flag_declaration_guard.rs` and
    /// `tools/flag-census/check-surface.sh` would report them as undeclared
    /// read sites — which is the correct behaviour for a *read* site and the
    /// wrong answer for an assertion that the name does not exist. Splitting
    /// before the underscore is enough: both scanners key on `"CRATONVM_`.
    #[test]
    fn knobs_without_a_consumer_are_not_declared() {
        for key in [
            concat!("CRATONVM", "_TIER_BROKER"),
            concat!("CRATONVM", "_JIT_BYTECODE_UNROLL"),
        ] {
            assert!(
                canonical_spelling(key).is_none(),
                "{key} is declared, but nothing reads it. Delete the row, or \
                 land the consumer in the same commit."
            );
        }
        for token in ["compilation-broker", "bytecode-unroll"] {
            assert!(lookup(Group::JIT, token).is_none(), "{token} is declared");
        }
        // And the tokens that *do* exist are reported as unknown, so a runbook
        // exporting one is told rather than silently ignored.
        let c = case(&[("CRATONVM_JIT", "compilation-broker")]);
        assert_eq!(
            c.resolve().unknown_tokens,
            vec!["CRATONVM_JIT=compilation-broker"]
        );
    }

    /// The two ZGC rows are **kill switches**, and `-token` has to reach the
    /// value their own parsers read as false.
    ///
    /// Both `zgc_tlab_enabled_by_default` and `zgc_start_bits_enabled_by_default`
    /// return `true` for an unset key, so the `off_word: None` these rows
    /// carried until 2026-08-13 made `CRATONVM_GC=-zgc-tlab` expand to *unset*
    /// — i.e. to the ON state. That is the failure mode this test exists for,
    /// and it is invisible from the outside: the spelling is accepted, no
    /// unknown-token diagnostic fires, and the feature stays on. It also fed a
    /// wrong `opt-in | off` row into the generated `flag-inventory.md`.
    ///
    /// The exact edit that trips it: put `off_word: None` back on either row.
    #[test]
    fn the_zgc_kill_switches_expand_to_the_word_their_parsers_read_as_false() {
        for (token, key) in [
            ("zgc-tlab", "CRATONVM_ZGC_TLAB"),
            ("zgc-startbits", "CRATONVM_ZGC_STARTBITS"),
            // Both joined the default-ON set on 2026-08-13 and hit the same
            // trap on the way in: declared as opt-ins, so `-token` expanded to
            // unsetting a key whose unset meaning had just become ON.
            ("zgc-parmark", "CRATONVM_ZGC_PARMARK"),
            ("zgc-relocate", "CRATONVM_ZGC_RELOCATE"),
            // Joined 2026-08-21, default-ON for the same reason: without a
            // falsey word its `-token` form would unset the key and leave
            // the machinery on, which is not a kill switch.
            (
                "zgc-relocate-proven-jit",
                "CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT",
            ),
        ] {
            let e = lookup(Group::GC, token).unwrap_or_else(|| panic!("{token} is undeclared"));
            assert_eq!(e.on_key, Some(key));
            assert!(e.off_key.is_none(), "{token} gained an opt-out key");
            assert_eq!(
                e.off_word,
                Some("0"),
                "{token} gates default-ON machinery; without a falsey word                  `CRATONVM_GC=-{token}` unsets the key and leaves it ON"
            );

            // Off: the grouped spelling must WRITE the falsey word, not clear
            // the key — and it must win over a stale `=1` from a parent shell.
            let spec = format!("-{token}");
            let c = case(&[("CRATONVM_GC", spec.as_str()), (key, "1")]);
            assert_eq!(
                c.resolve().get(key),
                Some(OsString::from("0")),
                "CRATONVM_GC=-{token} must set {key}=0"
            );

            // On: the bare token still reaches the key affirmatively.
            let c = case(&[("CRATONVM_GC", token)]);
            assert_eq!(c.resolve().get(key), Some(OsString::from("1")));
        }
    }

    /// The rest of the audit: knobs that were read by a live `getenv` because
    /// no row named them. Polarity is asserted per row, since these were
    /// classified by reading each consumer.
    #[test]
    fn the_previously_undeclared_knobs_expand_the_way_their_consumers_parse() {
        // Default-ON, killed only by an exact `"0"`.
        for (group, token, key) in [
            (Group::JIT, "sp-inline-ic", "CRATONVM_JIT_SP_INLINE_IC"),
            (Group::JIT, "sp-tailcall", "CRATONVM_JIT_SP_TAILCALL"),
            (
                Group::JIT,
                "osr-dead-locals",
                "CRATONVM_JIT_OSR_DEAD_LOCALS",
            ),
        ] {
            let e = lookup(group, token).unwrap_or_else(|| panic!("{token} is undeclared"));
            assert_eq!(e.on_key, Some(key));
            assert_eq!(e.off_word, Some("0"), "{token} needs a falsey word");
            let spec = format!("-{token}");
            let c = case(&[(group.var(), spec.as_str())]);
            assert_eq!(c.resolve().get(key), Some(OsString::from("0")));
        }

        // Default-ON with a dedicated opt-out name: `-token` sets it.
        for (group, token, off_key) in [
            (
                Group::JIT,
                "shadow-end-guard",
                "CRATONVM_SHADOW_NO_END_GUARD",
            ),
            (Group::JIT, "statics-index", "CRATONVM_NO_STATICS_INDEX"),
            (
                Group::JIT,
                "mic-rust-entry-cache",
                "CRATONVM_JIT_NO_MIC_RUST_ENTRY_CACHE",
            ),
            (
                Group::JIT,
                "precise-virtual-invokes",
                "CRATONVM_JIT_NO_PRECISE_VIRTUAL_INVOKES",
            ),
            (
                Group::GC,
                "exact-refproc-survival",
                "CRATONVM_NO_EXACT_REFPROC_SURVIVAL",
            ),
            (Group::GC, "oldgen-coalesce", "CRATONVM_NO_OLDGEN_COALESCE"),
            (
                Group::THREADS,
                "striped-counters",
                "CRATONVM_STRIPED_COUNTERS_OFF",
            ),
        ] {
            let e = lookup(group, token).unwrap_or_else(|| panic!("{token} is undeclared"));
            assert_eq!(e.off_key, Some(off_key));
            assert!(e.on_key.is_none(), "{token} gained an unread positive key");
            let spec = format!("-{token}");
            let c = case(&[(group.var(), spec.as_str())]);
            assert_eq!(c.resolve().get(off_key), Some(OsString::from("1")));
            // ...and the positive token clears a stale export.
            let c = case(&[(group.var(), token), (off_key, "1")]);
            assert_eq!(c.resolve().get(off_key), None);
        }
    }

    /// The two OSR diagnosis levers declared 2026-08-04 resolve BOTH ways, and
    /// are default-OFF.
    ///
    /// `flag_declaration_guard` only asks whether a name appears in
    /// [`INVENTORY`]; a row can be listed and still not resolve, and from there
    /// the two look identical. So this drives the grouped spelling and checks
    /// the legacy key it must set — and checks the polarity, because these two
    /// sit four rows from `osr-dead-locals`, which is the opposite: default-ON
    /// with `"0"` as its kill switch.
    ///
    /// The exact edit that trips it: give either row an `off_word`. The
    /// `off_word.is_none()` assertion fails, and so does the last block —
    /// `-osr-single-pc` would start expanding to `=0` instead of doing nothing,
    /// and for a consumer that accepts only `1`/`on`/`true`/`yes` that is an
    /// opt-out spelling nobody wrote.
    #[test]
    fn the_osr_diagnosis_levers_resolve_and_are_default_off() {
        for (token, key) in [
            ("osr-seed-frame-slots", "CRATONVM_JIT_OSR_SEED_FRAME_SLOTS"),
            ("osr-single-pc", "CRATONVM_JIT_OSR_SINGLE_PC"),
        ] {
            let e = lookup(Group::JIT, token).unwrap_or_else(|| panic!("{token} is undeclared"));
            assert_eq!(e.on_key, Some(key));
            assert!(e.off_key.is_none(), "{token} gained an opt-out key");
            assert!(
                e.off_word.is_none(),
                "{token} is default-OFF; an `off_word` would label it a kill switch"
            );

            // The positive token reaches the key `jit::osr_always_seed_frame_slot`
            // and `jit::osr_single_pc_entry_only` actually read.
            let c = case(&[("CRATONVM_JIT", token)]);
            assert_eq!(
                c.resolve().get(key),
                Some(OsString::from("1")),
                "CRATONVM_JIT={token} must set {key}"
            );

            // And the negative spelling exports nothing, rather than a word
            // those consumers never agreed to read.
            let spec = format!("-{token}");
            let c = case(&[("CRATONVM_JIT", spec.as_str())]);
            assert_eq!(
                c.resolve().get(key),
                None,
                "-{token} must not export a value; these are off by absence"
            );
        }
    }

    /// `ir-linear-scan` is one token name in two groups — the capability in
    /// `JIT`, its trace in `DBG` — like `ir-long` and `xt-jit-root-scan` before
    /// it. They must stay two distinct keys, or `CRATONVM_DBG=all` would arm a
    /// register allocator.
    #[test]
    fn a_token_shared_between_groups_stays_two_keys() {
        for token in ["ir-linear-scan", "ir-long", "xt-jit-root-scan"] {
            let jit = lookup(Group::JIT, token).unwrap_or_else(|| panic!("JIT/{token}"));
            let dbg = lookup(Group::DBG, token).unwrap_or_else(|| panic!("DBG/{token}"));
            assert_ne!(jit.on_key, dbg.on_key, "{token} collapsed into one key");
        }
        let c = case(&[("CRATONVM_DBG", "all")]);
        let r = c.resolve();
        assert_eq!(
            r.get("CRATONVM_DBG_IR_LINEAR_SCAN"),
            Some(OsString::from("1"))
        );
        assert_eq!(r.get("CRATONVM_JIT_IR_LINEAR_SCAN"), None);
    }

    /// The six GC/native-collections knobs brought inside the boundary on
    /// 2026-08-04 are reachable BOTH ways, which is the whole point of
    /// declaring them.
    ///
    /// `flag_declaration_guard` only asks whether a name appears in
    /// [`INVENTORY`]; it cannot tell a token that resolves from one that was
    /// merely listed. Until this landed, all six were served by a live
    /// `getenv`, so `CRATONVM_GC=…` could not reach them and
    /// `flags::with_thread_overrides` could not arrange them in a test — which
    /// is exactly how a flag-dependent test ends up measuring the developer's
    /// ambient environment instead of what it claims to check.
    #[test]
    fn the_gc_diagnostic_knobs_resolve_from_their_grouped_token() {
        // (group variable, token, the legacy key it must set)
        let on: &[(&str, &str, &str)] = &[
            (
                "CRATONVM_DBG",
                "sweep-referrers",
                "CRATONVM_DBG_SWEEP_REFERRERS",
            ),
            ("CRATONVM_DBG", "owner-filter", "CRATONVM_DBG_OWNER_FILTER"),
            ("CRATONVM_GC", "oldgen-compact", "CRATONVM_OLDGEN_COMPACT"),
            (
                "CRATONVM_GC",
                "owner-class-filter",
                "CRATONVM_OWNER_CLASS_FILTER",
            ),
        ];
        for &(var, token, key) in on {
            let c = case(&[(var, token)]);
            assert_eq!(
                c.resolve().get(key),
                Some(OsString::from("1")),
                "{var}={token} must set {key}"
            );
        }

        // `old-interior-pins` is a default-ON capability whose only spelling
        // was ever the opt-out, so the token is stated positively and it is
        // `-old-interior-pins` that sets the `NO_` key. Both directions are
        // asserted: a token that silently did nothing would otherwise look
        // exactly like one that worked.
        let off = case(&[("CRATONVM_GC", "-old-interior-pins")]);
        assert_eq!(
            off.resolve().get("CRATONVM_GC_NO_OLD_INTERIOR_PINS"),
            Some(OsString::from("1")),
            "-old-interior-pins must set the NO_ key"
        );
        let on = case(&[("CRATONVM_GC", "old-interior-pins")]);
        assert_eq!(
            on.resolve().get("CRATONVM_GC_NO_OLD_INTERIOR_PINS"),
            None,
            "the positive token must leave the pins on, i.e. the NO_ key unset"
        );
    }

    #[test]
    fn the_whole_surface_is_eighteen_variables() {
        // This is the number the refactor exists to hold down. Raising it wants
        // an argument, not a merge.
        //
        // 15 -> 17 -> 18 on 2026-09-01, and every argument was made at the
        // entry itself, in `SCALARS`, by the change that added it:
        // `CRATONVM_JFR_ENABLE_EVENTS` (B10), the `native.encoding` override
        // (D3), and `CRATONVM_STDOUT_ENCODING`.
        //
        // The argument for the third, since this is where it is owed: it is the
        // opt-out for deriving `stdout.encoding` / `stderr.encoding` /
        // `stdin.encoding` from the host instead of pinning UTF-8, which
        // `docs/known-issues/stdout-encoding-differs-from-hotspot-on-windows-
        // 20260901.md` §9 asked for by name when it recommended following the
        // host — that key decides the `Charset` stamped on `System.out`, so the
        // change moves BYTES and not only a property string, and a change that
        // moves bytes on every non-UTF-8 host needs a same-binary way back.
        // A scalar rather than an `INVENTORY` token for the same reason as the
        // two above: it carries a VALUE, and the `E` model is presence-only.
        //
        // Two GROUPS were not added — `Group::ALL` is still ten. Adding one of
        // those is the move that would want a fresh argument.
        assert_eq!(Group::ALL.len() + SCALARS.len(), 18);
    }
    /// The repository's first commit. No knob can predate it, so a `since:`
    /// earlier than this is a typo rather than a very old flag.
    const REPOSITORY_EPOCH: &str = "2026-04-26";

    /// A `DBG` knob declared on or after this date must have a live consumer.
    ///
    /// # Why the rule is stated forwards rather than backwards
    ///
    /// The obvious retirement rule — "a knob older than N months must justify
    /// itself" — is red on arrival here: **43** `DBG` tokens declared before
    /// 2026-08-01 are named nowhere outside `types/`, the two generated
    /// documents and `docs/internal` (the historical bug-write-up archive,
    /// which is stripped from published history, so a mention there is not a
    /// consumer anybody can reach). Landing a test that fails on 43 rows nobody
    /// in this branch is allowed to delete produces a red gate everyone learns
    /// to skip, which is worse than no gate at all.
    ///
    /// So the horizon runs the other way: rows declared **before** it are
    /// grandfathered and enumerated in
    /// `docs/config/flag-retirement-candidates-20260901.md`; rows declared **on
    /// or after** it have to earn their place. That is the half that can be
    /// green today and still bite, because every new flag is on the biting side
    /// of the line from the day it is added.
    ///
    /// # Why this exact date
    ///
    /// It is measured, not chosen for roundness. The newest consumer-less `DBG`
    /// row in the tree is `dupclass-filter`, dated 2026-07-27; 2026-08-01 is the
    /// first month boundary above it. That leaves **126** `DBG` rows inside the
    /// rule today, all of them passing, and every row added from now on. Moving
    /// the horizon *earlier* is the retirement work itself: each step back has
    /// to be paid for by deleting or documenting the rows it newly covers, and
    /// this test names exactly which those are.
    const DBG_CONSUMER_HORIZON: &str = "2026-08-01";

    /// Directories never searched for a consumer: build output, VCS metadata,
    /// and the Java application harnesses under `apps/`.
    ///
    /// `types/` and the internal write-up archive are excluded too, but by
    /// name at the walk's top level rather than here — see
    /// [`names_with_a_possible_consumer`].
    const NOT_A_CONSUMER: &[&str] = &["target", ".git", "apps", "node_modules"];

    /// File kinds that can carry a consumer: source, runbooks, CI, fixtures.
    const CONSUMER_EXTENSIONS: &[&str] = &[
        "rs", "md", "sh", "py", "toml", "yml", "yaml", "txt", "ps1", "json", "java",
    ];

    fn workspace_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("types/ always has a workspace root above it")
            .to_path_buf()
    }

    /// Every `CRATONVM_*` name mentioned anywhere a *consumer* could live, and
    /// the number of files actually read.
    ///
    /// Two directories are cut out of the walk. `types/` is the declaration
    /// itself, and a row citing its own registry is not evidence of anything.
    /// The `internal` subtree of `docs` is the historical write-up archive; it
    /// is stripped from published history, so a name whose only mention is
    /// there has no reader outside this working copy. The two generated
    /// documents are cut for the same reason as `types/`: they are rendered
    /// *from* this table, so every row is in them by construction. The
    /// retirement-candidate list is cut for a third reason: it names a knob in
    /// order to propose DELETING it, which is the opposite of evidence that
    /// something still wants it. (It is also self-referential — it was written
    /// by this same measurement, and counting it moved the candidate set from
    /// 63 to 0 the first time it was left in.)
    ///
    /// A mention, not a read site: the policy accepts a runbook, a CI step or a
    /// known-issue page as evidence that a knob is still wanted, which is why
    /// this scans Markdown and shell as well as Rust. It deliberately does not
    /// require the quoting precision of
    /// `flag_declaration_guard::exact_literals` — prose does not quote.
    fn names_with_a_possible_consumer() -> (std::collections::BTreeSet<String>, usize) {
        let root = workspace_root();
        let derived = [
            "docs/config/flag-inventory.md",
            "docs/flag-tokens.md",
            "docs/config/flag-retirement-candidates-20260901.md",
        ];
        let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut files = 0usize;
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if path.is_dir() {
                    if name.starts_with('.') || NOT_A_CONSUMER.contains(&name) {
                        continue;
                    }
                    if dir == root && name == "types" {
                        continue;
                    }
                    if name == "internal" && dir == root.join("docs") {
                        continue;
                    }
                    stack.push(path);
                    continue;
                }
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if !CONSUMER_EXTENSIONS.contains(&ext) {
                    continue;
                }
                let relative = path.strip_prefix(&root).unwrap_or(path.as_path());
                let shown = relative.display().to_string().replace('\\', "/");
                if derived.contains(&shown.as_str()) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                files += 1;
                if !text.contains("CRATONVM_") {
                    continue;
                }
                let mut rest = text.as_str();
                while let Some(at) = rest.find("CRATONVM_") {
                    let tail = &rest[at..];
                    let end = tail
                        .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                        .unwrap_or(tail.len());
                    names.insert(tail[..end].to_string());
                    rest = &rest[at + "CRATONVM_".len()..];
                }
            }
        }
        (names, files)
    }

    /// Every row says when its knob arrived, in a form that can be compared.
    ///
    /// The date is what makes retirement possible at all, so a malformed one is
    /// worse than a wrong one: `since: "aug 2026"` sorts below every real date
    /// and would quietly drop its row out of every horizon check for good.
    #[test]
    fn every_row_states_when_it_arrived() {
        for e in INVENTORY {
            let s = e.since;
            let well_formed = s.len() == 10
                && s.as_bytes().iter().enumerate().all(|(i, c)| {
                    if i == 4 || i == 7 {
                        *c == b'-'
                    } else {
                        c.is_ascii_digit()
                    }
                });
            assert!(
                well_formed,
                "{}={} has since: {s:?}, which is not an ISO YYYY-MM-DD date. \
                 The field is compared as a string, so anything else sorts \
                 silently outside every horizon instead of failing.",
                e.group.var(),
                e.token
            );
            assert!(
                s >= REPOSITORY_EPOCH,
                "{}={} claims since: {s:?}, which is before the repository's \
                 first commit ({REPOSITORY_EPOCH}) — no knob can be older than \
                 the tree it lives in",
                e.group.var(),
                e.token
            );
        }
    }

    /// A `DBG` knob declared on or after `DBG_CONSUMER_HORIZON` is still wanted
    /// by something outside the registry.
    ///
    /// This is the retirement policy, and it is the first thing in this
    /// repository that can make a flag's *absence of use* fail a build. The
    /// census that motivated the grouping recorded that flags arrive "at roughly
    /// one per fixed bug with no retirement path"; grouping renamed them and
    /// removed none. A knob whose only mentions are the four registry files is a
    /// knob nobody can be running, and the whole cost of the surface is made of
    /// those.
    ///
    /// `DBG` first because its contract — no token here changes a program's
    /// result — makes an unused row pure cost, and because it is the group that
    /// grew fastest: 510 of the 970 tokens.
    #[test]
    fn a_dbg_knob_declared_since_the_horizon_has_a_live_consumer() {
        let (names, files) = names_with_a_possible_consumer();
        // A walk that stops reaching the tree passes this test on every row. It
        // is the same failure `flag_declaration_guard::scan` guards against, and
        // it is the only way this test can be wrong in the direction nobody
        // notices.
        assert!(
            files > 500 && names.len() > 400,
            "the consumer scan read {files} files and found {} names under {} — \
             it is not reaching the workspace, so this test was about to approve \
             every row",
            names.len(),
            workspace_root().display()
        );

        let mut covered = 0usize;
        let mut orphans: Vec<String> = Vec::new();
        for e in INVENTORY {
            if e.group != Group::DBG || e.since < DBG_CONSUMER_HORIZON {
                continue;
            }
            covered += 1;
            let cited = [e.on_key, e.off_key]
                .into_iter()
                .flatten()
                .any(|k| names.contains(k));
            if !cited {
                orphans.push(format!("{} (since {})", e.token, e.since));
            }
        }

        // If the horizon ever stops selecting anything, the rule has become a
        // no-op and would go on passing while the surface grows underneath it.
        assert!(
            covered > 50,
            "only {covered} DBG rows are on or after {DBG_CONSUMER_HORIZON}; the \
             horizon has drifted past the table and this rule now checks nothing"
        );

        assert!(
            orphans.is_empty(),
            "these DBG knobs were declared on or after {DBG_CONSUMER_HORIZON} \
             and are named nowhere outside `types/`, `docs/internal` and the two \
             generated flag documents — nothing reads them, no runbook sets them, \
             no known-issue page cites them:\n  {}\n\
             Either land the consumer, or delete the row and its four-file \
             registration. Adding the name to `docs/config/flag-inventory.md` or \
             `docs/flag-tokens.md` does not count: those are generated FROM this \
             table.",
            orphans.join("\n  ")
        );
    }
}
