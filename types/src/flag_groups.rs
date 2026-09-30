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
    // Round 11 wave 19 (lane lock): a bounded in-running monitor park, in
    // microseconds (capped at 100000). Unset = off. A scalar because it carries
    // a value.
    "CRATONVM_MONITOR_LAZY_PARK_US",
    // Round 12 wave 1 (lane lock): cap on concurrent contended spinners per monitor (A/B knob). Unset = no cap.
    "CRATONVM_MONITOR_MAX_SPINNERS",
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
    ///
    /// A knob whose default is ON on only SOME backends (a
    /// `flags::alloc_policy_defaults::BackendDefault`, gen r5w4/defaults8)
    /// needs one too: unsetting its key restores the per-backend default, which
    /// is ON on the backend that flipped it.
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
    E { group: Group::DBG, token: "write0-trace", on_key: Some("CRATONVM_DBG_WRITE0_TRACE"), off_key: None, off_word: None, since: "2026-09-22" },
    E { group: Group::DBG, token: "refdisc", on_key: Some("CRATONVM_DBG_REFDISC"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::DBG, token: "site-alias", on_key: Some("CRATONVM_DBG_SITE_ALIAS"), off_key: None, off_word: None, since: "2026-08-06" },
    E { group: Group::DBG, token: "sp-ic-sites", on_key: Some("CRATONVM_DBG_SP_IC_SITES"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::DBG, token: "stub-yield", on_key: Some("CRATONVM_DBG_STUB_YIELD"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::DBG, token: "access", on_key: Some("CRATONVM_DBG_ACCESS"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "active-profiles-identity-trace", on_key: Some("CRATONVM_ACTIVE_PROFILES_IDENTITY_TRACE"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::DBG, token: "aio", on_key: Some("CRATONVM_DBG_AIO"), off_key: None, off_word: None, since: "2026-06-23" },
    E { group: Group::DBG, token: "aio-inline", on_key: Some("CRATONVM_DBG_AIO_INLINE"), off_key: None, off_word: None, since: "2026-07-30" },
    // Round 9 wave 5 (2026-09-21), the third name `flag_declaration_guard` was
    // red on, and the only one of the three whose read site also had to change:
    // `native-io/src/lib.rs` read it with a RAW `std::env::var_os`, so it was
    // outside the flag machinery altogether rather than merely undeclared.
    // That read is now `flags::runtime_var_os`, which is what declaring a name
    // obliges -- a raw `std::env::var` on a declared name trips check 4 of
    // `tools/flag-census/check-surface.sh`.
    //
    // Pure DBG by the group's contract: it gates two `eprintln!`s naming the
    // `sun.nio.ch.PendingFuture` field slots that `try_wrap_pending_future`
    // resolved. Nothing downstream reads `dbg`, so no token here can change a
    // program's result -- it is not one of the three documented exceptions.
    // Opt-IN by presence, so `off_key`/`off_word` stay `None`: unsetting the
    // key IS the off state, and a `"0"` would leave the print armed.
    //
    // Behaviour with the flag unset is unchanged for the same checkable reason
    // as the two GC rows: nothing in this tree ever writes this name -- no
    // `set_var`, no script, no fixture -- so the latched snapshot and the live
    // `getenv` it replaces return `None` alike, and the prints stay silent.
    E { group: Group::DBG, token: "afc-pending-future", on_key: Some("CRATONVM_DBG_AFC_PENDING_FUTURE"), off_key: None, off_word: None, since: "2026-09-21" },
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
    E { group: Group::DBG, token: "deadref-store", on_key: Some("CRATONVM_DBG_DEADREF_STORE"), off_key: None, off_word: None, since: "2026-09-09" },
    E { group: Group::DBG, token: "watch-addr", on_key: Some("CRATONVM_DBG_WATCH_ADDR"), off_key: None, off_word: None, since: "2026-09-09" },
    E { group: Group::DBG, token: "bb", on_key: Some("CRATONVM_DBG_BB"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "bblp", on_key: Some("CRATONVM_DBG_BBLP"), off_key: None, off_word: None, since: "2026-05-24" },
    E { group: Group::DBG, token: "bd-debug", on_key: Some("CRATONVM_BD_DEBUG"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "blocked-access", on_key: Some("CRATONVM_DBG_BLOCKED_ACCESS"), off_key: None, off_word: None, since: "2026-07-15" },
    E { group: Group::DBG, token: "blockgc", on_key: Some("CRATONVM_DBG_BLOCKGC"), off_key: None, off_word: None, since: "2026-06-10" },
    E { group: Group::DBG, token: "root-remap-audit", on_key: Some("CRATONVM_DBG_ROOT_REMAP_AUDIT"), off_key: None, off_word: None, since: "2026-08-17" },
    // gen r5w1/crash5: every raw root write-back (blocked-peer native-stack
    // word, shadow-stack indirect home, and the JIT frame remaps once wired)
    // logs where its target lies relative to the writing thread's own stack
    // (`cratonvm_gc::root_write_audit`). Presence-tested: no `off_word`.
    E { group: Group::DBG, token: "root-write-audit", on_key: Some("CRATONVM_DBG_ROOT_WRITE_AUDIT"), off_key: None, off_word: None, since: "2026-09-26" },
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
    E { group: Group::DBG, token: "deopt-eager-chains", on_key: Some("CRATONVM_DEOPT_EAGER_CHAINS"), off_key: None, off_word: None, since: "2026-09-28" },
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
    E { group: Group::DBG, token: "deadrecv", on_key: Some("CRATONVM_DBG_DEADRECV"), off_key: None, off_word: None, since: "2026-09-07" },
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
    // The reachability oracle that judges the three narrowings above.
    E { group: Group::DBG, token: "verify-reg-oop-maps", on_key: Some("CRATONVM_DBG_VERIFY_REG_OOP_MAPS"), off_key: None, off_word: None, since: "2026-09-09" },
    E { group: Group::GC, token: "late-resolve-dropped", on_key: Some("CRATONVM_GC_LATE_RESOLVE_DROPPED"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::GC, token: "tlab-skip", on_key: None, off_key: Some("CRATONVM_GC_NO_TLAB_SKIP"), off_word: None, since: "2026-09-06" },
    E { group: Group::GC, token: "g1-only-jit-pins", on_key: Some("CRATONVM_GC_G1_ONLY_JIT_PINS"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::GC, token: "peer-pin-divert", on_key: None, off_key: Some("CRATONVM_GC_NO_PEER_PIN_DIVERT"), off_word: None, since: "2026-09-06" },
    E { group: Group::GC, token: "conditional-tlab-skip-publish", on_key: Some("CRATONVM_GC_CONDITIONAL_TLAB_SKIP_PUBLISH"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::GC, token: "frame-trace-span-retire", on_key: None, off_key: Some("CRATONVM_GC_NO_FRAME_TRACE_SPAN_RETIRE"), off_word: None, since: "2026-09-06" },
    // The three conservative-JIT-root narrowings the register oop maps
    // license, each its own lever because each rests on a DIFFERENT claim and a
    // regression has to be attributable to one of them: the compiler's register
    // model, the operand-spill cursor, and the outgoing-argument reserve.
    E { group: Group::GC, token: "reg-oop-maps", on_key: Some("CRATONVM_GC_REG_OOP_MAPS"), off_key: None, off_word: Some("0"), since: "2026-09-09" },
    E { group: Group::GC, token: "dead-spill-roots", on_key: Some("CRATONVM_GC_DEAD_SPILL_ROOTS"), off_key: None, off_word: Some("0"), since: "2026-09-09" },
    E { group: Group::GC, token: "outgoing-arg-roots", on_key: Some("CRATONVM_GC_OUTGOING_ARG_ROOTS"), off_key: None, off_word: Some("0"), since: "2026-09-09" },
    // gen r5w6/oomjit10: default ON, `=0` keeps every callee-saved GPR image
    // word a conservative root again. A callee's image of its caller's
    // register is dropped when the compiled frame that owns the register
    // proves it dead there (`vm/src/jit/conservative_roots.rs`,
    // `caller_register_claim`); the VM's own Rust callers always keep theirs.
    E { group: Group::GC, token: "callee-saved-image-liveness", on_key: Some("CRATONVM_GC_CALLEE_SAVED_IMAGE_LIVENESS"), off_key: None, off_word: Some("0"), since: "2026-09-27" },
    // gen r5w6/oomjit10: default ON (the words are dropped), `=0` keeps them
    // roots. An optimizing-tier frame's primitive colours
    // (`OopMapEntry::non_oop_stack_slots`) never hold a reference of the
    // activation, only a previous frame's leftover; they stop being roots
    // unless the word is a minted handle (`conservative_roots::ir_prim_slots`).
    E { group: Group::GC, token: "ir-prim-slot-roots", on_key: Some("CRATONVM_GC_IR_PRIM_SLOT_ROOTS"), off_key: None, off_word: Some("0"), since: "2026-09-27" },
    // gce e1/f: default-ON kill switches (`=0` restores the old band claims):
    // a compiled frame's stack-pointer local map and a never-written frame-deopt
    // register image stop rooting stale words (`vm/src/jit/conservative_roots.rs`).
    E { group: Group::GC, token: "sp-local-map-roots", on_key: Some("CRATONVM_GC_SP_LOCAL_MAP_ROOTS"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::GC, token: "deopt-image-residue-roots", on_key: Some("CRATONVM_GC_DEOPT_IMAGE_RESIDUE_ROOTS"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    // gen r5w2/oomjit6: opt-in (default OFF). A safepoint that staged a
    // reference where no map names it keeps its register-oop mask instead of
    // withdrawing it; the method's outgoing-reserve claim is withdrawn instead
    // (`jit/src/x64/safepoint.rs`, `live_oop_register_mask`; `x64/driver.rs`).
    E { group: Group::GC, token: "staged-args-keep-mask", on_key: Some("CRATONVM_GC_STAGED_ARGS_KEEP_MASK"), off_key: None, off_word: None, since: "2026-09-26" },
    // G1's pin set honouring the movable/rewritable partition the
    // generational path has always honoured.
    // DEFAULT-ON since 2026-09-12, so a KILL SWITCH, and it grew an `off_word`
    // with the flip -- the same correction the ZGC rows above needed, for the
    // same reason: without it `CRATONVM_GC=-g1-movable-pins` expands to
    // UNSETTING the key, and an unset key now means ON. It shipped opt-in on
    // 2026-09-09 having measured itself worth 1 pin in 34; the deopt
    // `SavedRegisters` partition (`FrameLayout::deopt_gpr_lo`) is what made it
    // worth 6 of 7. See `gc/src/g1.rs::g1_movable_pins_enabled`.
    E { group: Group::GC, token: "g1-movable-pins", on_key: Some("CRATONVM_GC_G1_MOVABLE_PINS"), off_key: None, off_word: Some("0"), since: "2026-09-09" },
    // The producer half: publishing the verifiable band partition as movable.
    E { group: Group::GC, token: "movable-band-roots", on_key: Some("CRATONVM_GC_MOVABLE_BAND_ROOTS"), off_key: None, off_word: Some("0"), since: "2026-09-09" },
    // UNSAFE pricing lever for the A5 unregistered-frame span sweep.
    E { group: Group::JIT, token: "a5-mark-span", on_key: Some("CRATONVM_JIT_A5_MARK_SPAN"), off_key: None, off_word: Some("0"), since: "2026-09-09" },
    // Recover frames from the A5 band instead of sweeping it as a raw span.
    E { group: Group::JIT, token: "a5-frame-scan", on_key: Some("CRATONVM_JIT_A5_FRAME_SCAN"), off_key: None, off_word: None, since: "2026-09-09" },
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
    // Value-carrying: the `[gcpause]` report threshold in microseconds
    // (default 100000; `0` = every cycle). gen r4w2/obs.
    E { group: Group::DBG, token: "gcpause-min-us", on_key: Some("CRATONVM_DBG_GCPAUSE_MIN_US"), off_key: None, off_word: None, since: "2026-09-23" },
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
    E { group: Group::DBG, token: "young-mark-watch", on_key: Some("CRATONVM_DBG_YOUNG_MARK_WATCH"), off_key: None, off_word: None, since: "2026-09-07" },
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
    // `cast-memo-crosscheck` — re-run the full `instanceof` path on every
    // negative-memo hit and report a disagreement. Presence-parsed.
    E { group: Group::DBG, token: "cast-memo-crosscheck", on_key: Some("CRATONVM_DBG_CAST_MEMO_CROSSCHECK"), off_key: None, off_word: None, since: "2026-09-23" },
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
    E { group: Group::DBG, token: "ir-param-fill", on_key: Some("CRATONVM_DBG_IR_PARAM_FILL"), off_key: None, off_word: None, since: "2026-09-29" },
    E { group: Group::DBG, token: "ir-self-call-answer", on_key: Some("CRATONVM_DBG_IR_SELF_CALL_ANSWER"), off_key: None, off_word: None, since: "2026-09-29" },
    E { group: Group::DBG, token: "ir-entry-fold", on_key: Some("CRATONVM_DBG_IR_ENTRY_FOLD"), off_key: None, off_word: None, since: "2026-09-29" },
    E { group: Group::DBG, token: "ir-bufsize", on_key: Some("CRATONVM_DBG_IR_BUFSIZE"), off_key: None, off_word: None, since: "2026-08-01" },
    // The witness half of `jit/code-near-globals`: one line per placement
    // decision. That flag is a HINT to `mmap`, so "set" and "engaged" are
    // different facts and only this line separates them.
    E { group: Group::DBG, token: "code-near-globals", on_key: Some("CRATONVM_DBG_CODE_NEAR_GLOBALS"), off_key: None, off_word: None, since: "2026-09-10" },
    // Which cell the layout-epoch counter ended up in, and its address. The
    // witness for `jit/epoch-cell`, whose install FAILS CLOSED and would
    // otherwise do so silently.
    E { group: Group::DBG, token: "epoch-cell", on_key: Some("CRATONVM_DBG_EPOCH_CELL"), off_key: None, off_word: None, since: "2026-09-11" },
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
    E { group: Group::DBG, token: "jit-locals-floor", on_key: Some("CRATONVM_DBG_JIT_LOCALS_FLOOR"), off_key: None, off_word: None, since: "2026-09-10" },
    E { group: Group::DBG, token: "zip-immune", on_key: Some("CRATONVM_DBG_ZIPIMMUNE"), off_key: None, off_word: None, since: "2026-09-10" },
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
    // 2026-09-17, from the quarkus lane. Two presence-only traces, both read
    // with `std::env::var(..).is_ok()` at their sites and neither able to
    // change a result: `cmctx` prints why SmallRye's config-mapping context
    // construction took the arm it took, `findclass` prints the lookup class
    // and loader `MethodHandles.Lookup.findClass` resolved against.
    E { group: Group::DBG, token: "cmctx", on_key: Some("CRATONVM_DBG_CMCTX"), off_key: None, off_word: None, since: "2026-09-17" },
    E { group: Group::DBG, token: "findclass", on_key: Some("CRATONVM_DBG_FINDCLASS"), off_key: None, off_word: None, since: "2026-09-17" },
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
    E { group: Group::DBG, token: "jrt-modules", on_key: Some("CRATONVM_DBG_JRT_MODULES"), off_key: None, off_word: None, since: "2026-09-25" },
    E { group: Group::DBG, token: "jrt-read-count", on_key: Some("CRATONVM_DBG_JRT_READ_COUNT"), off_key: None, off_word: None, since: "2026-09-25" },
    E { group: Group::DBG, token: "instrument-census", on_key: Some("CRATONVM_INSTRUMENT_CENSUS"), off_key: None, off_word: None, since: "2026-09-25" },
    E { group: Group::DBG, token: "jit-names", on_key: Some("CRATONVM_DBG_JIT_NAMES"), off_key: None, off_word: None, since: "2026-06-16" },
    E { group: Group::DBG, token: "jit-spin-warn", on_key: Some("CRATONVM_DBG_JIT_SPIN_WARN"), off_key: None, off_word: None, since: "2026-09-28" },
    E { group: Group::DBG, token: "jit-pin", on_key: Some("CRATONVM_DBG_JIT_PIN"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::DBG, token: "jit-putfield", on_key: Some("CRATONVM_DBG_JIT_PUTFIELD"), off_key: None, off_word: None, since: "2026-06-01" },
    E { group: Group::DBG, token: "jit-safepoints", on_key: Some("CRATONVM_DBG_JIT_SAFEPOINTS"), off_key: None, off_word: None, since: "2026-07-24" },
    E { group: Group::DBG, token: "jit-stale-ic", on_key: Some("CRATONVM_DBG_JIT_STALE_IC"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::DBG, token: "jit-unmap", on_key: Some("CRATONVM_DBG_JIT_UNMAP"), off_key: None, off_word: None, since: "2026-07-28" },
    E { group: Group::DBG, token: "intrinsic", on_key: Some("CRATONVM_DBG_INTRINSIC"), off_key: None, off_word: None, since: "2026-08-13" },
    E { group: Group::DBG, token: "jitc", on_key: Some("CRATONVM_DBG_JITC"), off_key: None, off_word: None, since: "2026-05-21" },
    E { group: Group::DBG, token: "jitnpe", on_key: Some("CRATONVM_DBG_JITNPE"), off_key: None, off_word: None, since: "2026-09-11" },
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
    E { group: Group::DBG, token: "load-cse", on_key: Some("CRATONVM_DBG_LOAD_CSE"), off_key: None, off_word: None, since: "2026-09-11" },
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
    E { group: Group::DBG, token: "monitor-contention", on_key: Some("CRATONVM_DBG_MONITOR_CONTENTION"), off_key: None, off_word: None, since: "2026-09-26" },
    E { group: Group::DBG, token: "ir-poison-int-high", on_key: Some("CRATONVM_DBG_IR_POISON_INT_HIGH"), off_key: None, off_word: None, since: "2026-09-26" },
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
    // gce e1/j: per-phase nanosecond census of a JNI native call (door, dispatch,
    // in-native deposit sub-phases); one `[GC] jni_phase:` line at exit.
    E { group: Group::DBG, token: "jni-phase", on_key: Some("CRATONVM_DBG_JNI_PHASE"), off_key: None, off_word: None, since: "2026-09-29" },
    E { group: Group::DBG, token: "redefine-dump", on_key: Some("CRATONVM_DBG_REDEFINE_DUMP"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::DBG, token: "npe-stack", on_key: Some("CRATONVM_DBG_NPE_STACK"), off_key: None, off_word: None, since: "2026-05-28" },
    E { group: Group::DBG, token: "npe-trace", on_key: Some("CRATONVM_DBG_NPE_TRACE"), off_key: None, off_word: None, since: "2026-05-23" },
    E { group: Group::DBG, token: "nsee-trace", on_key: Some("CRATONVM_NSEE_TRACE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "nsme", on_key: Some("CRATONVM_DBG_NSME"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "null-native", on_key: Some("CRATONVM_DBG_NULL_NATIVE"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "nullthis", on_key: Some("CRATONVM_DBG_NULLTHIS"), off_key: None, off_word: None, since: "2026-06-11" },
    E { group: Group::DBG, token: "obj-equals", on_key: Some("CRATONVM_DBG_OBJ_EQUALS"), off_key: None, off_word: None, since: "2026-05-23" },
    E { group: Group::DBG, token: "obj-watch", on_key: Some("CRATONVM_DBG_OBJ_WATCH"), off_key: None, off_word: None, since: "2026-09-11" },
    E { group: Group::DBG, token: "objects", on_key: Some("CRATONVM_DBG_OBJECTS"), off_key: None, off_word: None, since: "2026-06-14" },
    E { group: Group::DBG, token: "objkey", on_key: Some("CRATONVM_DBG_OBJKEY"), off_key: None, off_word: None, since: "2026-08-01" },
    E { group: Group::DBG, token: "obsreg", on_key: Some("CRATONVM_DBG_OBSREG"), off_key: None, off_word: None, since: "2026-07-22" },
    E { group: Group::DBG, token: "oldsweep-owners", on_key: Some("CRATONVM_DBG_OLDSWEEP_OWNERS"), off_key: None, off_word: None, since: "2026-08-01" },
    // gen r4w5/oomjit5: per-root-category attribution of what every STW
    // Generational major keeps alive, printed at the end of the major
    // (`gc/src/gen_heap_oldmark_census.rs`), plus the JIT catch doors' thread
    // line (`vm/src/jit/helpers.rs`).
    E { group: Group::DBG, token: "oldmark-root-census", on_key: Some("CRATONVM_DBG_OLDMARK_ROOT_CENSUS"), off_key: None, off_word: None, since: "2026-09-24" },
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
    // gc-common w3-d: run finalize(), cleaner actions and the GC-enqueue
    // `ReferenceQueue` wake-ups on a per-VM delivery thread instead of the
    // allocating mutator (JLS 12.6). Default off until the battery has run it.
    E { group: Group::GC, token: "finalizer-thread", on_key: Some("CRATONVM_FINALIZER_THREAD"), off_key: None, off_word: None, since: "2026-09-23" },
    // gc-common w6-g: JNI local refs handed to native code as indirect
    // `(frame, slot)` handles into the remapped local-frame table instead of
    // raw addresses, so a native's own copy follows a moving collection.
    // Default off until a JNI-library run (netty-tcnative, JNA, lz4/zstd-jni).
    E { group: Group::GC, token: "jni-indirect-locals", on_key: Some("CRATONVM_JNI_INDIRECT_LOCALS"), off_key: None, off_word: None, since: "2026-09-24" },
    E { group: Group::GC, token: "jni-jclass-mirror", on_key: Some("CRATONVM_JNI_JCLASS_MIRROR"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    // gc-common w10-c: a foreign-attached thread runs every JNI function as a
    // counted mutator (idle -> running -> idle per function) and keeps the
    // locals it creates outside a Java call in an attach-level frame until
    // detach. Default off until the foreign-attach soak and a JNA callback run.
    E { group: Group::GC, token: "jni-foreign-transitions", on_key: Some("CRATONVM_JNI_FOREIGN_TRANSITIONS"), off_key: None, off_word: None, since: "2026-09-24" },
    // gcd d3/k: a Java thread inside a JNI native method runs in native
    // (GC-safe, HotSpot's `_thread_in_native`) and every JNIEnv function it
    // calls transitions native -> VM -> native, so a native that blocks in C
    // holds no pause. gcd d10/j: the PACKAGE switch -- it also turns on
    // `jni-indirect-locals` and `jni-foreign-transitions` unless either is set
    // off; `jni-foreign-transitions` is refused without indirect locals.
    // Default off until the JNI corpus / netty-tcnative / JNA runs.
    E { group: Group::GC, token: "jni-native-transitions", on_key: Some("CRATONVM_JNI_NATIVE_TRANSITIONS"), off_key: None, off_word: None, since: "2026-09-27" },
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
    E { group: Group::DBG, token: "sc-close-phases", on_key: Some("CRATONVM_DBG_SC_CLOSE_PHASES"), off_key: None, off_word: None, since: "2026-09-12" },
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
    // One line per starvation-watchdog sample: the carrier pool's queue depth,
    // busy/live counts and dispatch counter, plus the per-state thread census.
    // This is the reading that showed the pool running away from its base 32 to
    // 233 on `VthreadGcStress` while `dispatch_count` sat frozen -- the shape a
    // wall clock reports only as "the VM hung". Sampled on the watchdog's own
    // interval, so it costs nothing when off and nothing hot when on.
    E { group: Group::DBG, token: "carrier", on_key: Some("CRATONVM_DBG_CARRIER"), off_key: None, off_word: None, since: "2026-09-09" },
    E { group: Group::DBG, token: "sweep-census", on_key: Some("CRATONVM_DBG_SWEEP_CENSUS"), off_key: None, off_word: None, since: "2026-07-07" },
    E { group: Group::DBG, token: "sweep-edges", on_key: Some("CRATONVM_DBG_SWEEP_EDGES"), off_key: None, off_word: None, since: "2026-06-03" },
    E { group: Group::DBG, token: "unreg-declined", on_key: Some("CRATONVM_DBG_UNREG_DECLINED"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::DBG, token: "above-chain-kb", on_key: Some("CRATONVM_DBG_ABOVE_CHAIN_KB"), off_key: None, off_word: None, since: "2026-09-08" },
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
    // gce e2/t: debug -- a compiled poll declines to park for at most 200 ms per
    // pause, so the take-over freezes peers in compiled code (probe arm only).
    E { group: Group::DBG, token: "xt-force-takeover", on_key: Some("CRATONVM_DBG_XT_FORCE_TAKEOVER"), off_key: None, off_word: None, since: "2026-09-29" },
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
    E { group: Group::JIT, token: "sp-inline-mega", on_key: Some("CRATONVM_JIT_SP_INLINE_MEGA"), off_key: None, off_word: Some("0"), since: "2026-08-05" },
    E { group: Group::JIT, token: "sp-inline-mic", on_key: Some("CRATONVM_JIT_SP_INLINE_MIC"), off_key: None, off_word: Some("0"), since: "2026-08-05" },
    E { group: Group::JIT, token: "sp-inline-pic", on_key: Some("CRATONVM_JIT_SP_INLINE_PIC"), off_key: None, off_word: Some("0"), since: "2026-08-05" },
    E { group: Group::JIT, token: "unreg-memo-gc-reset", on_key: Some("CRATONVM_JIT_UNREG_MEMO_GC_RESET"), off_key: None, off_word: Some("0"), since: "2026-08-06" },
    E { group: Group::JIT, token: "frame-bands", on_key: None, off_key: Some("CRATONVM_JIT_NO_FRAME_BANDS"), off_word: None, since: "2026-08-20" },
    E { group: Group::JIT, token: "oopmap-coverage-presence-only", on_key: Some("CRATONVM_JIT_OOPMAP_COVERAGE_PRESENCE_ONLY"), off_key: None, off_word: None, since: "2026-08-20" },
    E { group: Group::GC, token: "moving-young-bounds-guard", on_key: None, off_key: Some("CRATONVM_MOVING_YOUNG_NO_BOUNDS_GUARD"), off_word: None, since: "2026-08-20" },
    E { group: Group::GC, token: "moving-young-band-object-screen", on_key: None, off_key: Some("CRATONVM_MOVING_YOUNG_NO_BAND_OBJECT_SCREEN"), off_word: None, since: "2026-09-03" },
    E { group: Group::GC, token: "moving-young-band-liveness-screen", on_key: None, off_key: Some("CRATONVM_MOVING_YOUNG_NO_BAND_LIVENESS_SCREEN"), off_word: None, since: "2026-09-03" },
    E { group: Group::GC, token: "moving-young-band-thread-window", on_key: None, off_key: Some("CRATONVM_MOVING_YOUNG_NO_BAND_THREAD_WINDOW"), off_word: None, since: "2026-09-04" },
    E { group: Group::JIT, token: "unreg-accept-residue", on_key: Some("CRATONVM_JIT_UNREG_ACCEPT_RESIDUE"), off_key: None, off_word: None, since: "2026-08-07" },
    // 2026-09-08. OPT-IN: the collection's own root pass also scans the
    // native-stack band ABOVE the JIT entry chain conservatively. Written as a
    // fix for the BindableTests ByteBuddy reclaim and measured NOT to be one —
    // kept as the lever that says so. See `above_chain_scan_enabled`.
    E { group: Group::JIT, token: "above-chain-scan", on_key: Some("CRATONVM_JIT_ABOVE_CHAIN_SCAN"), off_key: None, off_word: None, since: "2026-09-08" },
    E { group: Group::JIT, token: "above-chain-all-paths", on_key: Some("CRATONVM_JIT_ABOVE_CHAIN_ALL_PATHS"), off_key: None, off_word: None, since: "2026-09-08" },
    E { group: Group::JIT, token: "above-chain-from-sp", on_key: Some("CRATONVM_JIT_ABOVE_CHAIN_FROM_SP"), off_key: None, off_word: None, since: "2026-09-08" },
    E { group: Group::JIT, token: "a5-residue-filter", on_key: Some("CRATONVM_JIT_A5_RESIDUE_FILTER"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::JIT, token: "a5-shape-filter", on_key: Some("CRATONVM_JIT_A5_SHAPE_FILTER"), off_key: None, off_word: None, since: "2026-09-06" },
    // 2026-09-08. Default-ON kill switch over the RELOCATION LICENCE half of the
    // unregistered-JIT-frame probe: `0` restores the pre-fix behaviour, where a
    // hit the returned-frame residue mark explained still refused compaction for
    // the cycle. Marking is unaffected either way, so this can only change how
    // often the collector is allowed to compact. See
    // docs/internal/fixed-suite-bugs/gc/zgc-oom-on-mvstore-was-returned-frame-residue-FIXED-20260908.md
    E { group: Group::JIT, token: "unreg-residue-licence", on_key: Some("CRATONVM_JIT_UNREG_RESIDUE_LICENCE"), off_key: None, off_word: None, since: "2026-09-08" },
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
    E { group: Group::JIT, token: "bg-compile", on_key: Some("CRATONVM_BG_COMPILE"), off_key: None, off_word: Some("0"), since: "2026-06-18" },
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
    E { group: Group::JIT, token: "buffer-session-direct", on_key: Some("CRATONVM_JIT_BUFFER_SESSION_DIRECT"), off_key: None, off_word: Some("0"), since: "2026-09-12" },
    E { group: Group::JIT, token: "scoped-memory-direct", on_key: Some("CRATONVM_JIT_SCOPED_MEMORY_DIRECT"), off_key: None, off_word: Some("0"), since: "2026-09-15" },
    // 2026-09-24. `Unsafe.compareAndSet{Int,Long}` thin direct-call bind
    // (AQS `compareAndSetState`, i.e. every `CountDownLatch.countDown` from
    // compiled code). Default ON; `=0` sends every site back to the funnel.
    E { group: Group::JIT, token: "unsafe-cas-direct", on_key: Some("CRATONVM_JIT_UNSAFE_CAS_DIRECT"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    E { group: Group::JIT, token: "md-update-direct-helper", on_key: Some("CRATONVM_JIT_MD_UPDATE_DIRECT_HELPER"), off_key: None, off_word: Some("0"), since: "2026-08-28" },
    E { group: Group::JIT, token: "cached-entry-owner-reuse", on_key: Some("CRATONVM_JIT_CACHED_ENTRY_OWNER_REUSE"), off_key: None, off_word: Some("0"), since: "2026-08-17" },
    E { group: Group::JIT, token: "c2-first-call", on_key: Some("CRATONVM_JIT_C2_FIRST_CALL"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::JIT, token: "c2-supersede", on_key: Some("CRATONVM_C2_SUPERSEDE"), off_key: None, off_word: Some("0"), since: "2026-07-06" },
    E { group: Group::JIT, token: "callee-oop-flush", on_key: None, off_key: Some("CRATONVM_JIT_NO_CALLEE_OOP_FLUSH"), off_word: None, since: "2026-06-21" },
    E { group: Group::JIT, token: "spill-slots-cap", on_key: Some("CRATONVM_JIT_SPILL_SLOTS_CAP"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::JIT, token: "inline-reserve-path", on_key: None, off_key: Some("CRATONVM_JIT_NO_INLINE_RESERVE_PATH"), off_word: None, since: "2026-09-02" },
    E { group: Group::JIT, token: "code-cache-max-mb", on_key: Some("CRATONVM_JIT_CODE_CACHE_MAX_MB"), off_key: None, off_word: None, since: "2026-06-21" },
    // 2026-09-17. RT-8's cold-body sweeper: at the cap, retire the bodies whose
    // compiled entry counter has not moved since the previous sweep. Opt-in,
    // and it declines by itself unless `CRATONVM_JIT_ENTRY_COUNTER` is also on,
    // because that counter is the only per-artifact usage signal in the tree
    // and without it every body reads equally cold.
    E { group: Group::JIT, token: "code-cache-sweep", on_key: Some("CRATONVM_JIT_CODE_CACHE_SWEEP"), off_key: None, off_word: None, since: "2026-09-17" },
    E { group: Group::JIT, token: "conservative-locals", on_key: None, off_key: Some("CRATONVM_NO_CONSERVATIVE_LOCALS"), off_word: None, since: "2026-06-16" },
    E { group: Group::JIT, token: "ctor-direct-call", on_key: None, off_key: Some("CRATONVM_NO_CTOR_DIRECT_CALL"), off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "osr-ctor-bind", on_key: None, off_key: Some("CRATONVM_NO_OSR_CTOR_BIND"), off_word: None, since: "2026-08-17" },
    E { group: Group::JIT, token: "deny", on_key: Some("CRATONVM_JIT_DENY"), off_key: None, off_word: None, since: "2026-07-03" },
    // Default-ON: `deopt_real_enabled()` reads an unset key as on and only `0`/
    // `false`/`off`/`no` as off. With `off_word: None`, `-deopt-real` unset the key
    // and so left precise resume ON, which is why this takes `"0"` (2026-09-12).
    E { group: Group::JIT, token: "deopt-real", on_key: Some("CRATONVM_DEOPT_REAL"), off_key: None, off_word: Some("0"), since: "2026-06-17" },
    // The tier-up deopt sink's precise resume. Default-ON with a `0` opt-out:
    // off, the sink asks `can_deopt_resume` again and raises the hard
    // `InternalError: ... refusing side-effecting replay` an optimizing-tier
    // artifact could never avoid. Measurement lever, not a configuration.
    E { group: Group::JIT, token: "deopt-sink-resume", on_key: Some("CRATONVM_JIT_DEOPT_SINK_RESUME"), off_key: None, off_word: Some("0"), since: "2026-09-07" },
    E { group: Group::JIT, token: "direct-callee-calls", on_key: Some("CRATONVM_JIT_DIRECT_CALLEE_CALLS"), off_key: None, off_word: Some("0"), since: "2026-07-25" },
    E { group: Group::JIT, token: "dispatch-cache-direct-entry", on_key: Some("CRATONVM_JIT_DISPATCH_CACHE_DIRECT_ENTRY"), off_key: None, off_word: Some("0"), since: "2026-07-22" },
    E { group: Group::JIT, token: "dispatch-cache-virtual-direct-entry", on_key: Some("CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY"), off_key: None, off_word: Some("0"), since: "2026-07-25" },
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
    E { group: Group::JIT, token: "enable-callee-saved-gpr-locals", on_key: Some("CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS"), off_key: None, off_word: Some("0"), since: "2026-07-03" },
    E { group: Group::JIT, token: "enable-inline-new", on_key: Some("CRATONVM_JIT_ENABLE_INLINE_NEW"), off_key: None, off_word: None, since: "2026-07-12" },
    E { group: Group::JIT, token: "exc-table-c2", on_key: None, off_key: Some("CRATONVM_JIT_NO_EXC_TABLE_C2"), off_word: None, since: "2026-07-27" },
    E { group: Group::JIT, token: "force-c2", on_key: Some("CRATONVM_JIT_FORCE_C2"), off_key: None, off_word: None, since: "2026-07-31" },
    // The class-blind arm of the native-shadow seal. Default ON (correctness
    // guard); `-native-shadow-interface-blind` measures its cost.
    E { group: Group::JIT, token: "native-shadow-interface-blind", on_key: Some("CRATONVM_JIT_NATIVE_SHADOW_INTERFACE_BLIND"), off_key: None, off_word: Some("0"), since: "2026-08-10" },
    // The whole-method native-shadow caller seal at the eager and upgrade
    // doors. Retired to OPT-IN by interpreter round i1 wave 42 (lane L2): the
    // default worker never asked it and screens native shadows per site;
    // `native-shadow-caller-seal` restores it.
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
    // Class-hierarchy analysis (M5): the `unique_concrete_resolver` the three
    // `CompileRequest` sites in `vm/src/runtime/interpreter/jit_bridge.rs`
    // pass, backed by `ClassRealm::unique_implementor`. Default-ON opt-OUT —
    // `CRATONVM_JIT_CHA=0` withholds the resolver, and the JIT falls back to
    // the receiver-profile path at every site CHA would have answered for.
    //
    // It exists because CHA used to ride `guarded-virtual-inline` alone, which
    // made `=0` a correct lever for "is devirtualization the cause" and the
    // WRONG one for "is CHA the cause": the two produce different verdicts at
    // different sites, and a regression bisect could not separate them.
    //
    // The VM ANDs this with `guarded-virtual-inline` rather than reading it
    // alone, deliberately: CHA's verdict is lowered as a receiver class-id
    // compare, so it cannot be enabled where that guard cannot be emitted.
    // `=1` with `guarded-virtual-inline=0` therefore still means no CHA.
    // Read by `env_cache::jit_cha`.
    E { group: Group::JIT, token: "cha", on_key: Some("CRATONVM_JIT_CHA"), off_key: None, off_word: Some("0"), since: "2026-09-17" },
    // PGO-02: guarded monomorphic-virtual-call inlining (splice the callee
    // body behind a receiver class-id guard, miss falls to normal dispatch,
    // never a deopt). **Default-ON since N8** — it is the devirtualization
    // this tree performs, and with it off the shipped configuration performs
    // none at all: `cha` above is ANDed with it, so `=0` takes class-hierarchy
    // analysis away as well. `off_word: "0"` is the one-variable bisect for
    // devirtualization as a whole; see `env_cache::jit_guarded_virtual_inline`
    // for the argument and the soak this flip still owes, and
    // docs/feature-designs/profile-guided-inlining.md for the lowering.
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
    E { group: Group::JIT, token: "inline-fp-arith", on_key: None, off_key: Some("CRATONVM_JIT_NO_INLINE_FP_ARITH"), off_word: None, since: "2026-09-18" },
    E { group: Group::JIT, token: "inline-locals-floor", on_key: None, off_key: Some("CRATONVM_JIT_NO_INLINE_LOCALS_FLOOR"), off_word: None, since: "2026-09-08" },
    E { group: Group::JIT, token: "inline-new", on_key: None, off_key: Some("CRATONVM_JIT_DISABLE_INLINE_NEW"), off_word: None, since: "2026-05-28" },
    E { group: Group::JIT, token: "inline-putfield", on_key: None, off_key: Some("CRATONVM_NO_JIT_INLINE_PUTFIELD"), off_word: None, since: "2026-07-24" },
    E { group: Group::JIT, token: "inline-self-guard", on_key: Some("CRATONVM_JIT_INLINE_SELF_GUARD"), off_key: None, off_word: Some("0"), since: "2026-07-10" },
    E { group: Group::JIT, token: "inline-tlab-new", on_key: None, off_key: Some("CRATONVM_NO_JIT_INLINE_TLAB_NEW"), off_word: None, since: "2026-07-24" },
    E { group: Group::JIT, token: "inline-tlab-newarray", on_key: None, off_key: Some("CRATONVM_NO_JIT_INLINE_TLAB_NEWARRAY"), off_word: None, since: "2026-09-11" },
    E { group: Group::JIT, token: "intrinsics", on_key: None, off_key: Some("CRATONVM_DISABLE_INTRINSICS"), off_word: None, since: "2026-05-22" },
    E { group: Group::JIT, token: "sb-intrinsics", on_key: None, off_key: Some("CRATONVM_NO_JIT_SB_INTRINSICS"), off_word: None, since: "2026-09-11" },
    E { group: Group::JIT, token: "staged-arg-slot", on_key: None, off_key: Some("CRATONVM_NO_JIT_STAGED_ARG_SLOT"), off_word: None, since: "2026-09-11" },
    E { group: Group::JIT, token: "ir-branchy", on_key: None, off_key: Some("CRATONVM_NO_IR_BRANCHY"), off_word: None, since: "2026-06-18" },
    E { group: Group::JIT, token: "ir-call", on_key: Some("CRATONVM_JIT_IR_CALL"), off_key: None, off_word: Some("0"), since: "2026-06-20" },
    E { group: Group::JIT, token: "ir-call-special", on_key: Some("CRATONVM_JIT_IR_CALL_SPECIAL"), off_key: None, off_word: Some("0"), since: "2026-06-21" },
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
    // The attribution arm of `ir_optimize::LoopGuards::permits_hoist`. Default
    // ON, unlike this group's usual convention for a widening, because the arm
    // moves no guard (so it moves no deopt point) and everything it admits
    // clears a permission STRICTER than the unguarded path's. Setting it to `0`
    // restores the blanket refusal `control_token_has_guard` describes — the
    // control to bisect with if a guarded loop starts misbehaving.
    E { group: Group::JIT, token: "ir-licm-guard-attribution", on_key: Some("CRATONVM_JIT_IR_LICM_GUARD_ATTRIBUTION"), off_key: None, off_word: Some("0"), since: "2026-09-17" },
    // `Op::Guard`'s token edge: the PRODUCER side only. Default OFF, and the
    // default is the whole point — the consumer side (the arity-keyed shape
    // table, the verifier's lanes, `LoopGuards::permits_hoist`'s use of the
    // token) is complete and correct at either setting, so this gate decides
    // only whether `IrBuilder` appends the edge.
    //
    // Off because the failure mode of a modelling gap here has NO symptom: a
    // lane that rejects the new shape makes `ir_verify_reject` answer `true`,
    // the method falls back to the single-pass backend, and the optimizing
    // tier goes quiet on every method containing a guard — no crash, no failing
    // test, just a tier that stops doing anything.
    //
    // MEASURED 2026-09-17, which is the half the lane that wrote this could not
    // do: the whole `cratonvm-jit` suite runs clean with `=1` (3,192 passed,
    // and the one failure was an unrelated statics ratchet), and
    // `differential.rs`'s 28 tests pass with it set, including
    // `a_guarded_method_still_reaches_the_ir_tier_when_the_token_edge_is_emitted`
    // — which exists precisely to turn this flag's silent failure into a loud
    // one, by asserting `used_ir_backend` on a method carrying a division's
    // overflow guard. So no consumer rejects the wider shape today.
    //
    // It stays OFF anyway. A green test suite is not a soak: the tests compile
    // a few dozen small methods, and the population this gate protects is every
    // guarded method in a real application. Flip the default after a workload
    // run (hibernate or quarkus) shows the optimizing tier's compile count
    // unchanged with `=1`; the checklist is in the lane's NOTES-opts8.md.
    E { group: Group::JIT, token: "ir-guard-token", on_key: Some("CRATONVM_JIT_IR_GUARD_TOKEN"), off_key: None, off_word: None, since: "2026-09-17" },
    // The expander's census, printed on every decision and cumulative, so the
    // last line is the exit total. Cached in a `OnceLock` at the read site:
    // an uncached per-site flag read is how `CRATONVM_DBG_COMPACT_INLINE`
    // came to be 99.1% of all flag reads on a BigDecimal run.
    E { group: Group::DBG, token: "ir-string", on_key: Some("CRATONVM_DBG_IR_STRING"), off_key: None, off_word: None, since: "2026-09-01" },
    E { group: Group::JIT, token: "ir-deopt-resume", on_key: Some("CRATONVM_IR_DEOPT_RESUME"), off_key: None, off_word: None, since: "2026-06-18" },
    E { group: Group::JIT, token: "ir-direct-call", on_key: Some("CRATONVM_JIT_IR_DIRECT_CALL"), off_key: None, off_word: Some("0"), since: "2026-07-25" },
    // A/B opt-out: `-ir-buffer-estimate` restores the legacy code-buffer sizing
    // (`ir_lower.rs`), whose under-estimate floods "code buffer estimate too
    // small" bails. Declared as the OFF half of a default-on knob, so the
    // supported spelling is `CRATONVM_JIT=-ir-buffer-estimate`.
    E { group: Group::JIT, token: "ir-buffer-estimate", on_key: None, off_key: Some("CRATONVM_JIT_IR_LEGACY_BUFFER_ESTIMATE"), off_word: None, since: "2026-08-01" },
    E { group: Group::JIT, token: "ir-fp", on_key: Some("CRATONVM_JIT_IR_FP"), off_key: None, off_word: Some("0"), since: "2026-06-21" },
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
    // Array loads and PRIMITIVE array stores. A real lowering change, not
    // bookkeeping: the AIOOBE pad and the array null-check stub both published
    // nothing, and now route to deopt-stub reasons 11 and 10. `aastore` is
    // excluded -- its ZGC-barrier fallback arm still does not publish.
    E { group: Group::JIT, token: "precise-array-access", on_key: None, off_key: Some("CRATONVM_JIT_NO_PRECISE_ARRAY_ACCESS"), off_word: None, since: "2026-09-06" },
    // Opt-in. The GP register file landed beside the FP one on 2026-09-02, but
    // the flip still wants a wall-clock measurement -- see
    // `ir_lower::linear_scan_enabled`. `since` stays 2026-08-01: the flag is the
    // same flag, and this column dates the KNOB, not its capability.
    E { group: Group::JIT, token: "ir-linear-scan", on_key: Some("CRATONVM_JIT_IR_LINEAR_SCAN"), off_key: None, off_word: Some("0"), since: "2026-08-01" },
    // DEFAULT-ON since round 9 wave 4 (`runtime_flag_default_on`; 92 soak runs
    // with the bump engaged, outputs identical to HotSpot), so `0` is the kill
    // switch. Allocation-bearing methods reach the optimizing tier through the
    // entry counter (default-on since wave 3), which is what makes it live.
    E { group: Group::JIT, token: "ir-inline-tlab", on_key: Some("CRATONVM_JIT_IR_INLINE_TLAB"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "tls-thread-fetch", on_key: Some("CRATONVM_JIT_TLS_THREAD_FETCH"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "ir-receiver-guard-cse", on_key: Some("CRATONVM_JIT_IR_RECEIVER_GUARD_CSE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "ir-residency-pays", on_key: Some("CRATONVM_JIT_IR_RESIDENCY_PAYS"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "ir-fused-branch", on_key: Some("CRATONVM_JIT_IR_FUSED_BRANCH"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "ir-const-imm", on_key: Some("CRATONVM_JIT_IR_CONST_IMM"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "ir-phi-residency", on_key: Some("CRATONVM_JIT_IR_PHI_RESIDENCY"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "ir-phi-copy-regs", on_key: Some("CRATONVM_JIT_IR_PHI_COPY_REGS"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    E { group: Group::JIT, token: "ir-phi-copy-direct", on_key: Some("CRATONVM_JIT_IR_PHI_COPY_DIRECT"), off_key: None, off_word: Some("0"), since: "2026-09-11" },
    E { group: Group::JIT, token: "ir-branch-layout-polarity", on_key: Some("CRATONVM_JIT_IR_BRANCH_LAYOUT_POLARITY"), off_key: None, off_word: Some("0"), since: "2026-09-11" },
    E { group: Group::JIT, token: "ir-phi-home-publish-guard", on_key: Some("CRATONVM_JIT_IR_PHI_HOME_PUBLISH_GUARD"), off_key: None, off_word: Some("0"), since: "2026-09-07" },
    E { group: Group::JIT, token: "ir-phi-edge-interfere", on_key: Some("CRATONVM_JIT_IR_PHI_EDGE_INTERFERE"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-skip-republish", on_key: Some("CRATONVM_JIT_IR_SKIP_REPUBLISH"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    E { group: Group::JIT, token: "ir-deopt-regs", on_key: Some("CRATONVM_JIT_IR_DEOPT_REGS"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    E { group: Group::JIT, token: "ir-osr-entry", on_key: Some("CRATONVM_JIT_IR_OSR_ENTRY"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    // 2026-09-16. `ir-osr-entry` emits the stubs; this one says emit them even
    // into a body `try_osr` cannot enter (one that is not
    // `ir_osr_sentinel_free`, i.e. any method containing a call). Default OFF —
    // those stubs are unreachable — and it exists so the skip can be A/B'd on
    // one binary. Opt-IN, hence `off_key`/`off_word` unset.
    E { group: Group::JIT, token: "ir-osr-entry-always", on_key: Some("CRATONVM_JIT_IR_OSR_ENTRY_ALWAYS"), off_key: None, off_word: None, since: "2026-09-16" },
    // Round 11 wave 10 (lane irexc): IR reason-9 rethrow pads admit RBC.6 methods
    // (javac synchronized blocks) to the IR tier. OPT-IN until soaked.
    E { group: Group::JIT, token: "ir-precise-handler-frames", on_key: Some("CRATONVM_JIT_IR_PRECISE_HANDLER_FRAMES"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    E { group: Group::JIT, token: "ir-precise-frames-synchronized", on_key: Some("CRATONVM_JIT_IR_PRECISE_FRAMES_SYNCHRONIZED"), off_key: None, off_word: None, since: "2026-09-29" },
    // 2026-09-16. Carve JIT code buffers out of a shared executable arena
    // instead of one `VirtualAlloc`/`mmap` per method (64 KiB of address space
    // per ~5 KB body on Windows). Default OFF: unmeasured, and the
    // address-reuse ordering it imposes on `CompiledMethod::drop` is stated
    // rather than enforced. Opt-in.
    E { group: Group::JIT, token: "code-arena", on_key: Some("CRATONVM_JIT_CODE_ARENA"), off_key: None, off_word: None, since: "2026-09-16" },
    // 2026-09-16. Shadow-compile every method the optimizing tier takes with
    // the single-pass backend too, and report any capability the optimizing
    // body LOST (VEX, REP-string, the sieve SWAR constants). A diagnostic:
    // with it on, every IR-tier compile pays a full single-pass compile it
    // throws away, which is fine for a corpus run and unacceptable in
    // production. Opt-in.
    E { group: Group::JIT, token: "backend-parity", on_key: Some("CRATONVM_JIT_BACKEND_PARITY"), off_key: None, off_word: None, since: "2026-09-16" },
    // 2026-09-16. Profile persistence: write the live interpreter profile at
    // shutdown, replay it at startup to pre-seed tier decisions and receiver
    // shapes. Both take a PATH, not a boolean — unset or empty is off. A
    // replayed observation is a hint that every consumer can still deopt and
    // correct, never a fact.
    E { group: Group::JIT, token: "profile-save", on_key: Some("CRATONVM_JIT_PROFILE_SAVE"), off_key: None, off_word: None, since: "2026-09-16" },
    E { group: Group::JIT, token: "profile-load", on_key: Some("CRATONVM_JIT_PROFILE_LOAD"), off_key: None, off_word: None, since: "2026-09-16" },
    // 2026-09-17. The `-XX:+PrintCompilation` analogue: one line per install,
    // naming the method, the tier and the artifact. Opt-in, because it prints
    // on a path every compile takes.
    E { group: Group::JIT, token: "print-compilation", on_key: Some("CRATONVM_JIT_PRINT_COMPILATION"), off_key: None, off_word: None, since: "2026-09-17" },
    // 2026-09-17. Where the perf map is written. Takes a PATH, not a boolean:
    // unset keeps perf's own hard-coded `/tmp/perf-<pid>.map`, which is the
    // only location `perf report` looks in, so this is an explicit override
    // rather than a default that could silently write a profile the profiler
    // never finds. It exists so a multi-tenant `/tmp` is not the only choice
    // available -- see the `O_NOFOLLOW` note at the sink.
    E { group: Group::JIT, token: "perf-map-dir", on_key: Some("CRATONVM_JIT_PERF_MAP_DIR"), off_key: None, off_word: None, since: "2026-09-17" },
    // 2026-09-16. Apply `escape_analysis`'s lock-coarsening plans, which have
    // been computed on every EA run since the pass landed and never applied.
    // Opt-in: coarsening holds a lock for longer, which is a throughput trade
    // nobody has measured.
    E { group: Group::JIT, token: "lock-coarsen", on_key: Some("CRATONVM_JIT_LOCK_COARSEN"), off_key: None, off_word: None, since: "2026-09-16" },
    // ── The nine C2-cost changes of 2026-09-09 (`d21ae9e3f`) ──────────
    //
    // Landed on `dev` with no INVENTORY rows, which held the pre-push
    // flag-surface gate red for every branch cut from it. Declared here from
    // their read sites; the defaults below are what those sites actually do,
    // not what the commit message summarised.
    //
    // R2a. Splice a callee whose body carries `ldc`/`ldc2_w`: `IrInlineTables`
    // carries the constants now, so the builder no longer needs to invent the
    // float/double discriminator `InlineSite`'s raw `i64` dropped.
    E { group: Group::JIT, token: "ir-splice-ldc", on_key: Some("CRATONVM_JIT_IR_SPLICE_LDC"), off_key: None, off_word: Some("0"), since: "2026-09-09" },
    // R2b. Splice a callee whose body BRANCHES. The builder re-verifies each
    // relocated body on its own and rebases the merge targets and loop headers
    // that come out, which is the analysis the caller's `verified_code` cannot
    // supply for a region past its `code_len`.
    E { group: Group::JIT, token: "ir-splice-branch", on_key: Some("CRATONVM_JIT_IR_SPLICE_BRANCH"), off_key: None, off_word: Some("0"), since: "2026-09-09" },
    // Splice a callee whose body reads a STATIC. `InlineSite::static_field_info`
    // has carried the resolved rows all along; `IrInlineTables` now rebases
    // them, so the builder's `0xb2` arm finds a spliced site exactly as it
    // finds one of the caller's own. `putstatic` is refused separately and
    // unconditionally -- the builder has no arm for it.
    // Bind a surviving statically-bound call inside a spliced body to the
    // callee's entry with a raw CALL, instead of letting it fall through to
    // `jit_invoke_dispatch` and resolve the callee by name on every call. The
    // resolver had bound and keep-alive-registered the entry all along; nothing
    // put it where the lowerer looks.
    E { group: Group::JIT, token: "ir-splice-direct-call", on_key: Some("CRATONVM_JIT_IR_SPLICE_DIRECT_CALL"), off_key: None, off_word: Some("0"), since: "2026-09-09" },
    // Splice a callee whose body carries a `checkcast` or an `instanceof`.
    // `IrBuilder` has had both arms since cov-05; the splice scanner refused
    // the shape because nothing resolved the callee's targets or rebased the
    // rows. An unresolved target refuses the CALLEE rather than being dropped,
    // because a missing row bails the whole method.
    E { group: Group::JIT, token: "ir-splice-typecheck", on_key: Some("CRATONVM_JIT_IR_SPLICE_TYPECHECK"), off_key: None, off_word: Some("0"), since: "2026-09-09" },
    // Default ON. A splice must not leave behind a call worse than the one it
    // replaced: a surviving statically-bound call with no `direct_entry` has
    // no inline cache to fall back on and lowers to a blind name resolution.
    // Setting this to 0 re-admits that trade, for measuring it.
    E { group: Group::JIT, token: "ir-splice-refuse-unbindable", on_key: Some("CRATONVM_JIT_IR_SPLICE_REFUSE_UNBINDABLE"), off_key: None, off_word: Some("0"), since: "2026-09-10" },
    E { group: Group::JIT, token: "ir-splice-getstatic", on_key: Some("CRATONVM_JIT_IR_SPLICE_GETSTATIC"), off_key: None, off_word: Some("0"), since: "2026-09-09" },
    // Splice a callee whose body contains a `checkcast` or an `instanceof`.
    // The same rebase as the two above, and the refusal that page named as
    // the next one to take: every typed read out of an untyped container is a
    // `checkcast`. Opt-IN and default OFF -- the plumbing is here, the soak is
    // not, and a spliced type check is the first spliced site that can THROW
    // on a caller-produced value. Read site accepts `1`/`true`/`on`/`yes` and
    // nothing else, so there is no off-word: removing the key is the way back.
    // R1. Memoize the ACCEPTED optimizing OSR artifact, not only the refusals.
    // Without it one run recompiled the same method 502 times.
    E { group: Group::JIT, token: "osr-optimizing-cache", on_key: Some("CRATONVM_JIT_OSR_OPTIMIZING_CACHE"), off_key: None, off_word: Some("0"), since: "2026-09-09" },
    // R7. Elide the `JMP` to a block that is physically next.
    E { group: Group::JIT, token: "ir-fallthrough", on_key: Some("CRATONVM_JIT_IR_FALLTHROUGH"), off_key: None, off_word: Some("0"), since: "2026-09-09" },
    // R3, R4 and R6: opt-IN, default OFF pending their soaks. Each read site
    // accepts `1`/`true`/`on`/`yes` and nothing else, so there is no off-word
    // to state -- removing the key is the way back.
    E { group: Group::JIT, token: "ir-deopt-points-at-traps", on_key: Some("CRATONVM_JIT_IR_DEOPT_POINTS_AT_TRAPS"), off_key: None, off_word: None, since: "2026-09-09" },
    E { group: Group::JIT, token: "ir-reg-authoritative", on_key: Some("CRATONVM_JIT_IR_REG_AUTHORITATIVE"), off_key: None, off_word: Some("0"), since: "2026-09-09" },
    E { group: Group::JIT, token: "ir-gp-wide", on_key: Some("CRATONVM_JIT_IR_GP_WIDE"), off_key: None, off_word: None, since: "2026-09-10" },
    E { group: Group::JIT, token: "ir-epoch-guard-rip", on_key: Some("CRATONVM_JIT_IR_EPOCH_GUARD_RIP"), off_key: None, off_word: Some("0"), since: "2026-09-10" },
    // The single-pass twin of the row above, and the arm that unrolls: the
    // `osr/sp` body of a four-field loop carries eight of these guards.
    E { group: Group::JIT, token: "sp-epoch-guard-rip", on_key: Some("CRATONVM_JIT_SP_EPOCH_GUARD_RIP"), off_key: None, off_word: Some("0"), since: "2026-09-10" },
    // Default ON and UNSOUND when clear: it restores a baked compact body
    // offset that survives a layout replacement. It exists so the guard's
    // price is a number from one binary, not an argument.
    E { group: Group::JIT, token: "sp-field-layout-guard", on_key: Some("CRATONVM_JIT_SP_FIELD_LAYOUT_GUARD"), off_key: None, off_word: Some("0"), since: "2026-09-10" },
    // Diagnosis lever, default OFF: put the layout-replacement epoch back in
    // `.data` so its LOCATION is A/B-able independently of the encoding it
    // shipped with. Must not be combined with `code-near-globals`, whose
    // placement anchor is this counter.
    // Default ON: the layout-epoch counter takes a cell from the JIT code
    // cache's own allocator, so the short guard encoding is in reach by
    // construction. `=0` puts it back on the Rust heap, which is the arm the
    // 2026-09-11 A/B was taken against.
    E { group: Group::JIT, token: "epoch-cell", on_key: Some("CRATONVM_JIT_EPOCH_CELL"), off_key: None, off_word: Some("0"), since: "2026-09-11" },
    E { group: Group::JIT, token: "code-near-globals", on_key: Some("CRATONVM_JIT_CODE_NEAR_GLOBALS"), off_key: None, off_word: None, since: "2026-09-10" },
    E { group: Group::JIT, token: "ir-speculate", on_key: Some("CRATONVM_JIT_IR_SPECULATE"), off_key: None, off_word: None, since: "2026-09-09" },
    // Diagnosis lever, value-taking: a comma-separated list of conservative
    // root-band CLASSES to skip (`operand-spill`,
    // `outgoing-args-or-deopt-regs`, `safepoint-gpr-spill-image`). Absent
    // means skip nothing, which is the only setting that is safe -- dropping a
    // root frees what it named, and the lever exists to MEASURE the ceiling a
    // real fix would reach, not to be run. Landed 2026-09-09 with the G1
    // pinned-regions work and undeclared; see `band_skip_classes`.
    E { group: Group::JIT, token: "band-skip", on_key: Some("CRATONVM_JIT_BAND_SKIP"), off_key: None, off_word: None, since: "2026-09-09" },
    E { group: Group::JIT, token: "ls-carry-relief", on_key: Some("CRATONVM_JIT_LS_CARRY_RELIEF"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    E { group: Group::JIT, token: "ir-reserve-carried", on_key: Some("CRATONVM_JIT_IR_RESERVE_CARRIED"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    E { group: Group::JIT, token: "osr-optimizing", on_key: Some("CRATONVM_JIT_OSR_OPTIMIZING"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    E { group: Group::JIT, token: "osr-optimizing-memo", on_key: Some("CRATONVM_JIT_OSR_OPTIMIZING_MEMO"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    // Round 11 wave 9, tier proposal 26: the background OSR task builds the
    // optimizing OSR body too, and the mutator's background arm enters a
    // cached one. DEFAULT-ON; `0` restores the pre-wave-9 routing, where the
    // optimizing route was asked only after a single-pass OSR body published.
    E { group: Group::JIT, token: "osr-optimizing-bg", on_key: Some("CRATONVM_JIT_OSR_OPTIMIZING_BG"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // Round 11 wave 10 (lane irexc, tier proposal 27): optimizing OSR bodies whose
    // call-exception exits are resumable are admitted. DEFAULT-ON since wave 11
    // (soaked: R11 battery, legacy probes, IR census); `0` is the kill switch.
    E { group: Group::JIT, token: "osr-optimizing-exc-exits", on_key: Some("CRATONVM_JIT_OSR_OPTIMIZING_EXC_EXITS"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // Round 11 wave 12 (lane irexc, tier proposal 27 phase 3): optimizing OSR
    // bodies whose guard exits are exact or value-equivalent are admitted and
    // their guard exits resumed without a plan. DEFAULT-ON since wave 14
    // (soaked in waves 12 and 13: R11 battery, legacy probes, IR census);
    // `0` is the kill switch.
    E { group: Group::JIT, token: "osr-optimizing-guard-exits", on_key: Some("CRATONVM_JIT_OSR_OPTIMIZING_GUARD_EXITS"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // Round 11 wave 14 (lane osrmerge, r11w11-irexc-osr-entry-as-a-real-predecessor):
    // loop headers get an opaque OSR entry merge; the optimizing OSR door then
    // admits handler-reached headers and speculative guard exits. OPT-IN.
    E { group: Group::JIT, token: "osr-opaque-entry", on_key: Some("CRATONVM_JIT_OSR_OPAQUE_ENTRY"), off_key: None, off_word: None, since: "2026-09-24" },
    E { group: Group::JIT, token: "osr-opaque-handler-headers", on_key: Some("CRATONVM_JIT_OSR_OPAQUE_HANDLER_HEADERS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    // The remaining three of the 2026-09-09 C2-cost switches. The rest of that
    // work is declared in the block above; these are the ones whose read sites
    // arrived on a different branch.
    //
    // A spliced body with more than one `return`. Read by BOTH halves -- the
    // scanner in `jit_bridge` and `IrBuilder::splice_return` -- and off
    // because it measured neutral (the OSR containment it once cost came off
    // on 2026-09-09; see `ir_splice_multi_return_enabled`). No longer implied
    // by `ir-recursive-inline` (round 11 wave 11); the builder recognises a
    // multi-return body without it.
    E { group: Group::JIT, token: "ir-splice-multi-return", on_key: Some("CRATONVM_JIT_IR_SPLICE_MULTI_RETURN"), off_key: None, off_word: None, since: "2026-09-09" },
    E { group: Group::JIT, token: "self-lock-deopt-handover", on_key: Some("CRATONVM_JIT_SELF_LOCK_DEOPT_HANDOVER"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "deopt-unresumed-stash-releases", on_key: Some("CRATONVM_DEOPT_UNRESUMED_STASH_RELEASES"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    // Let a REFERENCE be register-resident, with the cached copy invalidated
    // at every point a collector could have run. Off because it does not pay,
    // not because it is unsafe.
    E { group: Group::JIT, token: "ir-ref-residency", on_key: Some("CRATONVM_JIT_IR_REF_RESIDENCY"), off_key: None, off_word: None, since: "2026-09-09" },
    // The two halves of `ir-ref-residency` cost different things: crossing a
    // safepoint admits the shapes that matter but RESERVES a register the
    // invalidation then makes unreadable for most of the range. Separated so
    // both are timeable from one binary. Only meaningful with the above on.
    E { group: Group::JIT, token: "ir-ref-residency-cross-safepoint", on_key: Some("CRATONVM_JIT_IR_REF_RESIDENCY_CROSS_SAFEPOINT"), off_key: None, off_word: Some("0"), since: "2026-09-09" },
    E { group: Group::JIT, token: "ir-drop-phi-home", on_key: Some("CRATONVM_JIT_IR_DROP_PHI_HOME"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    E { group: Group::JIT, token: "ir-publish-at-def", on_key: Some("CRATONVM_JIT_IR_PUBLISH_AT_DEF"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    E { group: Group::JIT, token: "ir-drop-home", on_key: Some("CRATONVM_JIT_IR_DROP_HOME"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    // Opt-IN: `ir_drop_unreachable_homes_enabled` is a `runtime_flag_on` read,
    // so unsetting the key IS the off state and `off_word` stays `None` (round
    // 9 wave 2; it said `Some("0")`, which rendered the row as default-on).
    E { group: Group::JIT, token: "ir-drop-unreachable-homes", on_key: Some("CRATONVM_JIT_IR_DROP_UNREACHABLE_HOMES"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-carry-single-use", on_key: Some("CRATONVM_JIT_IR_CARRY_SINGLE_USE"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    E { group: Group::JIT, token: "ir-sink-late", on_key: Some("CRATONVM_JIT_IR_SINK_LATE"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    E { group: Group::JIT, token: "ir-alu-imm", on_key: Some("CRATONVM_JIT_IR_ALU_IMM"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    // The 2026-09-10 instruction-count residue: the scheduler pairing that
    // feeds the carry, the second carry slot it fills, the fused compare that
    // reads its operands where they are, and the `LEA` that adds a constant
    // without routing through the accumulator. All four are default-ON levers
    // whose `0` is both the kill switch and the A/B arm.
    E { group: Group::JIT, token: "ir-pair-operands", on_key: Some("CRATONVM_JIT_IR_PAIR_OPERANDS"), off_key: None, off_word: Some("0"), since: "2026-09-09" },
    E { group: Group::JIT, token: "ir-carry-2nd", on_key: Some("CRATONVM_JIT_IR_CARRY_2ND"), off_key: None, off_word: Some("0"), since: "2026-09-09" },
    E { group: Group::JIT, token: "ir-cmp-in-place", on_key: Some("CRATONVM_JIT_IR_CMP_IN_PLACE"), off_key: None, off_word: Some("0"), since: "2026-09-10" },
    E { group: Group::JIT, token: "ir-add-lea", on_key: Some("CRATONVM_JIT_IR_ADD_LEA"), off_key: None, off_word: Some("0"), since: "2026-09-10" },
    E { group: Group::JIT, token: "merged-call-sentinel", on_key: Some("CRATONVM_JIT_MERGED_CALL_SENTINEL"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "ir-cold-arg-stage", on_key: Some("CRATONVM_JIT_IR_COLD_ARG_STAGE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    // Round 9 wave 2: put the `MFENCE` back after a volatile LOAD. Opt-IN —
    // `jit::runtime_lowering::volatile_load_fence_enabled` is a
    // `runtime_flag_on` read — so `off_word` stays `None`. The fence is gone by
    // default because an x86-TSO load is already an acquire and the JMM's
    // StoreLoad is the volatile STORE's fence; this is the one-release kill
    // switch for that memory-model change, on every tier that consults it.
    E { group: Group::JIT, token: "volatile-load-fence", on_key: Some("CRATONVM_JIT_VOLATILE_LOAD_FENCE"), off_key: None, off_word: None, since: "2026-09-18" },
    // Round 9 wave 2: zero every reference colour's frame word in the IR
    // prologue (and OSR entry stubs) so a safepoint map never publishes a word
    // this activation did not write. DEFAULT-ON (`runtime_flag_default_on`),
    // so a KILL SWITCH and `off_word` is exactly `"0"`.
    E { group: Group::JIT, token: "ir-zero-ref-slots", on_key: Some("CRATONVM_JIT_IR_ZERO_REF_SLOTS"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    // IR tier: publish a frame's reference homes ONCE per activation, as
    // indirect shadow-stack entries the collector rewrites through, instead of
    // pushing and copying back every live value around every call. DEFAULT-ON
    // (`runtime_flag_default_on`), so a KILL SWITCH and `off_word` is `"0"`.
    E { group: Group::JIT, token: "ir-shadow-frame-block", on_key: Some("CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK"), off_key: None, off_word: Some("0"), since: "2026-09-23" },
    // Round 9 wave 2, all default OFF (`runtime_flag_on`), so no `off_word`.
    // IR escape analysis: answer a load in another block from the allocation
    // block's last store when every store sits in the allocation's own block.
    E { group: Group::JIT, token: "ea-cross-block-loads", on_key: Some("CRATONVM_JIT_EA_CROSS_BLOCK_LOADS"), off_key: None, off_word: None, since: "2026-09-18" },
    // Single-pass inline primitive `putfield` (no helper call on the fast path).
    // Default ON since round 9 wave 4's integration (measured, see
    // `x64::op_field::inline_primitive_putfield_enabled`); `0` is the kill switch.
    E { group: Group::JIT, token: "inline-prim-putfield", on_key: Some("CRATONVM_JIT_INLINE_PRIM_PUTFIELD"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    // IR optimizer: count only snapshots a deopt can consult as uses in DCE and
    // write-only store elimination.
    E { group: Group::JIT, token: "ir-snapshot-liveness", on_key: Some("CRATONVM_JIT_IR_SNAPSHOT_LIVENESS"), off_key: None, off_word: None, since: "2026-09-18" },
    // IR optimizer: build volatile instance-field getfield/putfield as ordered
    // Load/Store nodes instead of refusing the method (stores also need the
    // ir_lower fence, ir::IR_LOWER_FENCES_VOLATILE_STORES).
    // Default ON since round 9 wave 7 (measured by irsnap7, see
    // `ir::ir_volatile_fields_enabled`); `0` is the kill switch.
    E { group: Group::JIT, token: "ir-volatile-fields", on_key: Some("CRATONVM_JIT_IR_VOLATILE_FIELDS"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    // Single-pass: place speculative BCE guards and the aaload / int-arith
    // hoists before a rotated loop's entry `goto`.
    // Default ON since round 9 wave 5 (measured, see
    // `x64::escape_analysis::rotated_preheader_enabled`); `0` is the kill switch.
    E { group: Group::JIT, token: "rotated-preheader", on_key: Some("CRATONVM_JIT_ROTATED_PREHEADER"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    // Round 9 wave 3. DEFAULT-ON kill switches (`0` restores the old behaviour):
    // direct binds inside a compile-time recursion cycle once the callee
    // publishes; the IR tier's constant-divisor strength reduction; the
    // profile-guarded inline String hashCode for `Object.hashCode()` sites;
    E { group: Group::JIT, token: "cycle-direct-bind", on_key: Some("CRATONVM_JIT_CYCLE_DIRECT_BIND"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    E { group: Group::JIT, token: "cycle-edge-cell", on_key: Some("CRATONVM_JIT_CYCLE_EDGE_CELL"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    E { group: Group::JIT, token: "instance-self-call", on_key: Some("CRATONVM_JIT_INSTANCE_SELF_CALL"), off_key: None, off_word: Some("0"), since: "2026-09-19" },
    E { group: Group::JIT, token: "div-guard-retier", on_key: Some("CRATONVM_JIT_DIV_GUARD_RETIER"), off_key: None, off_word: Some("0"), since: "2026-09-19" },
    E { group: Group::JIT, token: "osr-guard-exit-charge", on_key: Some("CRATONVM_JIT_OSR_GUARD_EXIT_CHARGE"), off_key: None, off_word: Some("0"), since: "2026-09-19" },
    E { group: Group::JIT, token: "osr-reentry-memo", on_key: Some("CRATONVM_JIT_OSR_REENTRY_MEMO"), off_key: None, off_word: Some("0"), since: "2026-09-19" },
    E { group: Group::JIT, token: "ir-carry-rcx-twin", on_key: Some("CRATONVM_JIT_IR_CARRY_RCX_TWIN"), off_key: None, off_word: Some("0"), since: "2026-09-19" },
    E { group: Group::JIT, token: "ir-box-unbox-fold", on_key: None, off_key: Some("CRATONVM_NO_IR_BOX_UNBOX_FOLD"), off_word: None, since: "2026-09-19" },
    E { group: Group::JIT, token: "retire-cell", on_key: Some("CRATONVM_JIT_RETIRE_CELL"), off_key: None, off_word: Some("0"), since: "2026-09-19" },
    E { group: Group::JIT, token: "arraylist-pin", on_key: None, off_key: Some("CRATONVM_JIT_NO_ARRAYLIST_PIN"), off_word: None, since: "2026-09-19" },
    E { group: Group::JIT, token: "omit-stack-trace-in-fast-throw", on_key: Some("CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW"), off_key: None, off_word: None, since: "2026-09-19" },
    E { group: Group::JIT, token: "ir-const-div", on_key: Some("CRATONVM_JIT_IR_CONST_DIV"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    E { group: Group::JIT, token: "object-hashcode-string", on_key: Some("CRATONVM_JIT_OBJECT_HASHCODE_STRING"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    // Round 9 wave 3, default OFF: inline `instanceof` FALSE for String and the
    // java/lang boxes on a class-id mismatch; bounded self-recursive IR inlining.
    // Opt-in since round 9 wave 3's integration measured the optimizing OSR
    // body faster once constant division was strength-reduced.
    E { group: Group::JIT, token: "osr-prefer-single-pass", on_key: Some("CRATONVM_JIT_OSR_PREFER_SINGLE_PASS"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    // Round 9 wave 4. Default-ON kill switch: compiled `invokespecial` sites use
    // the per-site native cache (e.g. an exception subclass's `super(msg)`).
    E { group: Group::JIT, token: "site-cache-special", on_key: Some("CRATONVM_JIT_SITE_CACHE_SPECIAL"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    // Round 9 wave 4, default OFF: a value whose last use is an operand of an
    // audited call-shaped op keeps its register through that op's clobber.
    E { group: Group::JIT, token: "ir-ls-call-last-use", on_key: Some("CRATONVM_JIT_IR_LS_CALL_LAST_USE"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    E { group: Group::JIT, token: "ir-ls-edge-resolution", on_key: Some("CRATONVM_JIT_IR_LS_EDGE_RESOLUTION"), off_key: None, off_word: None, since: "2026-09-18" },
    // Default ON since round 9 wave 5 (guarded by the duplicate-class-name
    // latch in `types`); `0` is the kill switch.
    E { group: Group::JIT, token: "instanceof-final-miss", on_key: Some("CRATONVM_JIT_INSTANCEOF_FINAL_MISS"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    // Round 9 wave 5: single-pass operand folding (ALU ops read operands in
    // place, `x = x OP y` stores fold). On by default; this is its kill switch.
    E { group: Group::JIT, token: "operand-fold", on_key: None, off_key: Some("CRATONVM_JIT_NO_OPERAND_FOLD"), off_word: None, since: "2026-09-18" },
    // Round 11: bounded self-recursive IR inlining, default ON (`CratonBench
    // fib`; dev flipped it independently for binary-trees on 2026-09-24);
    // `0` is the kill switch. A multi-return body is admitted under it only
    // as a copy of the method being compiled. Wave 11 reverted the flip
    // because the lowerer refused every spliced body; wave 12 (lane fib) fixed
    // the refusal in `ir_lower::Lowerer::phi_home_droppable`.
    E { group: Group::JIT, token: "ir-recursive-inline", on_key: Some("CRATONVM_JIT_IR_RECURSIVE_INLINE"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    // Round 11 wave 15 (lane fib): a second self-copy admission for bodies
    // whose replay repeats nothing visible (getfield / new / self call /
    // receiver-only constructor): binary-trees' `itemCheck`, `bottomUpTree`.
    // Default ON; `0` is the kill switch.
    E { group: Group::JIT, token: "ir-recursive-inline-replay-safe", on_key: Some("CRATONVM_JIT_IR_RECURSIVE_INLINE_REPLAY_SAFE"), off_key: None, off_word: Some("0"), since: "2026-09-25" },
    // Round 11 wave 15 (lane fib): a reference-free body's direct self-call
    // stores its safepoint id on the cold paths only. Default ON; `0` kills it.
    E { group: Group::JIT, token: "ir-self-call-lazy-sp-id", on_key: Some("CRATONVM_JIT_IR_SELF_CALL_LAZY_SP_ID"), off_key: None, off_word: Some("0"), since: "2026-09-25" },
    E { group: Group::JIT, token: "ir-ref-free-lean-prologue", on_key: Some("CRATONVM_JIT_IR_REF_FREE_LEAN_PROLOGUE"), off_key: None, off_word: Some("0"), since: "2026-09-25" },
    E { group: Group::JIT, token: "verify-post-optimize", on_key: Some("CRATONVM_JIT_VERIFY_POST_OPTIMIZE"), off_key: None, off_word: Some("0"), since: "2026-09-25" },
    E { group: Group::JIT, token: "ir-valueof-inline-box", on_key: Some("CRATONVM_JIT_IR_VALUEOF_INLINE_BOX"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "inline-inflated-lock", on_key: Some("CRATONVM_JIT_INLINE_INFLATED_LOCK"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "callee-promotion", on_key: Some("CRATONVM_JIT_CALLEE_PROMOTION"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "new-cp-site-memo", on_key: Some("CRATONVM_JIT_NEW_CP_SITE_MEMO"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "osr-loud-unresumable", on_key: Some("CRATONVM_JIT_OSR_LOUD_UNRESUMABLE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ic-supersede-retarget", on_key: Some("CRATONVM_JIT_IC_SUPERSEDE_RETARGET"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "verify-operand-types-release", on_key: Some("CRATONVM_JIT_VERIFY_OPERAND_TYPES_RELEASE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "verify-pre-lower-frame-states-release", on_key: Some("CRATONVM_JIT_VERIFY_PRE_LOWER_FRAME_STATES_RELEASE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "pgo-unroll", on_key: Some("CRATONVM_JIT_PGO_UNROLL"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "dominant-receiver-floor", on_key: Some("CRATONVM_JIT_DOMINANT_RECEIVER_FLOOR"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "osr-skip-entry-counter", on_key: Some("CRATONVM_JIT_OSR_SKIP_ENTRY_COUNTER"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-osr-header-entries-only", on_key: Some("CRATONVM_JIT_IR_OSR_HEADER_ENTRIES_ONLY"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "sp-valueof-inline-box", on_key: Some("CRATONVM_JIT_SP_VALUEOF_INLINE_BOX"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "mic-miss-calls-compiled", on_key: Some("CRATONVM_JIT_MIC_MISS_CALLS_COMPILED"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "quiesce-verdict-memo", on_key: Some("CRATONVM_JIT_QUIESCE_VERDICT_MEMO"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "lambda-get-memo", on_key: Some("CRATONVM_JIT_LAMBDA_GET_MEMO"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "inflated-exit-precheck", on_key: Some("CRATONVM_JIT_INFLATED_EXIT_PRECHECK"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "helper-wide-entry-calls", on_key: Some("CRATONVM_JIT_HELPER_WIDE_ENTRY_CALLS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "dispatch-virtual-memo-first", on_key: Some("CRATONVM_JIT_DISPATCH_VIRTUAL_MEMO_FIRST"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "long-valueof-inline-box", on_key: Some("CRATONVM_JIT_LONG_VALUEOF_INLINE_BOX"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "hashed-stub-wide", on_key: Some("CRATONVM_JIT_HASHED_STUB_WIDE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "mega-dispatch-table", on_key: Some("CRATONVM_JIT_MEGA_DISPATCH_TABLE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "eager-ordinary-door", on_key: Some("CRATONVM_JIT_EAGER_ORDINARY_DOOR"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "pgo-unroll-legacy-factor", on_key: Some("CRATONVM_JIT_PGO_UNROLL_LEGACY_FACTOR"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "drop-overtaken-tasks", on_key: Some("CRATONVM_JIT_DROP_OVERTAKEN_TASKS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "keep-optimizing-body", on_key: Some("CRATONVM_JIT_KEEP_OPTIMIZING_BODY"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-refuse-dead-end-block", on_key: Some("CRATONVM_JIT_IR_REFUSE_DEAD_END_BLOCK"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "lambda-get-service", on_key: Some("CRATONVM_JIT_LAMBDA_GET_SERVICE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "quiesce-by-generation", on_key: Some("CRATONVM_JIT_QUIESCE_BY_GENERATION"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "redefined-class-memo", on_key: Some("CRATONVM_JIT_REDEFINED_CLASS_MEMO"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "inline-inflated-spin", on_key: Some("CRATONVM_JIT_INLINE_INFLATED_SPIN"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "inline-inflated-two-way", on_key: Some("CRATONVM_JIT_INLINE_INFLATED_TWO_WAY"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-switch-search-tree", on_key: Some("CRATONVM_JIT_IR_SWITCH_SEARCH_TREE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-trap-replay-loop-extent", on_key: Some("CRATONVM_JIT_IR_TRAP_REPLAY_LOOP_EXTENT"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "replay-extent-handler-edges", on_key: Some("CRATONVM_JIT_REPLAY_EXTENT_HANDLER_EDGES"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "osr-drain-trap-frames", on_key: Some("CRATONVM_JIT_OSR_DRAIN_TRAP_FRAMES"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "mh-direct-lane", on_key: Some("CRATONVM_MH_DIRECT_LANE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "mh-direct-lane-raw-return", on_key: Some("CRATONVM_MH_DIRECT_LANE_RAW_RETURN"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "site-cache-mh-invoke", on_key: Some("CRATONVM_JIT_SITE_CACHE_MH_INVOKE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-long", on_key: Some("CRATONVM_JIT_IR_LONG"), off_key: None, off_word: Some("0"), since: "2026-06-21" },
    // Default-ON A/B lever: `x64::gated_ref_store_enabled` reads `0`. Its
    // predecessor `CRATONVM_NO_JIT_INLINE_PUTFIELD` measured exactly zero under
    // the default collector, because the path it disabled was already
    // unreachable there.
    E { group: Group::JIT, token: "gated-ref-store", on_key: Some("CRATONVM_JIT_GATED_REF_STORE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    // gen r4w4/cards4: the inline GENERATIONAL post barrier (card check before
    // and after each compiled reference store, collector call only for a clean
    // card). Opt-in, default OFF; `x64::objects::inline_card_mark_available`.
    E { group: Group::JIT, token: "inline-card-mark", on_key: Some("CRATONVM_JIT_INLINE_CARD_MARK"), off_key: None, off_word: None, since: "2026-09-24" },
    E { group: Group::JIT, token: "ir-inline-card-check", on_key: Some("CRATONVM_JIT_IR_INLINE_CARD_CHECK"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
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
    E { group: Group::JIT, token: "ir-selfrec-direct", on_key: Some("CRATONVM_JIT_IR_SELFREC_DIRECT"), off_key: None, off_word: Some("0"), since: "2026-06-22" },
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
    E { group: Group::JIT, token: "ir-site-trap", on_key: Some("CRATONVM_JIT_IR_SITE_TRAP"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-trap-replay-guard", on_key: Some("CRATONVM_JIT_IR_TRAP_REPLAY_GUARD"), off_key: None, off_word: Some("0"), since: "2026-09-07" },
    E { group: Group::JIT, token: "ir-scalar-intrinsics", on_key: Some("CRATONVM_JIT_IR_SCALAR_INTRINSICS"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    // DEFAULT-ON since 2026-09-06 (`ir_aastore_enabled` reads `0`/`false`/
    // `off`/`no` as off, unset as on), so a KILL SWITCH: `off_word` must be
    // `"0"` or `CRATONVM_JIT=-ir-aastore` merely unsets the key, which is ON
    // (round 9 wave 2).
    E { group: Group::JIT, token: "ir-aastore", on_key: Some("CRATONVM_JIT_IR_AASTORE"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-check-elim", on_key: Some("CRATONVM_JIT_IR_CHECK_ELIM"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-bce-range", on_key: Some("CRATONVM_JIT_IR_BCE_RANGE"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-range-edges", on_key: Some("CRATONVM_JIT_IR_RANGE_EDGES"), off_key: None, off_word: Some("0"), since: "2026-09-17" },
    E { group: Group::JIT, token: "ir-range-predicate", on_key: Some("CRATONVM_JIT_IR_RANGE_PREDICATE"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    E { group: Group::JIT, token: "ir-hot-layout", on_key: Some("CRATONVM_JIT_IR_HOT_LAYOUT"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-list-sched", on_key: Some("CRATONVM_JIT_IR_LIST_SCHED"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-carry-rcx-folded", on_key: Some("CRATONVM_JIT_IR_CARRY_RCX_FOLDED"), off_key: None, off_word: Some("0"), since: "2026-09-10" },
    E { group: Group::JIT, token: "ir-unroll-unreachable-frames", on_key: Some("CRATONVM_JIT_IR_UNROLL_UNREACHABLE_FRAMES"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-licm-mem-edge", on_key: Some("CRATONVM_JIT_IR_LICM_MEM_EDGE"), off_key: None, off_word: Some("0"), since: "2026-09-11" },
    E { group: Group::JIT, token: "ir-licm-before-unroll", on_key: Some("CRATONVM_JIT_IR_LICM_BEFORE_UNROLL"), off_key: None, off_word: None, since: "2026-09-11" },
    E { group: Group::JIT, token: "ir-load-cse", on_key: Some("CRATONVM_JIT_IR_LOAD_CSE"), off_key: None, off_word: Some("0"), since: "2026-09-11" },
    E { group: Group::JIT, token: "ir-load-cse-alias", on_key: Some("CRATONVM_JIT_IR_LOAD_CSE_ALIAS"), off_key: None, off_word: Some("0"), since: "2026-09-12" },
    // 2026-09-24 (round 11 wave 9). DEFAULT-ON kill switch for IR lock
    // coarsening (`ir_optimize::coarsen_adjacent_monitors`): an adjacent
    // `monitorexit(o); monitorenter(o)` on one control node is deleted.
    E { group: Group::JIT, token: "ir-lock-coarsen", on_key: Some("CRATONVM_JIT_IR_LOCK_COARSEN"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // 2026-09-24 (round 11 wave 14). OPT-IN IR nested-lock elision
    // (`ir_optimize::elide_nested_monitors`): the inner pair of
    // `synchronized (o) { synchronized (o) { .. } }` is deleted. DEFAULT-ON
    // kill switch since round 11 wave 17.
    E { group: Group::JIT, token: "ir-nested-lock-elim", on_key: Some("CRATONVM_JIT_IR_NESTED_LOCK_ELIM"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // 2026-09-24 (round 11 wave 11). DEFAULT-ON kill switch for committed
    // splice stores (`ir.rs` putfield arm): a spliced store into an object the
    // splice did not allocate is admitted when nothing after it can trap.
    E { group: Group::JIT, token: "ir-splice-committed-store", on_key: Some("CRATONVM_JIT_IR_SPLICE_COMMITTED_STORE"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    E { group: Group::JIT, token: "ir-licm-hoist-counted", on_key: Some("CRATONVM_JIT_IR_LICM_HOIST_COUNTED"), off_key: None, off_word: Some("0"), since: "2026-09-11" },
    E { group: Group::JIT, token: "ir-per-copy-frames", on_key: Some("CRATONVM_JIT_IR_PER_COPY_FRAMES"), off_key: None, off_word: None, since: "2026-09-11" },
    E { group: Group::JIT, token: "ir-partial-unroll", on_key: Some("CRATONVM_JIT_IR_PARTIAL_UNROLL"), off_key: None, off_word: None, since: "2026-09-11" },
    E { group: Group::JIT, token: "ir-partial-unroll-factor", on_key: Some("CRATONVM_JIT_IR_PARTIAL_UNROLL_FACTOR"), off_key: None, off_word: None, since: "2026-09-11" },
    E { group: Group::JIT, token: "ir-sink-equal-depth", on_key: Some("CRATONVM_JIT_IR_SINK_EQUAL_DEPTH"), off_key: None, off_word: None, since: "2026-09-12" },
    // Round 11 wave 12 (irback W11-2, lane ircore): the sink pass's φ-edge read
    // attribution without the equal-depth tie rule. OPT-IN.
    E { group: Group::JIT, token: "ir-sink-phi-edge", on_key: Some("CRATONVM_JIT_IR_SINK_PHI_EDGE"), off_key: None, off_word: None, since: "2026-09-24" },
    E { group: Group::JIT, token: "ir-residency-crossblock", on_key: Some("CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK"), off_key: None, off_word: None, since: "2026-09-12" },
    E { group: Group::JIT, token: "ir-residency-crossblock-budget", on_key: Some("CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK_BUDGET"), off_key: None, off_word: Some("0"), since: "2026-09-12" },
    E { group: Group::JIT, token: "c2-accept", on_key: Some("CRATONVM_C2_ACCEPT"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "c2-accept-memo", on_key: Some("CRATONVM_C2_ACCEPT_MEMO"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-unresolved-class-trap", on_key: Some("CRATONVM_JIT_IR_UNRESOLVED_CLASS_TRAP"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "ir-ls-loop-weight", on_key: Some("CRATONVM_JIT_IR_LS_LOOP_WEIGHT"), off_key: None, off_word: None, since: "2026-09-03" },
    // The linear-scan consumer's SPLIT half: follow a value the scan split from
    // register to register and back to memory, instead of declining it whole.
    // Off because it is unmeasured, not because it is unsound -- see R4 in
    // `NOTES-regalloc.md` and `docs/jit/linear-scan-regalloc.md`.
    //
    // `on_key` only, like `ir-ref-residency`: the flag is opt-in and has no
    // kill switch, because it IS the kill switch.
    E { group: Group::JIT, token: "ir-ls-splits", on_key: Some("CRATONVM_JIT_IR_LS_SPLITS"), off_key: None, off_word: None, since: "2026-09-17" },
    E { group: Group::JIT, token: "ir-param-copy", on_key: Some("CRATONVM_JIT_IR_PARAM_COPY"), off_key: None, off_word: None, since: "2026-09-04" },
    // 2026-09-24 (round 11 wave 14). DEFAULT-ON kill switch: an int/long
    // parameter's home is its prologue word, not a coloured copy
    // (`ir_lower::Lowerer::alias_param_home`).
    E { group: Group::JIT, token: "ir-param-home-alias", on_key: Some("CRATONVM_JIT_IR_PARAM_HOME_ALIAS"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    E { group: Group::JIT, token: "ir-rpo-layout", on_key: Some("CRATONVM_JIT_IR_RPO_LAYOUT"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    E { group: Group::JIT, token: "ir-call-anewarray", on_key: Some("CRATONVM_JIT_IR_CALL_ANEWARRAY"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    E { group: Group::JIT, token: "kernel-reg-locals", on_key: Some("CRATONVM_JIT_KERNEL_REG_LOCALS"), off_key: None, off_word: Some("0"), since: "2026-07-14" },
    E { group: Group::JIT, token: "kernel-reg-osr", on_key: Some("CRATONVM_JIT_KERNEL_REG_OSR"), off_key: None, off_word: Some("0"), since: "2026-07-25" },
    E { group: Group::JIT, token: "leak-code", on_key: Some("CRATONVM_JIT_LEAK_CODE"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::JIT, token: "licm", on_key: Some("CRATONVM_JIT_LICM"), off_key: None, off_word: Some("0"), since: "2026-06-18" },
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
    // Single-pass tier: publish a frame-slot shadow home by ADDRESS (an
    // indirect entry the collector rewrites through) instead of pushing its
    // value and copying it back after the call. DEFAULT-ON
    // (`runtime_flag_default_on`), so a KILL SWITCH and `off_word` is `"0"`.
    E { group: Group::JIT, token: "my-shadow-indirect", on_key: Some("CRATONVM_JIT_MY_SHADOW_INDIRECT"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // Single-pass tier: allocate a `new C; dup; <args>; invokespecial <init>`
    // at the constructor call instead of at `new`. DEFAULT-ON
    // (`runtime_flag_default_on`), so a KILL SWITCH and `off_word` is `"0"`.
    E { group: Group::JIT, token: "alloc-sink", on_key: Some("CRATONVM_JIT_ALLOC_SINK"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // Optimizing tier: an inline TLAB `new` of a class with no post-init
    // work skips the `tlab_post_init` call on a collector that walks chunks
    // (the single-pass tier's `TlabPostInit::Skip`). DEFAULT-ON
    // (`runtime_flag_default_on`), so a KILL SWITCH and `off_word` is `"0"`.
    E { group: Group::JIT, token: "ir-tlab-skip-post-init", on_key: Some("CRATONVM_JIT_IR_TLAB_SKIP_POST_INIT"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // Optimizing tier: an inline G1 reference store (compact receiver, null
    // old value, null or same-region new value), else the full helper.
    // DEFAULT-ON (`runtime_flag_default_on`), so a KILL SWITCH and `off_word`
    // is `"0"`.
    E { group: Group::JIT, token: "ir-g1-ref-store", on_key: Some("CRATONVM_JIT_IR_G1_REF_STORE"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // Optimizing tier: remove a zero/null store into a fresh compact object's
    // untouched field. DEFAULT-ON (`runtime_flag_default_on`), so a KILL
    // SWITCH and `off_word` is `"0"`.
    E { group: Group::JIT, token: "ir-initial-zero-stores", on_key: Some("CRATONVM_JIT_IR_INITIAL_ZERO_STORES"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // Optimizing tier: an inline `new`'s safepoint map (and so any frame-block
    // publication) on its slow path only. DEFAULT-ON
    // (`runtime_flag_default_on`), so a KILL SWITCH and `off_word` is `"0"`.
    E { group: Group::JIT, token: "ir-alloc-map-sink", on_key: Some("CRATONVM_JIT_IR_ALLOC_MAP_SINK"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // Optimizing tier: redundant-load elimination across a dominating block.
    // Only acts under `CRATONVM_JIT_IR_LOAD_CSE`. DEFAULT-ON
    // (`runtime_flag_default_on`), so a KILL SWITCH and `off_word` is `"0"`.
    E { group: Group::JIT, token: "ir-load-cse-dominance", on_key: Some("CRATONVM_JIT_IR_LOAD_CSE_DOMINANCE"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // Single-pass tier: an inlined constructor's reference store proven to
    // start from null skips a null value and, under G1, takes the constructor
    // arm. DEFAULT-ON (`runtime_flag_default_on`), so a KILL SWITCH and
    // `off_word` is `"0"`.
    E { group: Group::JIT, token: "ctor-store-from-null", on_key: Some("CRATONVM_JIT_CTOR_STORE_FROM_NULL"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // Single-pass and IR tiers: bind plain `jdk/internal/misc/Unsafe` scalar
    // accessor sites to the thin direct helpers (`jit::unsafe_accessor_direct_enabled`,
    // 1354647ae). DEFAULT-ON (`runtime_flag_default_on`); `0` sends every site
    // back through the generic native funnel. Declared in round 11 wave 5 (it
    // was read but undeclared, so it answered only to a live `getenv`).
    E { group: Group::JIT, token: "unsafe-accessor-direct", on_key: Some("CRATONVM_JIT_UNSAFE_ACCESSOR_DIRECT"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // Emit side of the register oop maps: `OopMapEntry::reg_oop_mask`.
    E { group: Group::JIT, token: "reg-oop-maps", on_key: Some("CRATONVM_JIT_REG_OOP_MAPS"), off_key: None, off_word: Some("0"), since: "2026-09-09" },
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
    E { group: Group::JIT, token: "osr-newarray", on_key: Some("CRATONVM_OSR_NEWARRAY"), off_key: None, off_word: Some("0"), since: "2026-07-10" },
    E { group: Group::JIT, token: "osr-exc-table", on_key: Some("CRATONVM_JIT_OSR_EXC_TABLE"), off_key: None, off_word: Some("0"), since: "2026-08-17" },
    // Declared 2026-08-24. An OPT-OUT: the optimizing tier republishes the
    // innermost-frame mirror after an inline-cache hit by default, and this key
    // restores the stale-mirror behaviour so one binary has both arms. See
    // `ir_lower::emit_call_loaded_ic_entry`.
    // Declared 2026-08-26. An OPT-OUT: the staged invoke-argument buffer is
    // published on the SHADOW stack by default, not merely named in the oop
    // map. The band verifier consults only the shadow stack. See
    // `x64::safepoint::collect_live_oop_homes`.
    E { group: Group::JIT, token: "staged-arg-shadow", on_key: None, off_key: Some("CRATONVM_JIT_NO_STAGED_ARG_SHADOW"), off_word: None, since: "2026-08-26" },
    E { group: Group::JIT, token: "ic-frame-republish", on_key: None, off_key: Some("CRATONVM_JIT_NO_IC_FRAME_REPUBLISH"), off_word: None, since: "2026-08-24" },
    E { group: Group::JIT, token: "zero-reserved-tail", on_key: None, off_key: Some("CRATONVM_JIT_NO_ZERO_RESERVED_TAIL"), off_word: None, since: "2026-09-04" },
    E { group: Group::JIT, token: "zero-unset-locals", on_key: None, off_key: Some("CRATONVM_JIT_NO_ZERO_UNSET_LOCALS"), off_word: None, since: "2026-09-04" },
    E { group: Group::JIT, token: "checkcast-inline", on_key: Some("CRATONVM_JIT_CHECKCAST_INLINE"), off_key: None, off_word: Some("0"), since: "2026-08-28" },
    E { group: Group::JIT, token: "final-devirt", on_key: Some("CRATONVM_JIT_FINAL_DEVIRT"), off_key: None, off_word: Some("0"), since: "2026-08-28" },
    E { group: Group::JIT, token: "final-devirt-native-screen", on_key: Some("CRATONVM_JIT_FINAL_DEVIRT_NATIVE_SCREEN"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    // Declared 2026-09-02. `final-devirt` above is `java/lang/String`'s
    // problem: String is final, so the rewrite it drives took EVERY String
    // access site away from the inline intrinsic. This is the opt-out for the
    // yield that gives them back -- default OFF, so the yield is on.
    E { group: Group::JIT, token: "devirt-intrinsic-yield", on_key: None, off_key: Some("CRATONVM_JIT_NO_DEVIRT_INTRINSIC_YIELD"), off_word: None, since: "2026-09-02" },
    E { group: Group::JIT, token: "inline-calls", on_key: Some("CRATONVM_JIT_INLINE_CALLS"), off_key: None, off_word: Some("0"), since: "2026-08-18" },
    E { group: Group::JIT, token: "inline-nest", on_key: Some("CRATONVM_JIT_INLINE_NEST"), off_key: None, off_word: Some("0"), since: "2026-08-18" },
    E { group: Group::JIT, token: "inline-call-dispatch", on_key: Some("CRATONVM_JIT_INLINE_CALL_DISPATCH"), off_key: None, off_word: None, since: "2026-08-18" },
    E { group: Group::JIT, token: "inline-splice-devirt", on_key: Some("CRATONVM_JIT_INLINE_SPLICE_DEVIRT"), off_key: None, off_word: Some("0"), since: "2026-08-18" },
    E { group: Group::JIT, token: "local-handlers", on_key: Some("CRATONVM_JIT_LOCAL_HANDLERS"), off_key: None, off_word: Some("0"), since: "2026-08-20" },
    // The IMPLICIT half of the row above, and a separate knob because it is a
    // separate pad, a separate runtime arm and a separate thing to bisect: a
    // bounds check, an inline null check or a zero-divisor guard entering this
    // method's own compiled `catch`. Reading `local-handlers` as `0` closes
    // this one too (`jit::implicit_local_handlers_enabled` ANDs them).
    E { group: Group::JIT, token: "implicit-local-handlers", on_key: Some("CRATONVM_JIT_IMPLICIT_LOCAL_HANDLERS"), off_key: None, off_word: Some("0"), since: "2026-09-20" },
    // gen r4w6/oomjit6: two opt-in (default OFF) retention fixes at a compiled
    // `catch`. `local-handler-clear-dead`: the local-handler stub zeroes the
    // frame words dead at its handler's entry (the operand-spill tail and the
    // outgoing reserve; `jit/src/x64/deopt_stubs.rs`). `local-handler-drop-orphans`:
    // the handler commit drops the exceptional frames its callees published
    // above the innermost JIT door's entry floor (`vm/src/jit/conservative_roots.rs`).
    E { group: Group::JIT, token: "local-handler-clear-dead", on_key: Some("CRATONVM_JIT_LOCAL_HANDLER_CLEAR_DEAD"), off_key: None, off_word: None, since: "2026-09-24" },
    E { group: Group::JIT, token: "local-handler-drop-orphans", on_key: Some("CRATONVM_JIT_LOCAL_HANDLER_DROP_ORPHANS"), off_key: None, off_word: None, since: "2026-09-24" },
    // gen r5w1/oom5: opt-in (default OFF). An OSR entry takes the reference
    // locals out of the interpreter frame it leaves behind, so they are not
    // roots for the whole OSR activation (`vm/src/runtime/interpreter/jit_bridge.rs`,
    // `osr_park_entry_locals_enabled`).
    E { group: Group::JIT, token: "osr-park-entry-locals", on_key: Some("CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    // gen r5w1/oom5: opt-in (default OFF). The OSR sink drops the exceptional
    // frames published inside the OSR activation above its entry floor
    // instead of re-stashing them (`route_osr_exception_out_of_artifact`).
    E { group: Group::JIT, token: "osr-drop-orphans", on_key: Some("CRATONVM_JIT_OSR_DROP_ORPHANS"), off_key: None, off_word: None, since: "2026-09-26" },
    // gen r5w2/oomjit6: opt-in (default OFF). At every GC point of an
    // optimizing-tier call site, zero the reference homes no later use and no
    // reachable deopt snapshot can read (`jit/src/ir_lower.rs`,
    // `DeadRefSlotClear`), so a dead value in a live IR frame -- above all an
    // OSR body's -- stops being a conservative root.
    E { group: Group::JIT, token: "ir-clear-dead-ref-slots", on_key: Some("CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS"), off_key: None, off_word: None, since: "2026-09-26" },
    // gen r5w3/live7: opt-in (default OFF). The store-free form of the row
    // above: the optimizing tier publishes, per call-site safepoint, which of
    // its reference homes and argument-staging words hold a LIVE reference
    // (`CompiledMethod::frame_liveness`), leaves the dead homes out of the
    // map and the shadow push, and the collector's band scan skips the words
    // the claim proves dead (`conservative_roots::scan_one_frame_filtered`).
    // Oracle: `CRATONVM_DBG_VERIFY_REG_OOP_MAPS=1`.
    E { group: Group::JIT, token: "precise-frame-liveness", on_key: Some("CRATONVM_JIT_PRECISE_FRAME_LIVENESS"), off_key: None, off_word: None, since: "2026-09-26" },
    // gen r5w5/oomjit9: default ON, `=0` is the kill switch. At the first byte
    // of every optimizing-tier call site (inlined ones included), zero the
    // reference homes that are dead there and may still hold a value, and the
    // argument-staging words an earlier call left a reference in
    // (`jit/src/ir_lower.rs`, `plan_dead_home_clears`), so data a program
    // dropped stops being reachable through a live compiled frame's words.
    E { group: Group::JIT, token: "ir-dead-home-clears", on_key: Some("CRATONVM_JIT_IR_DEAD_HOME_CLEARS"), off_key: None, off_word: Some("0"), since: "2026-09-27" },
    // gen r5w6/oomjit10: default ON, `=0` restores the gen r5w5 keep set. The
    // dead-reference rule's keep set becomes a liveness (a colour is killed at
    // its sole occupant's definition), snapshot locals are narrowed by
    // handler-aware bytecode liveness, and phi homes become clearable
    // (`jit/src/ir_lower.rs`, `ir_precise_keep_set_enabled`).
    E { group: Group::JIT, token: "ir-precise-keep-set", on_key: Some("CRATONVM_JIT_IR_PRECISE_KEEP_SET"), off_key: None, off_word: Some("0"), since: "2026-09-27" },
    // gcd d4/o: default ON, `=0` restores the liveness-blind reachability
    // union. Under the precise keep set, a colour defined by a call, an
    // allocation or a load at own bci D is not kept through snapshots reached
    // only through D (`jit/src/ir_lower.rs`, `refine_reach_by_def_kills`).
    E { group: Group::JIT, token: "ir-keep-set-def-kills", on_key: Some("CRATONVM_JIT_IR_KEEP_SET_DEF_KILLS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    // gcd d6/u: default ON, `=0` restores the slot plan's ranges. The
    // dead-reference rule judges a colour's live range by the VALUE uses of
    // its node only: a call or load reused as the next node's memory token
    // no longer keeps its result's home alive until that node
    // (`jit/src/ir_lower.rs`, `value_use_ranges`).
    E { group: Group::JIT, token: "ir-dead-home-value-ranges", on_key: Some("CRATONVM_JIT_IR_DEAD_HOME_VALUE_RANGES"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    // gen r5w6/oomjit10: default ON, `=0` restores the unconditional RAX bit.
    // A single-pass Java invoke's register oop mask leaves RAX out: its CALL
    // target never reads RAX and nothing after the call reads its pre-call
    // value (`jit/src/x64/safepoint.rs`, `live_oop_register_mask`).
    E { group: Group::JIT, token: "rax-dead-at-call", on_key: Some("CRATONVM_JIT_RAX_DEAD_AT_CALL"), off_key: None, off_word: Some("0"), since: "2026-09-27" },
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
    E { group: Group::JIT, token: "precise-coverage-pin", on_key: Some("CRATONVM_PRECISE_COVERAGE_PIN"), off_key: None, off_word: None, since: "2026-06-21" },
    // Wrong-answer A/B lever, not a tuning knob: OFF restores the params-only
    // `run_jit_callee_handler` resume that zeroed a compiled callee's
    // non-parameter locals. See `params_only_callee_handler_frames`.
    E { group: Group::JIT, token: "callee-handler-precise-frame", on_key: None, off_key: Some("CRATONVM_NO_JIT_CALLEE_HANDLER_PRECISE_FRAME"), off_word: None, since: "2026-08-01" },
    E { group: Group::JIT, token: "handler-sink-genuine-receiver", on_key: Some("CRATONVM_JIT_HANDLER_SINK_GENUINE_RECEIVER"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "precise-handler-frames", on_key: None, off_key: Some("CRATONVM_NO_JIT_PRECISE_HANDLER_FRAMES"), off_word: None, since: "2026-07-28" },
    E { group: Group::JIT, token: "precise-inline-frame-record", on_key: None, off_key: Some("CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD"), off_word: None, since: "2026-06-21" },
    E { group: Group::JIT, token: "precise-jit-maps", on_key: None, off_key: Some("CRATONVM_NO_PRECISE_JIT_MAPS"), off_word: None, since: "2026-06-17" },
    E { group: Group::JIT, token: "precise-reg-spill", on_key: None, off_key: Some("CRATONVM_NO_PRECISE_REG_SPILL"), off_word: None, since: "2026-07-11" },
    E { group: Group::JIT, token: "precise-virtual-invokes", on_key: None, off_key: Some("CRATONVM_JIT_NO_PRECISE_VIRTUAL_INVOKES"), off_word: None, since: "2026-07-31" },
    // Guard-dominated bounds-check elimination (`x64::bce::range_bce_enabled`).
    // Default-**OFF**. The gate was presence-parsed (`.is_some()`) until
    // 2026-09-12, when `CRATONVM_JIT_RANGE_BCE=0` *enabled* it; it is now a
    // `runtime_flag_on` read, so `=0` is off. `off_word` still stays `None`:
    // unsetting the key is off under both readings, and a wrong elision here is
    // an out-of-bounds heap write, so the token must not depend on which parser
    // a future edit leaves behind. `-bce` (`CRATONVM_JIT_NO_BCE`) still kills
    // every reason including this one.
    E { group: Group::JIT, token: "range-bce", on_key: Some("CRATONVM_JIT_RANGE_BCE"), off_key: None, off_word: None, since: "2026-08-01" },
    E { group: Group::JIT, token: "range-scan-legacy", on_key: Some("CRATONVM_JIT_RANGE_SCAN_LEGACY"), off_key: None, off_word: None, since: "2026-06-23" },
    E { group: Group::JIT, token: "reassoc", on_key: Some("CRATONVM_JIT_REASSOC"), off_key: None, off_word: None, since: "2026-06-16" },
    E { group: Group::JIT, token: "transform-census", on_key: Some("CRATONVM_JIT_TRANSFORM_CENSUS"), off_key: None, off_word: None, since: "2026-09-16" },
    // Opt-in, and it changes HOT-PATH CODEGEN: two instructions
    // (`mov r10, imm64` + `add qword [r10], 1`) in the method-entry prologue of
    // every single-pass-compiled body, bumping a per-method invocation counter
    // that the VM drains back into the tiered manager's `observed_count`. It
    // exists because `MethodState::invocation_count` has NO writer on the
    // compiled path -- it freezes the moment a C1 body is published, which is
    // exactly the event that closes every interpreter-side counting door, so
    // `should_compile`'s C1->C2 hotness arm is asked a question whose answer
    // stopped moving at roughly the C1 threshold. See
    // `cratonvm_jit::entry_counter`.
    //
    // Default OFF so it can be PRICED before anyone pays for it. "Two
    // instructions" is a defensible cost and an indefensible assumption on a
    // path taken by every compiled invocation in the process: the same
    // prologue's shadow-stack thread fetch was one CALL and cost fib44 ~2.8x
    // until it was NOP'd out for methods that did not need it. The A/B is this
    // token against a baseline run of the same binary.
    // DEFAULT-ON since round 9 wave 3 (measured: IrEscapeProbe 9.7s -> 6.3s,
    // outputs identical), so `0` is the kill switch and `off_word` is "0".
    E { group: Group::JIT, token: "entry-counter", on_key: Some("CRATONVM_JIT_ENTRY_COUNTER"), off_key: None, off_word: Some("0"), since: "2026-09-16" },
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
    E { group: Group::JIT, token: "rootsnap-cache", on_key: Some("CRATONVM_ROOTSNAP_CACHE"), off_key: None, off_word: Some("0"), since: "2026-06-14" },
    E { group: Group::JIT, token: "rootsnap-cache-survive-gc", on_key: Some("CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC"), off_key: None, off_word: Some("0"), since: "2026-06-15" },
    E { group: Group::JIT, token: "safepoint-polls", on_key: Some("CRATONVM_JIT_SAFEPOINT_POLLS"), off_key: None, off_word: Some("0"), since: "2026-07-23" },
    E { group: Group::JIT, token: "safepoint-reg-spill", on_key: Some("CRATONVM_JIT_SAFEPOINT_REG_SPILL"), off_key: None, off_word: None, since: "2026-06-16" },
    E { group: Group::JIT, token: "scalar-deopt", on_key: Some("CRATONVM_SCALAR_DEOPT"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "scalar-new", on_key: Some("CRATONVM_JIT_SCALAR_NEW"), off_key: None, off_word: Some("0"), since: "2026-06-20" },
    E { group: Group::JIT, token: "scalar-replacement", on_key: None, off_key: Some("CRATONVM_DISABLE_SCALAR_REPLACEMENT"), off_word: None, since: "2026-05-20" },
    E { group: Group::JIT, token: "scalar-under-precise-frames", on_key: Some("CRATONVM_JIT_SCALAR_UNDER_PRECISE_FRAMES"), off_key: None, off_word: Some("0"), since: "2026-09-22" },
    E { group: Group::JIT, token: "self-cache-inherit", on_key: None, off_key: Some("CRATONVM_JIT_NO_SELF_CACHE_INHERIT"), off_word: None, since: "2026-07-14" },
    E { group: Group::JIT, token: "atomic-intrinsic", on_key: None, off_key: Some("CRATONVM_JIT_NO_ATOMIC_INTRINSIC"), off_word: None, since: "2026-08-13" },
    E { group: Group::JIT, token: "ffm-intrinsic", on_key: None, off_key: Some("CRATONVM_JIT_NO_FFM_INTRINSIC"), off_word: None, since: "2026-08-26" },
    E { group: Group::JIT, token: "c2-alloc-upgrade", on_key: Some("CRATONVM_JIT_C2_ALLOC_UPGRADE"), off_key: None, off_word: None, since: "2026-08-27" },
    E { group: Group::JIT, token: "ir-inline", on_key: Some("CRATONVM_JIT_IR_INLINE"), off_key: None, off_word: Some("0"), since: "2026-08-27" },
    E { group: Group::JIT, token: "ir-fused-fcmp", on_key: Some("CRATONVM_JIT_IR_FUSED_FCMP"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    E { group: Group::JIT, token: "ir-zgc-announce", on_key: Some("CRATONVM_JIT_IR_ZGC_ANNOUNCE"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    E { group: Group::JIT, token: "arraylist-intrinsics", on_key: None, off_key: Some("CRATONVM_NO_JIT_ARRAYLIST_INTRINSICS"), off_word: None, since: "2026-09-18" },
    E { group: Group::JIT, token: "field-site-cache", on_key: Some("CRATONVM_JIT_FIELD_SITE_CACHE"), off_key: None, off_word: Some("0"), since: "2026-08-04" },
    E { group: Group::JIT, token: "cast-site-cache", on_key: None, off_key: Some("CRATONVM_JIT_NO_CAST_SITE_CACHE"), off_word: None, since: "2026-08-18" },
    // `cast-site-namespaced-arrays` — the per-thread cast / `anewarray` site
    // tables admit an ARRAY answer resolved from a user loader's class when
    // its element answer is covered (`opcodes::namespaced_array_answer_admitted`,
    // interpreter round i1 wave 43, lane L5; proposal i41-L5).
    E { group: Group::JIT, token: "cast-site-namespaced-arrays", on_key: Some("CRATONVM_JIT_CAST_SITE_NAMESPACED_ARRAYS"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    // Default-ON kill switch: `CRATONVM_JIT=-negative-cast-memo` withdraws the
    // `instanceof` negative receiver memo (`CastSite::negative_receivers`).
    E { group: Group::JIT, token: "negative-cast-memo", on_key: None, off_key: Some("CRATONVM_JIT_NO_NEGATIVE_CAST_MEMO"), off_word: None, since: "2026-09-23" },
    // `indy-callsite-cache` — the per-instruction cache of a linked non-JDK
    // `invokedynamic` CallSite (invokedynamic.rs, `bootstrap_generic`). Unset
    // caches `ConstantCallSite`s and records failures; `all` also caches
    // mutable sites; `0` restores re-bootstrapping on every execution.
    E { group: Group::JIT, token: "indy-callsite-cache", on_key: Some("CRATONVM_INDY_CALLSITE_CACHE"), off_key: None, off_word: Some("0"), since: "2026-09-23" },
    // Default-ON kill switch: `CRATONVM_JIT=-execute-sync` stops
    // `interpreter::execute` from taking an `ACC_SYNCHRONIZED` method's monitor
    // itself. Presence-parsed.
    E { group: Group::JIT, token: "execute-sync", on_key: None, off_key: Some("CRATONVM_NO_EXECUTE_SYNC"), off_word: None, since: "2026-09-23" },
    // Default-ON kill switch (every mode since 2026-09-25, after the
    // `--compatible` census): a registered native that answers an
    // `ACC_SYNCHRONIZED` method runs under that method's monitor, and such
    // native entries stay out of the inline cache (`interpreter/invoke.rs`,
    // `native_sync_enabled`). `CRATONVM_JIT=-native-sync` writes `0`, which
    // disarms it; `1` / `true` re-arms it.
    E { group: Group::JIT, token: "native-sync", on_key: Some("CRATONVM_NATIVE_SYNC"), off_key: None, off_word: Some("0"), since: "2026-09-23" },
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
    // 2026-09-08. Opt-in restore of the pre-door refusal of `synchronized`
    // callees. That refusal was the single largest reason any fast door
    // declined anything -- 888,105 of 2.47 M interpreted calls on a
    // `java.text` collator workload, 36% of them -- because ICU's normaliser
    // drives `StringBuffer` one character at a time. See
    // `invoke_fast::door_sync_enabled`.
    E { group: Group::JIT, token: "door-sync", on_key: None, off_key: Some("CRATONVM_JIT_NO_DOOR_SYNC"), off_word: None, since: "2026-09-08" },
    // 2026-09-11. Off restores the per-call `resolve_method_ref` that every
    // cached-native invoke used to pay for two facts that are constants of
    // the call site: each parameter's descriptor tag and the return tag.
    // See `CachedInvokeTarget::Native::facts`.
    E { group: Group::JIT, token: "cached-native-facts", on_key: None, off_key: Some("CRATONVM_JIT_NO_CACHED_NATIVE_FACTS"), off_word: None, since: "2026-09-11" },
    // `frame-emplace` — off builds the frame on the Rust stack and moves it
    // into the slot instead of constructing it there.
    E { group: Group::JIT, token: "frame-emplace", on_key: None, off_key: Some("CRATONVM_JIT_NO_FRAME_EMPLACE"), off_word: None, since: "2026-09-03" },
    // `locals-slab` — off builds every cached-install frame's locals and
    // operand stack from the thread pools instead of one window of its frame
    // stack's slot slab (stage 1 of the contiguous interpreter stack,
    // interpreter round i1 wave 29 lane L7). See `runtime::slot_slab`.
    E { group: Group::JIT, token: "locals-slab", on_key: None, off_key: Some("CRATONVM_JIT_NO_LOCALS_SLAB"), off_word: None, since: "2026-09-30" },
    // `overlap-args` — DEFAULT-OFF while it is measured: the interpreter's
    // fast invoke doors lay a callee's locals over the arguments its caller
    // pushed (stage 2 of the contiguous interpreter stack, argument overlap,
    // interpreter round i1 wave 37 lane L7). Read once per VM into
    // `VmConfig::overlap_interpreter_args`. See
    // `FrameStack::push_cached_compact_overlapping`.
    E { group: Group::JIT, token: "overlap-args", on_key: Some("CRATONVM_JIT_OVERLAP_ARGS"), off_key: None, off_word: None, since: "2026-10-01" },
    E { group: Group::JIT, token: "bg-decline-redefined", on_key: Some("CRATONVM_JIT_BG_DECLINE_REDEFINED"), off_key: None, off_word: None, since: "2026-10-02" },
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
    // `resolution-failure-record` — off restores re-resolving a CONSTANT_Class /
    // CONSTANT_Dynamic entry whose resolution already failed (JVMS 5.4.3 says
    // rethrow the recorded error). Presence-parsed.
    E { group: Group::LOADER, token: "resolution-failure-record", on_key: None, off_key: Some("CRATONVM_LOADER_NO_RESOLUTION_FAILURE_RECORD"), off_word: None, since: "2026-09-23" },
    // `subtype-display` — off makes `Class::is_subclass_of` walk the whole
    // supertype DAG for every class target instead of answering from the
    // per-class primary-supers display. Presence-parsed.
    E { group: Group::LOADER, token: "subtype-display", on_key: None, off_key: Some("CRATONVM_LOADER_NO_SUBTYPE_DISPLAY"), off_word: None, since: "2026-09-23" },
    E { group: Group::JIT, token: "ldc-const-cache", on_key: None, off_key: Some("CRATONVM_JIT_NO_LDC_CONST_CACHE"), off_word: None, since: "2026-08-18" },
    E { group: Group::JIT, token: "ir-unresumable-trap-guard", on_key: Some("CRATONVM_JIT_IR_UNRESUMABLE_TRAP_GUARD"), off_key: None, off_word: Some("0"), since: "2026-08-21" },
    E { group: Group::JIT, token: "compiled-ldc-const-cache", on_key: Some("CRATONVM_JIT_COMPILED_LDC_CONST_CACHE"), off_key: None, off_word: Some("0"), since: "2026-08-20" },
    E { group: Group::JIT, token: "new-site-cache", on_key: None, off_key: Some("CRATONVM_JIT_NO_NEW_SITE_CACHE"), off_word: None, since: "2026-08-17" },
    E { group: Group::JIT, token: "site-cache", on_key: Some("CRATONVM_JIT_SITE_CACHE"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::JIT, token: "unreg-memo-hiwater", on_key: Some("CRATONVM_JIT_UNREG_MEMO_HIWATER"), off_key: None, off_word: Some("0"), since: "2026-08-05" },
    // `field-site-cache-loader` — default-ON since interpreter round i1 wave
    // 25 (lane L5); `0` / `-field-site-cache-loader` is the kill switch, read
    // with `field-site-cache`'s spelling (`field_access.rs`).
    E { group: Group::JIT, token: "field-site-cache-loader", on_key: Some("CRATONVM_JIT_FIELD_SITE_CACHE_LOADER"), off_key: None, off_word: Some("0"), since: "2026-08-04" },
    E { group: Group::JIT, token: "field-site-slots", on_key: Some("CRATONVM_JIT_FIELD_SITE_SLOTS"), off_key: None, off_word: None, since: "2026-08-05" },
    E { group: Group::JIT, token: "method-site-cache", on_key: Some("CRATONVM_JIT_METHOD_SITE_CACHE"), off_key: None, off_word: None, since: "2026-08-04" },
    E { group: Group::JIT, token: "loop-work-tierup", on_key: Some("CRATONVM_JIT_LOOP_WORK_TIERUP"), off_key: None, off_word: None, since: "2026-08-04" },
    E { group: Group::JIT, token: "shadow-nopush", on_key: Some("CRATONVM_SHADOW_NOPUSH"), off_key: None, off_word: None, since: "2026-06-04" },
    E { group: Group::JIT, token: "sync-methods", on_key: Some("CRATONVM_JIT_SYNC_METHODS"), off_key: None, off_word: None, since: "2026-08-04" },
    // Round 11 wave 10 (lane synccall): a compiled caller holds the class monitor
    // around a direct CALL to a closed static synchronized callee. DEFAULT-ON.
    E { group: Group::JIT, token: "sync-direct", on_key: Some("CRATONVM_JIT_SYNC_DIRECT"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    E { group: Group::JIT, token: "sp-sync-direct-instance", on_key: Some("CRATONVM_JIT_SP_SYNC_DIRECT_INSTANCE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
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
    // gcd d2/f (found by JIT round 13): default ON. Once any shadow push has
    // bailed on that guard, its oops are published nowhere, so every later
    // moving young cycle is refused (`vm/src/jit/conservative_roots.rs`,
    // `shadow_bail_refuses_moving`). `=0` restores the old, unsound trust.
    E { group: Group::JIT, token: "shadow-bail-blocks-moving", on_key: Some("CRATONVM_JIT_SHADOW_BAIL_BLOCKS_MOVING"), off_key: None, off_word: Some("0"), since: "2026-09-27" },
    // gcd d3/o: opt-in (default OFF). Since d3/o the refusal above is per
    // thread (only while a bailed push may still be live on a proving
    // thread's shadow stack); `=1` restores d2/f's process-wide sticky
    // refusal of every cycle after the first bail.
    E { group: Group::JIT, token: "shadow-bail-sticky", on_key: Some("CRATONVM_JIT_SHADOW_BAIL_STICKY"), off_key: None, off_word: None, since: "2026-09-27" },
    // gcd d2/f: opt-in (default OFF). The single-pass prologue zeroes its
    // operand-spill band so a returned frame's leftover reference is not a
    // root of the next frame at that address (`jit/src/x64/frames.rs`,
    // `emit_prologue`).
    E { group: Group::JIT, token: "sp-zero-spill-band", on_key: Some("CRATONVM_JIT_SP_ZERO_SPILL_BAND"), off_key: None, off_word: None, since: "2026-09-27" },
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
    E { group: Group::JIT, token: "ir-frame-block-max-homes", on_key: Some("CRATONVM_JIT_IR_FRAME_BLOCK_MAX_HOMES"), off_key: None, off_word: None, since: "2026-09-29" },
    E { group: Group::JIT, token: "ic-grace-handshake-ms", on_key: Some("CRATONVM_JIT_IC_GRACE_HANDSHAKE_MS"), off_key: None, off_word: None, since: "2026-09-29" },
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
    // Compile-worker counts for the tiered manager's two lanes
    // (`tiered::compiler_thread_counts`): C1 defaults to 1 worker, C2 to
    // max(1, log2(cpus)); either is clamped to 1..=64.
    E { group: Group::JIT, token: "tier-c1-threads", on_key: Some("CRATONVM_TIER_C1_THREADS"), off_key: None, off_word: None, since: "2026-09-12" },
    E { group: Group::JIT, token: "tier-c1-threshold", on_key: Some("CRATONVM_TIER_C1_THRESHOLD"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "tier-c2-min-invocations", on_key: Some("CRATONVM_TIER_C2_MIN_INVOCATIONS"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "tier-c2-threads", on_key: Some("CRATONVM_TIER_C2_THREADS"), off_key: None, off_word: None, since: "2026-09-12" },
    E { group: Group::JIT, token: "tier-c2-threshold", on_key: Some("CRATONVM_TIER_C2_THRESHOLD"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "tier-osr-backedge", on_key: Some("CRATONVM_TIER_OSR_BACKEDGE"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "tier-osr-threshold", on_key: Some("CRATONVM_TIER_OSR_THRESHOLD"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "tier-pgo", on_key: Some("CRATONVM_TIER_PGO"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "tier-pgo-receivers", on_key: Some("CRATONVM_TIER_PGO_RECEIVERS"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "tier-pgo-c2-window", on_key: Some("CRATONVM_TIER_PGO_C2_WINDOW"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    // `tier-pgo` turns profiling on for a WINDOW; this turns it on for the
    // whole run, which is what a branch profile needs to be worth reading at
    // the moment a method tiers up. Affordable since the counters went
    // lock-free (measured 0.6% on an always-on run), and default OFF only
    // until that measurement has been repeated on a soak.
    // Presence-tested (`runtime_var_os(..).is_some()`), so any value turns it on.
    E { group: Group::JIT, token: "tier-pgo-always", on_key: Some("CRATONVM_TIER_PGO_ALWAYS"), off_key: None, off_word: None, since: "2026-09-09" },
    E { group: Group::JIT, token: "tiered", on_key: Some("CRATONVM_TIER_ENABLED"), off_key: None, off_word: Some("0"), since: "2026-06-22" },
    E { group: Group::JIT, token: "tlab-zero-elision", on_key: None, off_key: Some("CRATONVM_NO_JIT_TLAB_ZERO_ELISION"), off_word: None, since: "2026-07-30" },
    // Interpreter-side, but it lives with the execution-engine knobs like
    // `rootsnap-cache`. Default-ON; `0`/`off`/`false`/`no` is the kill switch.
    E { group: Group::JIT, token: "trivial-getter", on_key: Some("CRATONVM_TRIVIAL_GETTER"), off_key: None, off_word: Some("0"), since: "2026-07-30" },
    E { group: Group::JIT, token: "unroll", on_key: Some("CRATONVM_JIT_UNROLL"), off_key: Some("CRATONVM_DISABLE_UNROLL"), off_word: None, since: "2026-05-22" },
    // Vectorized emission. Default-ON since 2026-09-18 (round 9 wave 4, priced
    // in NOTES-w4-x64core4.md) for the single-pass SIMD detectors
    // (`x64::simd_analysis::simd_sum_forms_enabled`, `runtime_flag_default_on`);
    // `0`/`off`/`false`/`no` is the kill switch.
    E { group: Group::JIT, token: "vectorize", on_key: Some("CRATONVM_JIT_VECTORIZE"), off_key: None, off_word: Some("0"), since: "2026-08-01" },
    // IR verifier lanes. `verify-ir` is the only *tri-state* knob in the table:
    // unset means "follow the build profile" (on under `debug_assertions`), an
    // explicit `1` forces it on in a release build, and an explicit `0` is also
    // the kill switch for the unconditional pre-lowering check. `off_word: "0"`
    // is what makes `CRATONVM_JIT=-verify-ir` reach that third state instead of
    // merely unsetting the variable back to the profile default.
    E { group: Group::JIT, token: "verify-arena-order", on_key: Some("CRATONVM_JIT_VERIFY_ARENA_ORDER"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::JIT, token: "verify-frame-states", on_key: Some("CRATONVM_JIT_VERIFY_FRAME_STATES"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::JIT, token: "verify-ir", on_key: Some("CRATONVM_JIT_VERIFY_IR"), off_key: None, off_word: Some("0"), since: "2026-07-31" },
    // Tri-state like `verify-ir`: forced on/off, or unset = follow the master
    // gate (debug builds, after build and after optimize). Round 9 wave 2.
    E { group: Group::JIT, token: "verify-branches", on_key: Some("CRATONVM_JIT_VERIFY_BRANCHES"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    E { group: Group::JIT, token: "verify-memory-chain", on_key: Some("CRATONVM_JIT_VERIFY_MEMORY_CHAIN"), off_key: None, off_word: None, since: "2026-07-31" },
    // Compatibility alias: `check_schedule` was split into the memory-chain and
    // arena-order lanes, and this token still seeds both when neither is set.
    E { group: Group::JIT, token: "lambda-adapter", on_key: Some("CRATONVM_JIT_LAMBDA_ADAPTER"), off_key: None, off_word: Some("0"), since: "2026-08-18" },
    E { group: Group::JIT, token: "lambda-capture-adapter", on_key: Some("CRATONVM_JIT_LAMBDA_CAPTURE_ADAPTER"), off_key: None, off_word: Some("0"), since: "2026-08-18" },
    E { group: Group::JIT, token: "lambda-const-probe", on_key: Some("CRATONVM_JIT_LAMBDA_CONST_PROBE"), off_key: None, off_word: None, since: "2026-08-23" },
    E { group: Group::JIT, token: "fjp-subclass-blocklist", on_key: Some("CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST"), off_key: None, off_word: None, since: "2026-08-26" },
    E { group: Group::JIT, token: "lambda-site", on_key: Some("CRATONVM_JIT_LAMBDA_SITE"), off_key: None, off_word: Some("0"), since: "2026-08-17" },
    E { group: Group::JIT, token: "lambda-tierup", on_key: Some("CRATONVM_JIT_LAMBDA_TIERUP"), off_key: None, off_word: Some("0"), since: "2026-08-17" },
    E { group: Group::JIT, token: "verify-schedule", on_key: Some("CRATONVM_JIT_VERIFY_SCHEDULE"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::JIT, token: "verify-types", on_key: Some("CRATONVM_JIT_VERIFY_TYPES"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::JIT, token: "virtual-tierup", on_key: Some("CRATONVM_JIT_VIRTUAL_TIERUP"), off_key: None, off_word: Some("0"), since: "2026-06-14" },
    E { group: Group::JIT, token: "xt-helper-window-discharge", on_key: Some("CRATONVM_XT_HELPER_WINDOW_DISCHARGE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "xt-pinned-peer-depth", on_key: Some("CRATONVM_XT_PINNED_PEER_DEPTH"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "xt-pinned-peer-publish-only", on_key: Some("CRATONVM_XT_PINNED_PEER_PUBLISH_ONLY"), off_key: None, off_word: None, since: "2026-09-02" },
    // Credit the pinned-peer depth even on a collector that cannot honour the
    // pin. Default OFF, which is the corrected behaviour; setting it restores
    // the ten-second H2 SIGSEGV, so the fix has a positive control rather than
    // only an absence of crashes.
    E { group: Group::JIT, token: "xt-pinned-peer-unpinnable", on_key: Some("CRATONVM_XT_PINNED_PEER_UNPINNABLE"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "xt-peer-shadow-scan", on_key: Some("CRATONVM_XT_PEER_SHADOW_SCAN"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    // gcd d2/i: a peer blocked in the compiled `monitorenter` helper answers
    // with its blocking deposit's coverage proof instead of as a helper
    // window. Opt-in (default OFF).
    E { group: Group::JIT, token: "xt-blocked-monitor-proof", on_key: Some("CRATONVM_XT_BLOCKED_MONITOR_PROOF"), off_key: None, off_word: None, since: "2026-09-27" },
    E { group: Group::JIT, token: "dbg-stale-frame-words", on_key: Some("CRATONVM_DBG_STALE_FRAME_WORDS"), off_key: None, off_word: None, since: "2026-09-03" },
    E { group: Group::JIT, token: "pin-unnamed-frame-refs", on_key: Some("CRATONVM_JIT_PIN_UNNAMED_FRAME_REFS"), off_key: None, off_word: None, since: "2026-09-03" },
    E { group: Group::JIT, token: "remap-unmapped-dupes", on_key: Some("CRATONVM_JIT_REMAP_UNMAPPED_DUPES"), off_key: None, off_word: None, since: "2026-09-03" },
    E { group: Group::GC, token: "zgc-jit-blanket-refusal", on_key: Some("CRATONVM_ZGC_JIT_BLANKET_REFUSAL"), off_key: None, off_word: None, since: "2026-09-03" },
    // Default-ON since 2026-09-05, so it takes an `off_word`: presence alone no
    // longer decides it and `=0` has to be able to turn it off.
    E { group: Group::JIT, token: "local-mask-unreached-fail-closed", on_key: Some("CRATONVM_JIT_LOCAL_MASK_UNREACHED_FAIL_CLOSED"), off_key: None, off_word: Some("0"), since: "2026-09-03" },
    // Default-ON kill switch as of 2026-09-08, hence `off_key` only: the JIT
    // half of a blocked-region wake (active compiled frames, register images,
    // shadow stack). It shipped opt-in and wired only into the LEAKED-region
    // fallback, so the wake that actually runs remapped interpreter frames and
    // nothing compiled -- a peer that blocked with compiled frames below it
    // resumed with every JIT oop at its pre-move address.
    E { group: Group::GC, token: "blocked-wake-jit-remap", on_key: None, off_key: Some("CRATONVM_NO_BLOCKED_WAKE_JIT_REMAP"), off_word: None, since: "2026-09-03" },
    // Default-ON kill switch, hence `off_key` only: a blocked peer's
    // conservatively-scanned native-stack words are written back on wake. The
    // objects were kept alive AND relocated while it slept, nothing else
    // rewrites those words, and the pin that nominally protected them is a
    // no-op on a Cheney copy.
    E { group: Group::GC, token: "blocked-peer-stack-remap", on_key: None, off_key: Some("CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP"), off_word: None, since: "2026-09-07" },
    E { group: Group::JIT, token: "xt-keep-unrewritable-on-discharge", on_key: Some("CRATONVM_XT_KEEP_UNREWRITABLE_ON_DISCHARGE"), off_key: None, off_word: None, since: "2026-09-04" },
    E { group: Group::GC, token: "zgc-unrewritable-peer-refuses", on_key: Some("CRATONVM_ZGC_UNREWRITABLE_PEER_REFUSES"), off_key: None, off_word: None, since: "2026-09-04" },
    // Default-ON since 2026-09-08. It shipped opt-in, and the helper-window
    // DISCHARGE (default-on, same family) then made the pin load-bearing: a
    // discharged cycle relocates on the strength of "every window pinned", and
    // `is_heap_addr` -- the predicate it used to pin with -- drops a misaligned
    // interior pointer and a one-past-the-end cursor, which are the two shapes a
    // compiled loop leaves in a frozen peer's registers. `0` is the kill switch.
    E { group: Group::JIT, token: "xt-helper-window-pin-resolve", on_key: Some("CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    // Root a frozen peer's derived pointers (interior / one-past-the-end
    // cursors) in the TAKE-OVER pass, not only exact object bases. Shipped
    // opt-in by gc-common w1-c; DEFAULT-ON since w2-c (same day), so a kill
    // switch with `off_word: "0"`. Argument on the page
    // docs/internal/gc-common-round-20260923/common-c-takeover-probe-drops-derived-pointers-FIXED-20260923.md.
    E { group: Group::JIT, token: "xt-takeover-interior", on_key: Some("CRATONVM_XT_TAKEOVER_INTERIOR"), off_key: None, off_word: Some("0"), since: "2026-09-23" },
    // gcd d10/t (`common-c-linux-takeover-signals-every-thread-FIXED-20260929`): opt-in, the
    // Linux take-over signals only roster threads whose published JIT depth is
    // not zero (a thread with no compiled frame cannot be in compiled code),
    // for the first passes of a take-over; later passes signal the whole
    // roster. `CRATONVM_XT_ROOT_SCAN_AUDIT=1` checks the rule either way
    // (`JIT-ONLY SIGNAL MISS`).
    E { group: Group::JIT, token: "xt-takeover-signal-jit-only", on_key: Some("CRATONVM_XT_TAKEOVER_SIGNAL_JIT_ONLY"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "xt-helper-window-pin", on_key: Some("CRATONVM_XT_HELPER_WINDOW_PIN"), off_key: None, off_word: None, since: "2026-09-01" },
    E { group: Group::JIT, token: "xt-helper-window-scan", on_key: Some("CRATONVM_XT_HELPER_WINDOW_SCAN"), off_key: None, off_word: None, since: "2026-07-02" },
    // Kill switch for the `process_vm_readv` peer-stack reader, so the reader
    // and the historical direct load are A/B-able inside one binary. Opt-in:
    // setting it restores the pre-fix behaviour exactly, SIGSEGV included.
    E { group: Group::JIT, token: "xt-no-safe-peer-read", on_key: Some("CRATONVM_XT_NO_SAFE_PEER_READ"), off_key: None, off_word: None, since: "2026-09-08" },
    E { group: Group::JIT, token: "xt-jit-root-scan", on_key: Some("CRATONVM_XT_JIT_ROOT_SCAN"), off_key: None, off_word: None, since: "2026-06-23" },
    // Verification-only, and expensive on purpose: re-walks the WHOLE system
    // thread table on every take-over pass to prove the process-local roster
    // `take_over_pass` uses did not miss a thread that was in compiled code.
    // Kept off the `xt-jit-root-scan` debug token deliberately — that walk is
    // the ~83 ms/pass cost the roster removed, so bundling the two would make
    // the scan impossible to observe without reintroducing what it fixed.
    E { group: Group::JIT, token: "xt-root-scan-audit", on_key: Some("CRATONVM_XT_ROOT_SCAN_AUDIT"), off_key: None, off_word: None, since: "2026-09-10" },
    // Value token, milliseconds: `CRATONVM_JIT=xt-peer-deadline-ms=50`. Unset
    // means the built-in 20 ms, and `0` is rejected by the parser's own filter,
    // so there is no off state to spell.
    E { group: Group::JIT, token: "xt-peer-deadline-ms", on_key: Some("CRATONVM_XT_PEER_DEADLINE_MS"), off_key: None, off_word: None, since: "2026-08-01" },
    E { group: Group::JIT, token: "xt-peer-total-ms", on_key: Some("CRATONVM_XT_PEER_TOTAL_MS"), off_key: None, off_word: None, since: "2026-08-05" },
    // gce e2/t: wait this many microseconds for cooperative arrivals before the
    // first take-over pass; a met quota runs one pass over the uncounted roster.
    E { group: Group::JIT, token: "xt-first-pass-grace-us", on_key: Some("CRATONVM_XT_FIRST_PASS_GRACE_US"), off_key: None, off_word: None, since: "2026-09-29" },
    // gce e2/t: the Linux parked take-over peer spins for the whole pause again
    // (the A/B of its futex sleep).
    E { group: Group::JIT, token: "xt-parked-spin-only", on_key: Some("CRATONVM_XT_PARKED_SPIN_ONLY"), off_key: None, off_word: None, since: "2026-09-29" },
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
    // gcd d9/d: default ON, `=0` restores the service copy. A baked
    // single-pass direct `invokestatic` into a body whose method has no
    // exception table keeps no copy of its arguments: they are the callee's,
    // so a dropped argument is not kept alive by the caller for the whole call
    // (`jit/src/x64/op_invoke.rs`, `sp_direct_call_args_owned_by_callee`;
    // `vm/src/jit/helpers.rs`, `drop_declined_callee_stash`). gcd d10/f: also
    // a statically bound instance call (invoke kind 1), and only where the
    // caller's bytecode before the call commits nothing, so the rare hand-off
    // (a re-run of the caller from entry) never commits a side effect twice.
    E { group: Group::JIT, token: "callee-owned-args", on_key: Some("CRATONVM_JIT_CALLEE_OWNED_ARGS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    // gce e1/f: default-ON kill switches (`=0` restores the old behaviour): a
    // not-entrant re-dispatch no longer re-runs the callee's handler or entry,
    // and the owned-args hand-off records its real replay verdict
    // (`vm/src/jit/helpers.rs`).
    E { group: Group::JIT, token: "not-entrant-escape", on_key: Some("CRATONVM_JIT_NOT_ENTRANT_ESCAPE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "owned-args-handoff-verdict", on_key: Some("CRATONVM_JIT_OWNED_ARGS_HANDOFF_VERDICT"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "merge-marks-exact", on_key: Some("CRATONVM_JIT_MERGE_MARKS_EXACT"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "inline-oop-coverage", on_key: Some("CRATONVM_JIT_INLINE_OOP_COVERAGE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "loader-blind-cp-resolve", on_key: Some("CRATONVM_JIT_LOADER_BLIND_CP_RESOLVE"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::JIT, token: "local-mask-fail-closed", on_key: Some("CRATONVM_JIT_LOCAL_MASK_FAIL_CLOSED"), off_key: None, off_word: Some("0"), since: "2026-09-03" },
    E { group: Group::JIT, token: "wide-local-oop-maps", on_key: Some("CRATONVM_JIT_WIDE_LOCAL_OOP_MAPS"), off_key: None, off_word: Some("0"), since: "2026-09-03" },
    // `arm64` is opt-in: the aarch64 backend compiles nothing unless it is set,
    // because nothing in this repository has proven that backend on hardware.
    E { group: Group::JIT, token: "arm64", on_key: Some("CRATONVM_JIT_ARM64"), off_key: None, off_word: None, since: "2026-09-12" },
    E { group: Group::JIT, token: "arm64-safepoints", on_key: Some("CRATONVM_JIT_ARM64_SAFEPOINTS"), off_key: None, off_word: Some("0"), since: "2026-09-03" },
    E { group: Group::JIT, token: "ir-gc-point-maps", on_key: Some("CRATONVM_JIT_IR_GC_POINT_MAPS"), off_key: None, off_word: Some("0"), since: "2026-08-30" },
    E { group: Group::JIT, token: "zero-spid", on_key: Some("CRATONVM_JIT_ZERO_SPID"), off_key: None, off_word: Some("0"), since: "2026-08-30" },
    // Profiler and debugger symbol sinks for JIT code, all opt-in: `perf-map`
    // writes perf-<pid>.map, `jitdump` writes jit-<pid>.dump (Linux), `gdb` feeds
    // the GDB JIT interface (Linux). See `jit/src/code_events.rs`.
    E { group: Group::JIT, token: "perf-map", on_key: Some("CRATONVM_JIT_PERF_MAP"), off_key: None, off_word: None, since: "2026-09-12" },
    E { group: Group::JIT, token: "jitdump", on_key: Some("CRATONVM_JIT_JITDUMP"), off_key: None, off_word: None, since: "2026-09-12" },
    E { group: Group::JIT, token: "gdb", on_key: Some("CRATONVM_JIT_GDB"), off_key: None, off_word: None, since: "2026-09-12" },
    // The two JIT-runtime sensors, both off by default and both for a lane
    // rather than for production. `helper-panics-fatal` turns a CONTAINED
    // helper panic — a silent wrong answer: a dropped store, an `instanceof`
    // answering 0 — into an abort at the first one, with the panicking helper
    // still on the stack (`vm/src/jit/helper_guard.rs`).
    // `assert-code-free-audit` panics at VM shutdown if any published compiled
    // body was unmapped outside the retirement queue, which three documents
    // call a must-be-zero invariant and nothing whole-VM checked (`vm-cli`'s
    // `maybe_dump_shutdown_reports`).
    E { group: Group::JIT, token: "helper-panics-fatal", on_key: Some("CRATONVM_JIT_HELPER_PANICS_FATAL"), off_key: None, off_word: None, since: "2026-09-17" },
    E { group: Group::JIT, token: "assert-code-free-audit", on_key: Some("CRATONVM_JIT_ASSERT_CODE_FREE_AUDIT"), off_key: None, off_word: None, since: "2026-09-17" },
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
    E { group: Group::GC, token: "compact-ref-fields", on_key: Some("CRATONVM_COMPACT_REF_FIELDS"), off_key: None, off_word: Some("0"), since: "2026-06-22" },
    E { group: Group::GC, token: "pack-fields-by-width", on_key: Some("CRATONVM_PACK_FIELDS_BY_WIDTH"), off_key: None, off_word: Some("0"), since: "2026-08-06" },
    E { group: Group::GC, token: "layout-scan-cache", on_key: Some("CRATONVM_GC_LAYOUT_SCAN_CACHE"), off_key: None, off_word: Some("0"), since: "2026-09-03" },
    E { group: Group::GC, token: "compressed-oops", on_key: Some("CRATONVM_COMPRESSED_OOPS"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::GC, token: "default-heap-ergonomics", on_key: Some("CRATONVM_DEFAULT_HEAP_ERGONOMICS"), off_key: None, off_word: Some("0"), since: "2026-06-17" },
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
    // gc-common w3-g (2026-09-23): G1 refuses evacuation when the one take-over
    // licence says `NonMoving` (`gc_quiescence::g1_takeover_licence_refusal`).
    // Opt-in until its refusal rate is measured.
    E { group: Group::GC, token: "g1-takeover-licence", on_key: Some("CRATONVM_G1_TAKEOVER_LICENCE"), off_key: None, off_word: None, since: "2026-09-23" },
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
    // 2026-09-11. The fourth of four routes to a direct compiled entry, and
    // the only one still refusing on the callee's exception table alone. See
    // `env_cache::jit_virtual_promote_handler_callee`.
    E { group: Group::JIT, token: "virtual-promote-handler-callee", on_key: Some("CRATONVM_JIT_VIRTUAL_PROMOTE_HANDLER_CALLEE"), off_key: None, off_word: None, since: "2026-09-11" },
    E { group: Group::JIT, token: "native-cf-postcomplete-skip", on_key: Some("CRATONVM_NATIVE_CF_POSTCOMPLETE_SKIP"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::JIT, token: "native-cf-postcomplete-direct", on_key: Some("CRATONVM_NATIVE_CF_POSTCOMPLETE_DIRECT"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    // --- composition residuals, part two: 2026-09-11 -------------------------
    // `composition-native-callback-and-the-promotion-question-20260902.md`.
    // Item 1's mechanism and its engagement census; item 2's second census,
    // the one that names why a NOMINATED site still refuses to promote.
    E { group: Group::JIT, token: "native-callback-memo", on_key: Some("CRATONVM_NATIVE_CALLBACK_MEMO"), off_key: None, off_word: Some("0"), since: "2026-09-11" },
    E { group: Group::JIT, token: "native-callback-memo-static", on_key: Some("CRATONVM_NATIVE_CALLBACK_MEMO_STATIC"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::DBG, token: "callback-memo", on_key: Some("CRATONVM_DBG_CALLBACK_MEMO"), off_key: None, off_word: None, since: "2026-09-11" },
    E { group: Group::DBG, token: "promote-refuse", on_key: Some("CRATONVM_DBG_PROMOTE_REFUSE"), off_key: None, off_word: None, since: "2026-09-11" },
    // The JIT per-call-site native cache used to refuse every
    // capability-classified triple outright, which since 836631dcc (leaf-only
    // -> every registered native) has meant refusing `jdk/internal/misc/Unsafe`
    // -- 99 428 of the 100 805 general-resolver calls on the composition probe.
    // The gate now runs on the dispatch side instead, where the funnel runs it;
    // `=0` restores the refusal.
    E { group: Group::JIT, token: "site-cache-capability", on_key: Some("CRATONVM_JIT_SITE_CACHE_CAPABILITY"), off_key: None, off_word: Some("0"), since: "2026-09-11" },
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
    // Lane W2-C, 2026-09-20. All five OPT-IN (`=1`): each moves a marking-cycle
    // policy number or a per-object cost, and this round moves a default on a
    // measurement rather than on an argument. See the field docs in
    // `types/src/flags.rs` and the A/B recipes on their consumers in `gc/src/g1.rs`.
    E { group: Group::GC, token: "g1-ihop-gross-growth", on_key: Some("CRATONVM_G1_IHOP_GROSS_GROWTH"), off_key: None, off_word: None, since: "2026-09-20" },
    E { group: Group::GC, token: "g1-mark-root-screen", on_key: Some("CRATONVM_G1_MARK_ROOT_SCREEN"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::GC, token: "g1-band-reject-pins", on_key: Some("CRATONVM_G1_BAND_REJECT_PINS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::GC, token: "g1-band-reject-pins-all", on_key: Some("CRATONVM_G1_BAND_REJECT_PINS_ALL"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::GC, token: "g1-pins-honour-unregistered-frame", on_key: Some("CRATONVM_G1_PINS_HONOUR_UNREGISTERED_FRAME"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::GC, token: "g1-cleanup-honours-jit-pins", on_key: Some("CRATONVM_G1_CLEANUP_HONOURS_JIT_PINS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::GC, token: "g1-remark-seeds-jit-pins", on_key: Some("CRATONVM_G1_REMARK_SEEDS_JIT_PINS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::GC, token: "g1-pins-honour-any-publication", on_key: Some("CRATONVM_G1_PINS_HONOUR_ANY_PUBLICATION"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::GC, token: "g1-overflow-rescan-eden-bitmap", on_key: Some("CRATONVM_G1_OVERFLOW_RESCAN_EDEN_BITMAP"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::GC, token: "g1-mark-gray-requires-header-flag", on_key: Some("CRATONVM_G1_MARK_GRAY_REQUIRES_HEADER_FLAG"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::GC, token: "g1-mark-cap-seed-screen", on_key: Some("CRATONVM_G1_MARK_CAP_SEED_SCREEN"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::GC, token: "g1-evac-headroom-trigger", on_key: Some("CRATONVM_G1_EVAC_HEADROOM_TRIGGER"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::GC, token: "g1-rset-dead-holder-filter", on_key: Some("CRATONVM_G1_RSET_DEAD_HOLDER_FILTER"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::GC, token: "g1-cleanup-scrub", on_key: Some("CRATONVM_G1_CLEANUP_SCRUB"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::GC, token: "g1-force-full-fresh-cycle", on_key: Some("CRATONVM_G1_FORCE_FULL_FRESH_CYCLE"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::GC, token: "g1-free-trigger-young-floor", on_key: Some("CRATONVM_G1_FREE_TRIGGER_YOUNG_FLOOR"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::GC, token: "g1-ihop-backoff", on_key: Some("CRATONVM_G1_IHOP_BACKOFF"), off_key: None, off_word: None, since: "2026-09-20" },
    E { group: Group::GC, token: "g1-mark-side-tables", on_key: Some("CRATONVM_G1_MARK_SIDE_TABLES"), off_key: None, off_word: None, since: "2026-09-20" },
    E { group: Group::GC, token: "g1-cleanup-satb-discard", on_key: Some("CRATONVM_G1_CLEANUP_SATB_DISCARD"), off_key: None, off_word: None, since: "2026-09-20" },
    E { group: Group::GC, token: "g1-cleanup-reset-tams", on_key: Some("CRATONVM_G1_CLEANUP_RESET_TAMS"), off_key: None, off_word: None, since: "2026-09-20" },
    E { group: Group::GC, token: "g1-mark-convergence-epoch", on_key: Some("CRATONVM_G1_MARK_CONVERGENCE_EPOCH"), off_key: None, off_word: None, since: "2026-09-20" },
    E { group: Group::GC, token: "g1-jit-mark-driver", on_key: Some("CRATONVM_G1_JIT_MARK_DRIVER"), off_key: None, off_word: None, since: "2026-09-05" },
    E { group: Group::GC, token: "g1-verify-holders", on_key: Some("CRATONVM_G1_VERIFY_HOLDERS"), off_key: None, off_word: None, since: "2026-09-05" },
    E { group: Group::GC, token: "g1-scrub-free", on_key: Some("CRATONVM_G1_SCRUB_FREE"), off_key: None, off_word: None, since: "2026-08-18" },
    E { group: Group::GC, token: "g1-narrow-fixup", on_key: Some("CRATONVM_G1_NARROW_FIXUP"), off_key: None, off_word: Some("0"), since: "2026-08-18" },
    E { group: Group::GC, token: "g1-parallel-evac-in-jit", on_key: Some("CRATONVM_G1_PARALLEL_EVAC_IN_JIT"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "g1-parallel-evac-screen", on_key: Some("CRATONVM_G1_PARALLEL_EVAC_SCREEN"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    E { group: Group::GC, token: "g1-serial-evac-holder-screen", on_key: Some("CRATONVM_G1_SERIAL_EVAC_HOLDER_SCREEN"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::GC, token: "g1-verify-forwards-retired", on_key: Some("CRATONVM_G1_VERIFY_FORWARDS_RETIRED"), off_key: None, off_word: None, since: "2026-09-07" },
    E { group: Group::GC, token: "g1-evac-copy-watch", on_key: Some("CRATONVM_G1_EVAC_COPY_WATCH"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::GC, token: "g1-parallel-evac-shared-dest", on_key: Some("CRATONVM_G1_PARALLEL_EVAC_SHARED_DEST"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::GC, token: "g1-evac-supply-screen", on_key: Some("CRATONVM_G1_EVAC_SUPPLY_SCREEN"), off_key: None, off_word: Some("0"), since: "2026-09-08" },
    E { group: Group::GC, token: "g1-evac-candidate-arena-screen", on_key: Some("CRATONVM_G1_EVAC_CANDIDATE_ARENA_SCREEN"), off_key: None, off_word: Some("0"), since: "2026-09-08" },
    E { group: Group::GC, token: "g1-evac-empty-header-grid-proof", on_key: Some("CRATONVM_G1_EVAC_EMPTY_HEADER_GRID_PROOF"), off_key: None, off_word: Some("0"), since: "2026-09-08" },
    E { group: Group::GC, token: "g1-ref-write-watch", on_key: Some("CRATONVM_G1_REF_WRITE_WATCH"), off_key: None, off_word: None, since: "2026-09-08" },
    E { group: Group::GC, token: "g1-parallel-evac-resume-dest", on_key: Some("CRATONVM_G1_PARALLEL_EVAC_RESUME_DEST"), off_key: None, off_word: Some("0"), since: "2026-09-06" },
    E { group: Group::GC, token: "g1-evac-ref-implausible-refuse", on_key: Some("CRATONVM_G1_EVAC_REF_IMPLAUSIBLE_REFUSE"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::GC, token: "g1-cleanup-walk", on_key: Some("CRATONVM_G1_CLEANUP_WALK"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::GC, token: "g1-adaptive-ihop", on_key: Some("CRATONVM_G1_ADAPTIVE_IHOP"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "g1-adaptive-tenuring", on_key: Some("CRATONVM_G1_ADAPTIVE_TENURING"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "gc-reserve", on_key: Some("CRATONVM_GC_RESERVE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "zgc-markbits", on_key: Some("CRATONVM_ZGC_MARKBITS"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "zgc-page-pinned-relocate", on_key: Some("CRATONVM_ZGC_PAGE_PINNED_RELOCATE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "zgc-parsweep", on_key: Some("CRATONVM_ZGC_PARSWEEP"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::GC, token: "zgc-par-relocate", on_key: Some("CRATONVM_ZGC_PAR_RELOCATE"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    E { group: Group::GC, token: "zgc-jit-inline-announce", on_key: Some("CRATONVM_ZGC_JIT_INLINE_ANNOUNCE"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    E { group: Group::GC, token: "zgc-bitmap-sweep", on_key: Some("CRATONVM_ZGC_BITMAP_SWEEP"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "zgc-mark-root-filter", on_key: Some("CRATONVM_ZGC_MARK_ROOT_FILTER"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    // Round 9 wave 5, default OFF: a thread sets start bits inside the ZGC VM
    // TLAB chunk it owns with a plain store (edge words keep the atomic).
    E { group: Group::GC, token: "zgc-tlab-owned-starts", on_key: Some("CRATONVM_ZGC_TLAB_OWNED_STARTS"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    // Default ON since round 9 wave 5 (`gc/src/zgc/vm_tlab.rs`,
    // `ZGC_VM_TLAB_DEFAULT_ON`, measured); `0` is the kill switch.
    E { group: Group::GC, token: "zgc-jit-tlab", on_key: Some("CRATONVM_ZGC_JIT_TLAB"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
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
    // Lane E, 2026-09-20. Both default ON with `=0` as the kill switch; see
    // `g1_tlab_clamp_oversize` and `g1_humongous_best_fit` in `gc/src/g1.rs`.
    E { group: Group::GC, token: "g1-tlab-clamp", on_key: Some("CRATONVM_G1_TLAB_CLAMP"), off_key: None, off_word: Some("0"), since: "2026-09-20" },
    E { group: Group::GC, token: "g1-humongous-best-fit", on_key: Some("CRATONVM_G1_HUMONGOUS_BEST_FIT"), off_key: None, off_word: Some("0"), since: "2026-09-20" },
    E { group: Group::GC, token: "g1-parallel-mark", on_key: Some("CRATONVM_G1_PARALLEL_MARK"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::DBG, token: "g1-dbg-rset", on_key: Some("CRATONVM_G1_DBG_RSET"), off_key: None, off_word: None, since: "2026-08-18" },
    E { group: Group::GC, token: "g1-workers", on_key: Some("CRATONVM_G1_WORKERS"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::GC, token: "g1-rset-source-cap", on_key: Some("CRATONVM_G1_RSET_SOURCE_CAP"), off_key: None, off_word: None, since: "2026-08-13" },
    E { group: Group::GC, token: "g1-verify-budget", on_key: Some("CRATONVM_G1_VERIFY_BUDGET"), off_key: None, off_word: None, since: "2026-08-13" },
    // Lane W4-B: the CSet verifier's target SWEEP PERIOD in pauses. `0` (the
    // default) keeps the flat object budget above, whose coverage falls
    // linearly with the live set — 0.31% of a 160 MiB heap per pause, 0.065%
    // of a 768 MiB one. See `verify_sweep_pauses` in `gc/src/g1.rs`.
    E { group: Group::GC, token: "g1-verify-sweep-pauses", on_key: Some("CRATONVM_G1_VERIFY_SWEEP_PAUSES"), off_key: None, off_word: None, since: "2026-09-21" },
    E { group: Group::DBG, token: "g1-verify-stale-forward", on_key: Some("CRATONVM_G1_VERIFY_STALE_FORWARD"), off_key: None, off_word: Some("0"), since: "2026-09-20" },
    // Lane B wave 2, 2026-09-20. `free-census-on-reclaim` is default ON (`=0`
    // is the kill switch) because the behaviour it replaces is a cache that a
    // test proves wrong; `narrow-drain-fixup` is opt-IN (`=1`) because the
    // drain, unlike the four pause drivers, never scans its remembered-set
    // sources and so has not earned the narrowing. See `free_census_on_reclaim`
    // and `narrow_drain_fixup` in `gc/src/g1.rs`; the 109 ms the second lever
    // is aimed at is written on its gate.
    E { group: Group::GC, token: "g1-free-census-on-reclaim", on_key: Some("CRATONVM_G1_FREE_CENSUS_ON_RECLAIM"), off_key: None, off_word: Some("0"), since: "2026-09-20" },
    E { group: Group::GC, token: "g1-narrow-drain-fixup", on_key: Some("CRATONVM_G1_NARROW_DRAIN_FIXUP"), off_key: None, off_word: None, since: "2026-09-20" },
    // Lane A, 2026-09-20. Both landed default-ON and were flipped to opt-in (`=1`)
    // the same day on a measurement: each costs 6-13% on two of three G1
    // workloads and wins ~4% on the third. The tables are on the gates. See
    // `lane_a_card_cursor` and `lane_a_rset_prune_on_scan` in `gc/src/g1.rs`.
    E { group: Group::GC, token: "g1-card-cursor", on_key: Some("CRATONVM_G1_CARD_CURSOR"), off_key: None, off_word: None, since: "2026-09-20" },
    E { group: Group::GC, token: "g1-rset-prune-on-scan", on_key: Some("CRATONVM_G1_RSET_PRUNE_ON_SCAN"), off_key: None, off_word: None, since: "2026-09-20" },
    // Lane A wave 4, 2026-09-21. THE BLOCK-OFFSET TABLE: the remembered-set
    // source walk enters a dirty card at a recorded object start instead of
    // sizing every object from the region's base to reach it. Opt-IN (`=1`),
    // and this one is opt-in for a correctness reason rather than a
    // performance one: every other lever in this group fails by over-scanning,
    // and a wrong block offset makes the walk decode a header at an address
    // that does not hold one. The entry is screened before it is used and a
    // refusal is counted on the `[GC] g1 block-offsets:` census line. See
    // `lane_a_block_offsets` in `gc/src/g1.rs` and
    // `docs/internal/g1-2026-09-20/w4a-block-offset-table.md`.
    E { group: Group::GC, token: "g1-block-offsets", on_key: Some("CRATONVM_G1_BLOCK_OFFSETS"), off_key: None, off_word: None, since: "2026-09-21" },
    // Lane D, 2026-09-20. `evac-local-queue` is default ON (`=0` restores the
    // take-one/push-all ordering); the two parallel-phase levers are opt-IN
    // (`=1`), so production keeps today's behaviour byte for byte. See
    // `evac_local_queue_enabled`, `parallel_seed_enabled` and
    // `parallel_mixed_enabled` in `gc/src/g1.rs`.
    E { group: Group::GC, token: "g1-evac-local-queue", on_key: Some("CRATONVM_G1_EVAC_LOCAL_QUEUE"), off_key: None, off_word: Some("0"), since: "2026-09-20" },
    // Lane D wave 2, 2026-09-20. The parallel evacuator's PER-WORKER engagement
    // census (objects scanned, bytes copied, queue lifts, idle time), default ON
    // and disarmed with `=0`. Default-on on a measurement: the counters are u64
    // adds on worker-local state and the only clock read is one `Instant` pair
    // per idle EPISODE, and the A/B is inside its own spread. Before it existed
    // there was no way to tell a pause that used 23 workers from one that woke
    // 23 workers and did the work on the driver. See `evac_census_enabled`.
    E { group: Group::GC, token: "g1-evac-census", on_key: Some("CRATONVM_G1_EVAC_CENSUS"), off_key: None, off_word: Some("0"), since: "2026-09-20" },
    // Lane W7-W, 2026-09-21. The two DISTRIBUTIONS behind the evacuator's
    // load-balancing valve: children pushed per `process_object`, and the local
    // stack depth those pushes reach. `EVAC_LOCAL_PUBLISH_HIGH` is compared
    // against the SECOND of those, and no instrument published either, so
    // "does a real Java graph ever reach 256" had no answer. Opt-in rather than
    // default-on: unlike `g1-evac-census` it has no A/B behind it, and this
    // round's rule is that a new lever defaults to today's behaviour until a
    // measurement says otherwise. See `evac_share_census_enabled`.
    E { group: Group::GC, token: "g1-evac-share-census", on_key: Some("CRATONVM_G1_EVAC_SHARE_CENSUS"), off_key: None, off_word: None, since: "2026-09-21" },
    // Lane D wave 2, 2026-09-20. `shared_dest_alloc` remembers that a full pass
    // over the region table found no room, instead of repeating the pass for
    // every object. Default ON on a measurement: on `HumongousChurn 48 6000 512
    // -Xmx160m` the pass ran 742,926 times and succeeded zero times, because on
    // a young pause a Survivor candidate cannot exist. `=0` restores the repeat.
    // See `evac_dest_latch_enabled` and `SharedEvac::dest_empty_slot`.
    E { group: Group::GC, token: "g1-evac-dest-latch", on_key: Some("CRATONVM_G1_EVAC_DEST_LATCH"), off_key: None, off_word: Some("0"), since: "2026-09-20" },
    E { group: Group::GC, token: "g1-parallel-seed", on_key: Some("CRATONVM_G1_PARALLEL_SEED"), off_key: None, off_word: None, since: "2026-09-20" },
    E { group: Group::GC, token: "g1-parallel-mixed", on_key: Some("CRATONVM_G1_PARALLEL_MIXED"), off_key: None, off_word: None, since: "2026-09-20" },
    // Lane W7-L, wave 7 of 2026-09-20. THE FUTILE OLD-TO-OLD COPY LOOP: after
    // four consecutive mixed pauses that reclaimed less than 150% of what they
    // copied, refuse the mixed phase instead of taking a fifth, and admit one
    // poll in 32 afterwards so a heap that starts reclaiming again can resume.
    // Opt-IN (`=1`); unset or `=0` is today's behaviour byte for byte, and the
    // counters behind the breaker are maintained in BOTH arms so the unarmed
    // run is the null arm. The refusals and the probes are printed on the
    // `[GC] g1 mixed-futile:` census line in both arms. See
    // `mixed_futile_breaker_allows_within` in `gc/src/g1.rs` and
    // `docs/internal/g1-2026-09-20/w7l-the-futile-old-to-old-copy-loop.md`.
    E { group: Group::GC, token: "g1-mixed-futile-guard", on_key: Some("CRATONVM_G1_MIXED_FUTILE_GUARD"), off_key: None, off_word: Some("0"), since: "2026-09-21" },
    // Lane E, wave 2 of 2026-09-20. All three opt-IN (`=1`), so production is
    // byte for byte what it was: wave 1's lesson was that two of three levers
    // landed default-ON on an argument rather than a number were wrong. See
    // `g1_heap_resize_enabled`, `g1_pause_cost_model` and
    // `g1_humongous_run_guard` in `gc/src/g1.rs`.
    E { group: Group::GC, token: "g1-heap-resize", on_key: Some("CRATONVM_G1_HEAP_RESIZE"), off_key: None, off_word: None, since: "2026-09-20" },
    E { group: Group::GC, token: "g1-pause-cost-model", on_key: Some("CRATONVM_G1_PAUSE_COST_MODEL"), off_key: None, off_word: None, since: "2026-09-20" },
    E { group: Group::GC, token: "g1-humongous-run-guard", on_key: Some("CRATONVM_G1_HUMONGOUS_RUN_GUARD"), off_key: None, off_word: None, since: "2026-09-20" },
    // Lane W4-B, wave 4 of 2026-09-20. Default ON with `=0` restoring the old
    // behaviour, unlike the wave-2 three above, because the number came first:
    // the single drain pass on `HumongousChurn 48 6000 512 -Xmx160m --nojit`
    // cut the young target by 21% and moved `fixup_ns_ema` by +11% while
    // recording itself as a young pause it is not. See `g1_drain_not_a_pause`
    // in `gc/src/g1.rs`.
    E { group: Group::GC, token: "g1-drain-not-a-pause", on_key: Some("CRATONVM_G1_DRAIN_NOT_A_PAUSE"), off_key: None, off_word: Some("0"), since: "2026-09-21" },
    // Lane W4-B: does a YOUNG pause refresh `evac_ns_per_byte`? Opt-in. Today
    // only the two mixed drivers do, so on a workload with no mixed collection
    // the estimate holds its constructor value 4 for the whole run — which is
    // the input `lane-e-proposals.md` P1's predictor is built on. See
    // `g1_evac_cost_from_young` in `gc/src/g1.rs`.
    E { group: Group::GC, token: "g1-evac-cost-from-young", on_key: Some("CRATONVM_G1_EVAC_COST_FROM_YOUNG"), off_key: None, off_word: None, since: "2026-09-21" },
    // Lane W5-C, wave 5 of 2026-09-20. A counted fail-safe on the mark-cycle
    // back-off (`CRATONVM_G1_IHOP_BACKOFF`), whose only re-arm today is
    // old-generation GROWTH. Growth cannot see old objects DYING, so a program
    // that drops a large old data set and then allocates only short-lived
    // objects holds the gate shut over exactly the cycle that would have paid
    // for itself. Opt-in, and inert unless the back-off is also on. See
    // `check_ihop` in `gc/src/g1.rs` and `tools/probes/G1OldBurstProbe.java`.
    E { group: Group::GC, token: "g1-mark-backoff-deadline", on_key: Some("CRATONVM_G1_MARK_BACKOFF_DEADLINE"), off_key: None, off_word: None, since: "2026-09-21" },
    // Lane W6-B, wave 6 of 2026-09-20. The mark-cycle back-off's DECLINED path
    // costs two read-modify-writes on two process-global cache lines, and it
    // is taken at the ALLOCATION rate — a suppressed cycle never sets
    // `g1_is_marking_active()`, so nothing short-circuits the JIT mark driver
    // that every compiled `new` calls. Wave 3 measured 1 616 077 polls, wave 5
    // measured 41 641 213 against 130 mark cycles, both single-threaded. This
    // makes a declined poll two relaxed loads and a thread-local increment.
    // Opt-in, and inert unless the back-off is also on. See `check_ihop` in
    // `gc/src/g1.rs` and `tools/probes/G1PollStormProbe.java`.
    E { group: Group::GC, token: "g1-ihop-poll-gate", on_key: Some("CRATONVM_G1_IHOP_POLL_GATE"), off_key: None, off_word: None, since: "2026-09-21" },
    // Lane W7-D, wave 7 of 2026-09-20. DEFAULT ON — `=0` is the kill switch,
    // and it restores the behaviour every measurement in this round was taken
    // under. `marking_complete` is set only by `G1Collector::cleanup`, which is
    // reached only through `interpreter::g1_final_remark_cleanup`, whose three
    // callers were `maybe_gc`'s epilogue, the opt-in JIT mark driver, and the
    // pre-OOM ladder. The allocation-failure pause — `maybe_gc_forced_at`,
    // which every JIT allocation helper, `alloc_object_shared`,
    // `gc_alloc_array` and the TLAB refill wedge funnel into, and which is
    // where G1 takes most of its pauses — had no lifecycle call at all. A
    // cycle that opened therefore never closed, and because
    // `g1_should_start_marking()` refuses to start while one is active, the
    // collector spent the rest of the process with one open cycle and no mixed
    // pause. This puts the same finish-then-start pair on that pause's
    // epilogue. See `docs/internal/g1-2026-09-20/w7d-the-cycle-that-no-pause-closes.md`.
    E { group: Group::GC, token: "g1-alloc-mark-drive", on_key: Some("CRATONVM_G1_ALLOC_MARK_DRIVE"), off_key: None, off_word: Some("0"), since: "2026-09-21" },
    // Lane W5-A, wave 5 of 2026-09-20. Makes `evac_ns_per_byte` — the term the
    // whole mixed copy budget is built on, and the only thing standing between
    // `max_gc_pause_ms` and a collection set of any size — a MEASUREMENT rather
    // than the constructor constant 4 it has held since Step 7. Its sole writer
    // is reached only from a mixed collection, so it is unwritten on every
    // young-only run and still unwritten on the FIRST mixed pause of every
    // other run, which is the largest one. Three changes, all under this
    // switch: a young pause calibrates its own per-byte term, the numerator
    // becomes the closure phase instead of the whole pause (the budget has
    // already subtracted the fixed phases, so a whole-pause numerator charges
    // them twice), and the first real sample is adopted whole instead of being
    // walked toward at 1/8 a pause. Opt-in: arming it can only shrink a mixed
    // collection set, which is the safe direction for the pause goal and the
    // unsafe one for throughput. See `g1_evac_cost_real` in `gc/src/g1.rs` and
    // `docs/internal/g1-2026-09-20/w5a-the-constant-nothing-writes.md`.
    E { group: Group::GC, token: "g1-evac-cost-real", on_key: Some("CRATONVM_G1_EVAC_COST_REAL"), off_key: None, off_word: None, since: "2026-09-21" },
    // Lane W5-B, wave 5 of 2026-09-20. The block-offset table's AUDIT arm, as
    // a SAMPLING PERIOD: `=N` walks the bytes one jump in every N skipped and
    // checks that no object starting in them holds a reference into another
    // region. A hit is the producer rule ("a card names the START of the
    // object holding the edge") breaking in the one shape the runtime
    // `GC_FLAG_HEADER` vouch cannot see — a producer that names a real object
    // start belonging to the WRONG object — and it DISARMS the jump for the
    // rest of the process, exactly as an unvouched producer address does.
    //
    // A period and not a boolean because the fault is a producer, and a
    // producer is systematic: sampling delays the catch by N jumps rather than
    // lowering the chance of it, and costs 1/N of what the jump saves. `=1`
    // audits every jump and is the soak/CI setting; `0`, unset and anything
    // unparseable are off (a typo must not silently buy a whole extra walk).
    // Inert unless `CRATONVM_G1_BLOCK_OFFSETS` is also on. Value tokens:
    // `CRATONVM_GC=g1-block-offset-audit=1024`. See
    // `lane_a_block_offset_audit` in `gc/src/g1.rs` and
    // `docs/internal/g1-2026-09-20/w5b-block-offset-enforcement.md`.
    E { group: Group::GC, token: "g1-block-offset-audit", on_key: Some("CRATONVM_G1_BLOCK_OFFSET_AUDIT"), off_key: None, off_word: None, since: "2026-09-21" },
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
    E { group: Group::JIT, token: "ffm-close-retires-verdicts", on_key: Some("CRATONVM_FFM_CLOSE_RETIRES_VERDICTS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-access-handshake", on_key: Some("CRATONVM_FFM_ACCESS_HANDSHAKE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-synthetic-arena-closed-check", on_key: Some("CRATONVM_FFM_SYNTHETIC_ARENA_CLOSED_CHECK"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "aaload-licm-clinit-fence", on_key: Some("CRATONVM_JIT_AALOAD_LICM_CLINIT_FENCE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "arith-licm-trim-entered", on_key: Some("CRATONVM_JIT_ARITH_LICM_TRIM_ENTERED"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "callee-table-memo", on_key: Some("CRATONVM_JIT_CALLEE_TABLE_MEMO"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "deopt-replay-first-arrival", on_key: Some("CRATONVM_JIT_DEOPT_REPLAY_FIRST_ARRIVAL"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "dispatch-soe-throws", on_key: Some("CRATONVM_JIT_DISPATCH_SOE_THROWS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "hashed-stub-clone-pic", on_key: Some("CRATONVM_JIT_HASHED_STUB_CLONE_PIC"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "hashed-stub-mega-table", on_key: Some("CRATONVM_JIT_HASHED_STUB_MEGA_TABLE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "implicit-callee-throw-pc", on_key: Some("CRATONVM_JIT_IMPLICIT_CALLEE_THROW_PC"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "inline-body-oop-marks", on_key: Some("CRATONVM_JIT_INLINE_BODY_OOP_MARKS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-bce-mask-index", on_key: Some("CRATONVM_JIT_IR_BCE_MASK_INDEX"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-bce-stride3", on_key: Some("CRATONVM_JIT_IR_BCE_STRIDE3"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-catch-retier", on_key: Some("CRATONVM_JIT_IR_CATCH_RETIER"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-ea-narrow-phi", on_key: Some("CRATONVM_JIT_IR_EA_NARROW_PHI"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-monitor-op-vouch", on_key: Some("CRATONVM_JIT_IR_MONITOR_OP_VOUCH"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-osr-predicate-refusal", on_key: Some("CRATONVM_JIT_IR_OSR_PREDICATE_REFUSAL"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-range-fold-cmp", on_key: Some("CRATONVM_JIT_IR_RANGE_FOLD_CMP"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-replay-fallback-check", on_key: Some("CRATONVM_JIT_IR_REPLAY_FALLBACK_CHECK"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-scalar-identities", on_key: Some("CRATONVM_JIT_IR_SCALAR_IDENTITIES"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "licm-row-oop-mark", on_key: Some("CRATONVM_JIT_LICM_ROW_OOP_MARK"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "mega-table-exc-callees", on_key: Some("CRATONVM_JIT_MEGA_TABLE_EXC_CALLEES"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "mega-table-publish-dispatch", on_key: Some("CRATONVM_JIT_MEGA_TABLE_PUBLISH_DISPATCH"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "mega-table-publish-lambda", on_key: Some("CRATONVM_JIT_MEGA_TABLE_PUBLISH_LAMBDA"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "npe-message-memo", on_key: Some("CRATONVM_JIT_NPE_MESSAGE_MEMO"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "oop-mark-check", on_key: Some("CRATONVM_JIT_OOP_MARK_CHECK"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "oop-mark-check-strict", on_key: Some("CRATONVM_JIT_OOP_MARK_CHECK_STRICT"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "self-call-floor-stride-margin", on_key: Some("CRATONVM_JIT_SELF_CALL_FLOOR_STRIDE_MARGIN"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "trap-splice-lazy-lines", on_key: Some("CRATONVM_JIT_TRAP_SPLICE_LAZY_LINES"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "vec-ewise-forms", on_key: Some("CRATONVM_JIT_VEC_EWISE_FORMS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "pgo-loop-keys", on_key: Some("CRATONVM_JIT_PGO_LOOP_KEYS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "profile-frame-len-check", on_key: Some("CRATONVM_JIT_PROFILE_FRAME_LEN_CHECK"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "eager-door-route-all", on_key: Some("CRATONVM_JIT_EAGER_DOOR_ROUTE_ALL"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "door-unsound-rerun-retire", on_key: Some("CRATONVM_JIT_DOOR_UNSOUND_RERUN_RETIRE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "single-pass-replay-check", on_key: Some("CRATONVM_JIT_SINGLE_PASS_REPLAY_CHECK"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "withdrawal-demotes", on_key: Some("CRATONVM_JIT_WITHDRAWAL_DEMOTES"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "helper-failure-conversion", on_key: Some("CRATONVM_JIT_HELPER_FAILURE_CONVERSION"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "trap-capture-one-walk", on_key: Some("CRATONVM_JIT_TRAP_CAPTURE_ONE_WALK"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-exact-receiver-splice", on_key: Some("CRATONVM_JIT_IR_EXACT_RECEIVER_SPLICE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-const-if-fold", on_key: Some("CRATONVM_JIT_IR_CONST_IF_FOLD"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "field-array-hoist", on_key: Some("CRATONVM_JIT_FIELD_ARRAY_HOIST"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ref-hoist-oop-maps", on_key: Some("CRATONVM_JIT_REF_HOIST_OOP_MAPS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "aaload-licm-typecheck-fence", on_key: Some("CRATONVM_JIT_AALOAD_LICM_TYPECHECK_FENCE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "hashed-stub-class-slots", on_key: Some("CRATONVM_JIT_HASHED_STUB_CLASS_SLOTS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "mega-class-slots", on_key: Some("CRATONVM_JIT_MEGA_CLASS_SLOTS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "mega-class-slots-iface", on_key: Some("CRATONVM_JIT_MEGA_CLASS_SLOTS_IFACE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "mega-selector-by-loader", on_key: Some("CRATONVM_JIT_MEGA_SELECTOR_BY_LOADER"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "mega-table-owner-index", on_key: Some("CRATONVM_JIT_MEGA_TABLE_OWNER_INDEX"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "define-scoped-withdrawal", on_key: Some("CRATONVM_JIT_DEFINE_SCOPED_WITHDRAWAL"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "guard-evidence-fidelity", on_key: Some("CRATONVM_JIT_GUARD_EVIDENCE_FIDELITY"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "tier-from-live-body", on_key: Some("CRATONVM_JIT_TIER_FROM_LIVE_BODY"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "compact-compiled-backtrace", on_key: Some("CRATONVM_JIT_COMPACT_COMPILED_BACKTRACE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "deopt-rematerialise-oom", on_key: Some("CRATONVM_DEOPT_REMATERIALISE_OOM"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "lambda-resume-failure-raises", on_key: Some("CRATONVM_JIT_LAMBDA_RESUME_FAILURE_RAISES"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-layout-order", on_key: Some("CRATONVM_FFM_LAYOUT_ORDER"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-native-align-check", on_key: Some("CRATONVM_FFM_NATIVE_ALIGN_CHECK"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-bulk-scope-check", on_key: Some("CRATONVM_FFM_BULK_SCOPE_CHECK"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-arena-close-frees", on_key: Some("CRATONVM_FFM_ARENA_CLOSE_FREES"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-shared-arena-close-frees", on_key: Some("CRATONVM_FFM_SHARED_ARENA_CLOSE_FREES"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-splice-fence-forward-branch", on_key: Some("CRATONVM_JIT_IR_SPLICE_FENCE_FORWARD_BRANCH"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-splice-fence-null-facts", on_key: Some("CRATONVM_JIT_IR_SPLICE_FENCE_NULL_FACTS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-splice-fence-guarded-call", on_key: Some("CRATONVM_JIT_IR_SPLICE_FENCE_GUARDED_CALL"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "trace-only-sp-id", on_key: Some("CRATONVM_JIT_TRACE_ONLY_SP_ID"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "deopt-resume-current-body-of-redefined", on_key: Some("CRATONVM_DEOPT_RESUME_CURRENT_BODY_OF_REDEFINED"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-downcall-acquire", on_key: Some("CRATONVM_FFM_DOWNCALL_ACQUIRE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-acquire-handshake", on_key: Some("CRATONVM_FFM_ACQUIRE_HANDSHAKE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-bulk-heap-segments", on_key: Some("CRATONVM_FFM_BULK_HEAP_SEGMENTS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "jit-door-unsound-rerun-raises", on_key: Some("CRATONVM_JIT_DOOR_UNSOUND_RERUN_RAISES"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "jit-door-sync-additive-resume", on_key: Some("CRATONVM_JIT_DOOR_SYNC_ADDITIVE_RESUME"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "deopt-sync-virtuals-resume", on_key: Some("CRATONVM_DEOPT_SYNC_VIRTUALS_RESUME"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-guarded-receiver-splice", on_key: Some("CRATONVM_JIT_IR_GUARDED_RECEIVER_SPLICE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-licm-hoist-dedup", on_key: Some("CRATONVM_JIT_IR_LICM_HOIST_DEDUP"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ic-same-key-refill", on_key: Some("CRATONVM_JIT_IC_SAME_KEY_REFILL"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "mega-table-superseded-rollback", on_key: Some("CRATONVM_JIT_MEGA_TABLE_SUPERSEDED_ROLLBACK"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "mega-table-reclaim", on_key: Some("CRATONVM_JIT_MEGA_TABLE_RECLAIM"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "mega-gate-fast-entry", on_key: Some("CRATONVM_JIT_MEGA_GATE_FAST_ENTRY"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-mega-gate-entry", on_key: Some("CRATONVM_JIT_IR_MEGA_GATE_ENTRY"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-ic-kind-screen-elide", on_key: Some("CRATONVM_JIT_IR_IC_KIND_SCREEN_ELIDE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bake-only-kept-callee", on_key: Some("CRATONVM_JIT_BAKE_ONLY_KEPT_CALLEE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "rescue-entered-unpublished", on_key: Some("CRATONVM_JIT_RESCUE_ENTERED_UNPUBLISHED"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-offset-access", on_key: Some("CRATONVM_JIT_FFM_OFFSET_ACCESS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-reinterpret-checks", on_key: Some("CRATONVM_FFM_REINTERPRET_CHECKS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-downcall-scratch-free-on-error", on_key: Some("CRATONVM_FFM_DOWNCALL_SCRATCH_FREE_ON_ERROR"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-segment-vh-checks", on_key: Some("CRATONVM_FFM_SEGMENT_VH_CHECKS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-segment-vh-form", on_key: Some("CRATONVM_FFM_SEGMENT_VH_FORM"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-bounds-before-scope", on_key: Some("CRATONVM_FFM_BOUNDS_BEFORE_SCOPE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-layout-vh-read-only", on_key: Some("CRATONVM_FFM_LAYOUT_VH_READ_ONLY"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-real-session-fast-check", on_key: Some("CRATONVM_FFM_REAL_SESSION_FAST_CHECK"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-upcall-stubs", on_key: Some("CRATONVM_FFM_UPCALL_STUBS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-upcall-type-check", on_key: Some("CRATONVM_FFM_UPCALL_TYPE_CHECK"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-upcall-attach", on_key: Some("CRATONVM_FFM_UPCALL_ATTACH"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-upcall-check-exceptions", on_key: Some("CRATONVM_FFM_UPCALL_CHECK_EXCEPTIONS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-opaque-low-pointer-args", on_key: Some("CRATONVM_FFM_OPAQUE_LOW_POINTER_ARGS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-arena-noinit-carrier", on_key: Some("CRATONVM_FFM_ARENA_NOINIT_CARRIER"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-downcall-return-allocator", on_key: Some("CRATONVM_FFM_DOWNCALL_RETURN_ALLOCATOR"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-upcall-type-check-object", on_key: Some("CRATONVM_FFM_UPCALL_TYPE_CHECK_OBJECT"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-downcall-gc-safe", on_key: Some("CRATONVM_FFM_DOWNCALL_GC_SAFE"), off_key: None, off_word: None, since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-null-address-npe-message", on_key: Some("CRATONVM_FFM_NULL_ADDRESS_NPE_MESSAGE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ffm-auto-arena-action-free", on_key: Some("CRATONVM_FFM_AUTO_ARENA_ACTION_FREE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ffm-group-members-unwrap", on_key: Some("CRATONVM_FFM_GROUP_MEMBERS_UNWRAP"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ffm-default-lookup-c-runtime", on_key: Some("CRATONVM_FFM_DEFAULT_LOOKUP_C_RUNTIME"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ffm-address-store-value-first", on_key: Some("CRATONVM_FFM_ADDRESS_STORE_VALUE_FIRST"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ffm-address-global-scope", on_key: Some("CRATONVM_FFM_ADDRESS_GLOBAL_SCOPE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ffm-auto-arena-block-free", on_key: Some("CRATONVM_FFM_AUTO_ARENA_BLOCK_FREE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ffm-carrier-scope-session", on_key: Some("CRATONVM_FFM_CARRIER_SCOPE_SESSION"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-upcall-cross-vm", on_key: Some("CRATONVM_FFM_UPCALL_CROSS_VM"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-address-read-target", on_key: Some("CRATONVM_FFM_ADDRESS_READ_TARGET"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-vh-address-heap-refusal", on_key: Some("CRATONVM_FFM_VH_ADDRESS_HEAP_REFUSAL"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ffm-null-address-npe", on_key: Some("CRATONVM_FFM_NULL_ADDRESS_NPE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ffm-lookup-symbol-writable", on_key: Some("CRATONVM_FFM_LOOKUP_SYMBOL_WRITABLE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ffm-default-lookup-syslookup", on_key: Some("CRATONVM_FFM_DEFAULT_LOOKUP_SYSLOOKUP"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ffm-upcall-direct-static", on_key: Some("CRATONVM_FFM_UPCALL_DIRECT_STATIC"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ffm-layout-vh-value-leaf", on_key: Some("CRATONVM_FFM_LAYOUT_VH_VALUE_LEAF"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ffm-layout-vh-enclosing-bounds", on_key: Some("CRATONVM_FFM_LAYOUT_VH_ENCLOSING_BOUNDS"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ffm-layout-vh-index-bound", on_key: Some("CRATONVM_FFM_LAYOUT_VH_INDEX_BOUND"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ffm-array-element-var-handle", on_key: Some("CRATONVM_FFM_ARRAY_ELEMENT_VAR_HANDLE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ffm-upcall-cross-vm-gc-safe-wait", on_key: Some("CRATONVM_FFM_UPCALL_CROSS_VM_GC_SAFE_WAIT"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ffm-auto-arena-cleaner", on_key: Some("CRATONVM_FFM_AUTO_ARENA_CLEANER"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-upcall-address-align-check", on_key: Some("CRATONVM_FFM_UPCALL_ADDRESS_ALIGN_CHECK"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-union-eightbyte-class", on_key: Some("CRATONVM_FFM_UNION_EIGHTBYTE_CLASS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-action-list-linked", on_key: Some("CRATONVM_FFM_ACTION_LIST_LINKED"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-layout-render-group-align", on_key: Some("CRATONVM_FFM_LAYOUT_RENDER_GROUP_ALIGN"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-check-layouts-groups", on_key: Some("CRATONVM_FFM_CHECK_LAYOUTS_GROUPS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-upcall-by-value-groups", on_key: Some("CRATONVM_FFM_UPCALL_BY_VALUE_GROUPS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-upcall-retire-at-close", on_key: Some("CRATONVM_FFM_UPCALL_RETIRE_AT_CLOSE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-session-add-atomic", on_key: Some("CRATONVM_FFM_SESSION_ADD_ATOMIC"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-close-walk-hotspot", on_key: Some("CRATONVM_FFM_CLOSE_WALK_HOTSPOT"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-add-or-cleanup-runs", on_key: Some("CRATONVM_FFM_ADD_OR_CLEANUP_RUNS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-group-members-by-name", on_key: Some("CRATONVM_FFM_GROUP_MEMBERS_BY_NAME"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-char-return-unsigned", on_key: Some("CRATONVM_FFM_CHAR_RETURN_UNSIGNED"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-close-runs-cleanup", on_key: Some("CRATONVM_FFM_CLOSE_RUNS_CLEANUP"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ffm-close-state-cas", on_key: Some("CRATONVM_FFM_CLOSE_STATE_CAS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-global-session-add-noop", on_key: Some("CRATONVM_FFM_GLOBAL_SESSION_ADD_NOOP"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-shared-record-per-thread", on_key: Some("CRATONVM_FFM_SHARED_RECORD_PER_THREAD"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-upcall-create-window", on_key: Some("CRATONVM_FFM_UPCALL_CREATE_WINDOW"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-upcall-auto-arena-free", on_key: Some("CRATONVM_FFM_UPCALL_AUTO_ARENA_FREE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-upcall-check-layouts", on_key: Some("CRATONVM_FFM_UPCALL_CHECK_LAYOUTS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-downcall-check-layouts", on_key: Some("CRATONVM_FFM_DOWNCALL_CHECK_LAYOUTS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ffm-upcall-uncaught-java-err", on_key: Some("CRATONVM_FFM_UPCALL_UNCAUGHT_JAVA_ERR"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "fma-inline", on_key: Some("CRATONVM_JIT_FMA_INLINE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "math-fma-exact", on_key: Some("CRATONVM_MATH_FMA_EXACT"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "math-fma-native", on_key: Some("CRATONVM_MATH_FMA_NATIVE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bigdecimal-divide-mc-oneshot", on_key: Some("CRATONVM_BIGDECIMAL_DIVIDE_MC_ONESHOT"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bigdecimal-pow-mc-jdk", on_key: Some("CRATONVM_BIGDECIMAL_POW_MC_JDK"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bigdecimal-tostring-cache", on_key: Some("CRATONVM_BIGDECIMAL_TOSTRING_CACHE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bignum-strings-uninterned", on_key: Some("CRATONVM_BIGNUM_STRINGS_UNINTERNED"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bigint-fast-mul", on_key: Some("CRATONVM_BIGINT_FAST_MUL"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bigdecimal-mc-mode-by-name", on_key: Some("CRATONVM_BIGDECIMAL_MC_MODE_BY_NAME"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bigint-todecimal-dc", on_key: Some("CRATONVM_BIGINT_TODECIMAL_DC"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bigdecimal-mc-result-constants", on_key: Some("CRATONVM_BIGDECIMAL_MC_RESULT_CONSTANTS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bignum-pow5-cache", on_key: Some("CRATONVM_BIGNUM_POW5_CACHE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bigdecimal-float-value-native", on_key: Some("CRATONVM_BIGDECIMAL_FLOAT_VALUE_NATIVE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bigdecimal-compare-by-bits", on_key: Some("CRATONVM_BIGDECIMAL_COMPARE_BY_BITS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bigdecimal-result-constants", on_key: Some("CRATONVM_BIGDECIMAL_RESULT_CONSTANTS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bigdecimal-binary-to-double", on_key: Some("CRATONVM_BIGDECIMAL_BINARY_TO_DOUBLE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bigdecimal-exact-divide-by-factors", on_key: Some("CRATONVM_BIGDECIMAL_EXACT_DIVIDE_BY_FACTORS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bigdecimal-add-mc-jdk", on_key: Some("CRATONVM_BIGDECIMAL_ADD_MC_JDK"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bigdecimal-null-arg-npe", on_key: Some("CRATONVM_BIGDECIMAL_NULL_ARG_NPE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bignum-jdk-identity", on_key: Some("CRATONVM_BIGNUM_JDK_IDENTITY"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "biginteger-negate-shares-mag", on_key: Some("CRATONVM_BIGINTEGER_NEGATE_SHARES_MAG"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bigdecimal-precision-no-render", on_key: Some("CRATONVM_BIGDECIMAL_PRECISION_NO_RENDER"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "bigdecimal-valueof-long-compact", on_key: Some("CRATONVM_BIGDECIMAL_VALUEOF_LONG_COMPACT"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "number-strings-uninterned", on_key: Some("CRATONVM_NUMBER_STRINGS_UNINTERNED"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "computed-strings-uninterned", on_key: Some("CRATONVM_COMPUTED_STRINGS_UNINTERNED"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "enum-ordinal-by-name", on_key: Some("CRATONVM_ENUM_ORDINAL_BY_NAME"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "biginteger-muladd-window", on_key: Some("CRATONVM_BIGINTEGER_MULADD_WINDOW"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "fp-special-literals", on_key: Some("CRATONVM_FP_SPECIAL_LITERALS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "normalizer-maybe-full-check", on_key: Some("CRATONVM_NORMALIZER_MAYBE_FULL_CHECK"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "inline-spin-backoff", on_key: Some("CRATONVM_JIT_INLINE_SPIN_BACKOFF"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-entry-fast-return", on_key: Some("CRATONVM_JIT_IR_ENTRY_FAST_RETURN"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-param-register-fill", on_key: Some("CRATONVM_JIT_IR_PARAM_REGISTER_FILL"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-entry-fold-self-calls", on_key: Some("CRATONVM_JIT_IR_ENTRY_FOLD_SELF_CALLS"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-self-call-answer", on_key: Some("CRATONVM_JIT_IR_SELF_CALL_ANSWER"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-entry-fold", on_key: Some("CRATONVM_JIT_IR_ENTRY_FOLD"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-self-call-sample-dominated", on_key: Some("CRATONVM_JIT_IR_SELF_CALL_SAMPLE_DOMINATED"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-self-call-cold-tails", on_key: Some("CRATONVM_JIT_IR_SELF_CALL_COLD_TAILS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "self-call-budget-follows-stack-size", on_key: Some("CRATONVM_JIT_SELF_CALL_BUDGET_FOLLOWS_STACK_SIZE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-guarded-exact-fact-scoped", on_key: Some("CRATONVM_JIT_IR_GUARDED_EXACT_FACT_SCOPED"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "frameless-trap-method-identity", on_key: Some("CRATONVM_JIT_FRAMELESS_TRAP_METHOD_IDENTITY"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-osr-frame-value-refusal", on_key: Some("CRATONVM_JIT_IR_OSR_FRAME_VALUE_REFUSAL"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-prune-loop-header-locals", on_key: Some("CRATONVM_JIT_IR_PRUNE_LOOP_HEADER_LOCALS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-prune-chain-snapshot-locals", on_key: Some("CRATONVM_JIT_IR_PRUNE_CHAIN_SNAPSHOT_LOCALS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-prune-handler-method-locals", on_key: Some("CRATONVM_JIT_IR_PRUNE_HANDLER_METHOD_LOCALS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "unload-withdraws-copied", on_key: Some("CRATONVM_JIT_UNLOAD_WITHDRAWS_COPIED"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-profile-guarded-splice", on_key: Some("CRATONVM_JIT_IR_PROFILE_GUARDED_SPLICE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "self-lock-static-deopt-handover", on_key: Some("CRATONVM_JIT_SELF_LOCK_STATIC_DEOPT_HANDOVER"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "holdslock-method-monitor-fold", on_key: Some("CRATONVM_JIT_HOLDSLOCK_METHOD_MONITOR_FOLD"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "self-lock-trim", on_key: Some("CRATONVM_JIT_SELF_LOCK_TRIM"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "inflated-sticky-count", on_key: Some("CRATONVM_JIT_INFLATED_STICKY_COUNT"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "lock-stack-single-bump", on_key: Some("CRATONVM_JIT_LOCK_STACK_SINGLE_BUMP"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-ls-prepin-refused", on_key: Some("CRATONVM_JIT_IR_LS_PREPIN_REFUSED"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-sync-splice", on_key: Some("CRATONVM_JIT_IR_SYNC_SPLICE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-guarded-splice-trap-miss", on_key: Some("CRATONVM_JIT_IR_GUARDED_SPLICE_TRAP_MISS"), off_key: None, off_word: None, since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-bimorphic-guarded-splice", on_key: Some("CRATONVM_JIT_IR_BIMORPHIC_GUARDED_SPLICE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "deopt-chain-outer-own-source", on_key: Some("CRATONVM_DEOPT_CHAIN_OUTER_OWN_SOURCE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "deopt-callsite-own-source", on_key: Some("CRATONVM_DEOPT_CALLSITE_OWN_SOURCE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "sync-direct-lookup-memo", on_key: Some("CRATONVM_JIT_SYNC_DIRECT_LOOKUP_MEMO"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-sync-splice-nested-elim", on_key: Some("CRATONVM_JIT_IR_SYNC_SPLICE_NESTED_ELIM"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-sync-splice-self-class-holdslock", on_key: Some("CRATONVM_JIT_IR_SYNC_SPLICE_SELF_CLASS_HOLDSLOCK"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-sync-splice-nested-same-receiver", on_key: Some("CRATONVM_JIT_IR_SYNC_SPLICE_NESTED_SAME_RECEIVER"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-sync-splice-multi-return", on_key: Some("CRATONVM_JIT_IR_SYNC_SPLICE_MULTI_RETURN"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-holdslock-fold", on_key: Some("CRATONVM_JIT_IR_HOLDSLOCK_FOLD"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-sync-static-method-monitor-facts", on_key: Some("CRATONVM_JIT_IR_SYNC_STATIC_METHOD_MONITOR_FACTS"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-holdslock-method-fold", on_key: Some("CRATONVM_JIT_IR_HOLDSLOCK_METHOD_FOLD"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-sync-method-monitor-facts", on_key: Some("CRATONVM_JIT_IR_SYNC_METHOD_MONITOR_FACTS"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-string-literal-query-fold", on_key: Some("CRATONVM_JIT_IR_STRING_LITERAL_QUERY_FOLD"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "inline-loop-sites-hot", on_key: Some("CRATONVM_JIT_INLINE_LOOP_SITES_HOT"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-sync-method-chains", on_key: Some("CRATONVM_JIT_IR_SYNC_METHOD_CHAINS"), off_key: None, off_word: None, since: "2026-09-29" },
    E { group: Group::JIT, token: "deopt-chain-own-source-sinks", on_key: Some("CRATONVM_DEOPT_CHAIN_OWN_SOURCE_SINKS"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "deopt-chain-inner-own-source", on_key: Some("CRATONVM_DEOPT_CHAIN_INNER_OWN_SOURCE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-splice-despec-skips-site", on_key: Some("CRATONVM_JIT_IR_SPLICE_DESPEC_SKIPS_SITE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-sync-window-rebuild", on_key: Some("CRATONVM_JIT_IR_SYNC_WINDOW_REBUILD"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "compile-id-code-grace", on_key: Some("CRATONVM_JIT_COMPILE_ID_CODE_GRACE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "osr-splice-entry-loop-hot", on_key: Some("CRATONVM_JIT_OSR_SPLICE_ENTRY_LOOP_HOT"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-string-literal-hash-fold", on_key: Some("CRATONVM_JIT_IR_STRING_LITERAL_HASH_FOLD"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-cross-call-cold-tails", on_key: Some("CRATONVM_JIT_IR_CROSS_CALL_COLD_TAILS"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-sync-splice-store-fence-skip", on_key: Some("CRATONVM_JIT_IR_SYNC_SPLICE_STORE_FENCE_SKIP"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-sync-splice-static-mirror", on_key: Some("CRATONVM_JIT_IR_SYNC_SPLICE_STATIC_MIRROR"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-sync-splice-in-region", on_key: Some("CRATONVM_JIT_IR_SYNC_SPLICE_IN_REGION"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "deopt-restash-keeps-source", on_key: Some("CRATONVM_DEOPT_RESTASH_KEEPS_SOURCE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "deopt-callsite-stash-template", on_key: Some("CRATONVM_DEOPT_CALLSITE_STASH_TEMPLATE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "osr-chain-guard-exit-charge", on_key: Some("CRATONVM_JIT_OSR_CHAIN_GUARD_EXIT_CHARGE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ic-quiescent-stamp", on_key: Some("CRATONVM_JIT_IC_QUIESCENT_STAMP"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ic-catch-up-self-stamp", on_key: Some("CRATONVM_JIT_IC_CATCH_UP_SELF_STAMP"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-inline-chain-by-sp-id", on_key: Some("CRATONVM_JIT_IR_INLINE_CHAIN_BY_SP_ID"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "refuse-dead-entry", on_key: Some("CRATONVM_JIT_REFUSE_DEAD_ENTRY"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-buffer-ref-home-reserve", on_key: Some("CRATONVM_JIT_IR_BUFFER_REF_HOME_RESERVE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "blocked-summary-door-exit", on_key: Some("CRATONVM_JIT_BLOCKED_SUMMARY_DOOR_EXIT"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "reclaim-cascade-proof", on_key: Some("CRATONVM_JIT_RECLAIM_CASCADE_PROOF"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-chain-snapshot-range-pins", on_key: Some("CRATONVM_JIT_IR_CHAIN_SNAPSHOT_RANGE_PINS"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "ir-residency-call-crossing", on_key: Some("CRATONVM_JIT_IR_RESIDENCY_CALL_CROSSING"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::JIT, token: "osr-splice-calls", on_key: Some("CRATONVM_JIT_OSR_SPLICE_CALLS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "osr-splice", on_key: Some("CRATONVM_JIT_OSR_SPLICE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-box-hash-fold", on_key: Some("CRATONVM_JIT_IR_BOX_HASH_FOLD"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-long-box-hash-fold", on_key: Some("CRATONVM_JIT_IR_LONG_BOX_HASH_FOLD"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-splice-cold-nested", on_key: Some("CRATONVM_JIT_IR_SPLICE_COLD_NESTED"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-splice-nested-exact-receiver", on_key: Some("CRATONVM_JIT_IR_SPLICE_NESTED_EXACT_RECEIVER"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-alloc-fast-join", on_key: Some("CRATONVM_JIT_IR_ALLOC_FAST_JOIN"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-spliced-new-skip-post-init", on_key: Some("CRATONVM_JIT_IR_SPLICED_NEW_SKIP_POST_INIT"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "door-sync-instance-body", on_key: Some("CRATONVM_JIT_DOOR_SYNC_INSTANCE_BODY"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "mega-cell-first", on_key: Some("CRATONVM_JIT_MEGA_CELL_FIRST"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ic-profile-mega-seed", on_key: Some("CRATONVM_JIT_IC_PROFILE_MEGA_SEED"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "pic-inline-first", on_key: Some("CRATONVM_JIT_PIC_INLINE_FIRST"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-splice-plan-store-fence", on_key: Some("CRATONVM_JIT_IR_SPLICE_PLAN_STORE_FENCE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-splice-rebuild", on_key: Some("CRATONVM_JIT_IR_SPLICE_REBUILD"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ir-splice-prune-nested", on_key: Some("CRATONVM_JIT_IR_SPLICE_PRUNE_NESTED"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-splice-skip-uninit-class", on_key: Some("CRATONVM_JIT_IR_SPLICE_SKIP_UNINIT_CLASS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-splice-nested-cold-reserve", on_key: Some("CRATONVM_JIT_IR_SPLICE_NESTED_COLD_RESERVE"), off_key: None, off_word: None, since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-const-if-prune", on_key: Some("CRATONVM_JIT_IR_CONST_IF_PRUNE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "tierup-sink-refusal-throws", on_key: Some("CRATONVM_JIT_TIERUP_SINK_REFUSAL_THROWS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "lambda-direct-rerun-as-doors", on_key: Some("CRATONVM_JIT_LAMBDA_DIRECT_RERUN_AS_DOORS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "deopt-tierup-sink-cp-stamp", on_key: Some("CRATONVM_DEOPT_TIERUP_SINK_CP_STAMP"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "eager-door-route-unsupported-ldc", on_key: Some("CRATONVM_JIT_EAGER_DOOR_ROUTE_UNSUPPORTED_LDC"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "frame-slot-offset-tripwire", on_key: Some("CRATONVM_JIT_FRAME_SLOT_OFFSET_TRIPWIRE"), off_key: None, off_word: None, since: "2026-09-28" },
    E { group: Group::JIT, token: "ic-stack-guard", on_key: Some("CRATONVM_JIT_IC_STACK_GUARD"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "static-intrinsic-first", on_key: Some("CRATONVM_JIT_STATIC_INTRINSIC_FIRST"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-entry-poll-outline", on_key: Some("CRATONVM_JIT_IR_ENTRY_POLL_OUTLINE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "splice-keeps-static-intrinsic", on_key: Some("CRATONVM_JIT_SPLICE_KEEPS_STATIC_INTRINSIC"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-splice-refuses-finalizable-new", on_key: Some("CRATONVM_JIT_IR_SPLICE_REFUSES_FINALIZABLE_NEW"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ic-grace-thread-evidence", on_key: Some("CRATONVM_JIT_IC_GRACE_THREAD_EVIDENCE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "soe-build-suspends-floor", on_key: Some("CRATONVM_JIT_SOE_BUILD_SUSPENDS_FLOOR"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "deopt-resume-obsolete-activation", on_key: Some("CRATONVM_DEOPT_RESUME_OBSOLETE_ACTIVATION"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "deopt-chain-inner-redefined-refuses", on_key: Some("CRATONVM_DEOPT_CHAIN_INNER_REDEFINED_REFUSES"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "deopt-chain-last-instr-pc", on_key: Some("CRATONVM_DEOPT_CHAIN_LAST_INSTR_PC"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "deopt-chain-run-from-outermost", on_key: Some("CRATONVM_DEOPT_CHAIN_RUN_FROM_OUTERMOST"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "osr-chain-guard-exits", on_key: Some("CRATONVM_JIT_OSR_CHAIN_GUARD_EXITS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "deopt-chain-outer-redefined-refuses", on_key: Some("CRATONVM_DEOPT_CHAIN_OUTER_REDEFINED_REFUSES"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "deopt-chain-callsite-by-point", on_key: Some("CRATONVM_DEOPT_CHAIN_CALLSITE_BY_POINT"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "door-unsound-foreign-rerun-raises", on_key: Some("CRATONVM_JIT_DOOR_UNSOUND_FOREIGN_RERUN_RAISES"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "callee-unsound-rerun-raises", on_key: Some("CRATONVM_JIT_CALLEE_UNSOUND_RERUN_RAISES"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "callee-declined-verdict-first", on_key: Some("CRATONVM_JIT_CALLEE_DECLINED_VERDICT_FIRST"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "callee-exception-rerun-raises", on_key: Some("CRATONVM_JIT_CALLEE_EXCEPTION_RERUN_RAISES"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "frameless-trap-identity", on_key: Some("CRATONVM_JIT_FRAMELESS_TRAP_IDENTITY"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "profile-save-atomic", on_key: Some("CRATONVM_JIT_PROFILE_SAVE_ATOMIC"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "field-array-bce", on_key: Some("CRATONVM_JIT_FIELD_ARRAY_BCE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "field-int-hoist", on_key: Some("CRATONVM_JIT_FIELD_INT_HOIST"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "sr-slots-past-licm", on_key: Some("CRATONVM_JIT_SR_SLOTS_PAST_LICM"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-splice-frame-states", on_key: Some("CRATONVM_JIT_IR_SPLICE_FRAME_STATES"), off_key: None, off_word: None, since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-splice-chain-fences", on_key: Some("CRATONVM_JIT_IR_SPLICE_CHAIN_FENCES"), off_key: None, off_word: None, since: "2026-09-28" },
    E { group: Group::JIT, token: "splice-chain-virtuals", on_key: Some("CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS"), off_key: None, off_word: None, since: "2026-09-28" },
    E { group: Group::JIT, token: "frameless-map-miss-fails", on_key: Some("CRATONVM_JIT_FRAMELESS_MAP_MISS_FAILS"), off_key: None, off_word: None, since: "2026-09-28" },
    E { group: Group::JIT, token: "ic-grace-at-stop", on_key: Some("CRATONVM_JIT_IC_GRACE_AT_STOP"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "mega-forget-dead-loaders", on_key: Some("CRATONVM_JIT_MEGA_FORGET_DEAD_LOADERS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ic-forget-dead-receivers", on_key: Some("CRATONVM_JIT_IC_FORGET_DEAD_RECEIVERS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ic-dead-ways-free-at-once", on_key: Some("CRATONVM_JIT_IC_DEAD_WAYS_FREE_AT_ONCE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ic-wide-stack-words", on_key: Some("CRATONVM_JIT_IC_WIDE_STACK_WORDS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "sp-ic-protected-sites", on_key: Some("CRATONVM_JIT_SP_IC_PROTECTED_SITES"), off_key: None, off_word: None, since: "2026-09-28" },
    E { group: Group::JIT, token: "ic-batch-retired-owners", on_key: Some("CRATONVM_JIT_IC_BATCH_RETIRED_OWNERS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "mic-install-wait-bound", on_key: Some("CRATONVM_JIT_MIC_INSTALL_WAIT_BOUND"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "mega-table-dir-reclaim", on_key: Some("CRATONVM_JIT_MEGA_TABLE_DIR_RECLAIM"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "stack-floor-word", on_key: Some("CRATONVM_JIT_STACK_FLOOR_WORD"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "self-locking-sync", on_key: Some("CRATONVM_JIT_SELF_LOCKING_SYNC"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "self-locking-sync-static", on_key: Some("CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "self-locking-sync-array-loops", on_key: Some("CRATONVM_JIT_SELF_LOCKING_SYNC_ARRAY_LOOPS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "sync-direct-class-mirror-slot", on_key: Some("CRATONVM_JIT_SYNC_DIRECT_CLASS_MIRROR_SLOT"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "inline-thin-lock-recursion", on_key: Some("CRATONVM_JIT_INLINE_THIN_LOCK_RECURSION"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "inline-inflated-recursion", on_key: Some("CRATONVM_JIT_INLINE_INFLATED_RECURSION"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "self-locking-sync-loops", on_key: Some("CRATONVM_JIT_SELF_LOCKING_SYNC_LOOPS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "self-locking-direct-bind", on_key: Some("CRATONVM_JIT_SELF_LOCKING_DIRECT_BIND"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "self-lock-refusal-memo", on_key: Some("CRATONVM_JIT_SELF_LOCK_REFUSAL_MEMO"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "self-locking-door-skip", on_key: Some("CRATONVM_JIT_SELF_LOCKING_DOOR_SKIP"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
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
    // gen r4w2/oldgen2: opt-in old-gen trigger hysteresis (a policy change,
    // so default OFF pending the A/B on its gap page). `non_empty_non_zero`.
    // gen r4w4/concmark4 (2026-09-24): the key is now also read as a TRI-STATE
    // (`GcFlags::old_trigger_hysteresis_explicit`) — unset takes the old-gen
    // policy's default (`OldGenPolicy::hysteresis`, OFF under both policies
    // today), and `=0` is an explicit off. THE DEFAULT FLIP must change this
    // row's `off_word` to `Some("0")` in the same commit: once the default is
    // on, `-old-trigger-hysteresis` has to WRITE the off word, not unset the
    // key (and the generated inventory then reads `default-on`, which is only
    // true after the flip).
    // gen r4w5 (2026-09-24): FLIPPED. Default ON under the concurrent-first
    // policy (the count-based cadence evidence is on `OldGenPolicy::hysteresis`),
    // so `-old-trigger-hysteresis` writes the off word `0`.
    E { group: Group::GC, token: "old-trigger-hysteresis", on_key: Some("CRATONVM_GC_OLD_TRIGGER_HYSTERESIS"), off_key: None, off_word: Some("0"), since: "2026-09-23" },
    // gen r4w4/concmark4 (2026-09-24): the generational old-gen policy. Default
    // ON (the concurrent cycle starts at an adaptive initiating occupancy below
    // the 75 % STW floor, and the STW old-gen collection defers to an open
    // cycle); `-concurrent-first` sets the opt-out key and restores the one
    // shared 75 % trigger. `non_empty_non_zero`.
    E { group: Group::GC, token: "concurrent-first", on_key: None, off_key: Some("CRATONVM_GC_NO_CONCURRENT_FIRST"), off_word: None, since: "2026-09-24" },
    // gen r4w4/concmark4: a fixed initiating occupancy (percent of the old
    // generation, clamped to [20, 75]) in place of the adaptive one. Carries a
    // value: `CRATONVM_GC=conc-start-percent=45`. Unset/0: adaptive.
    E { group: Group::GC, token: "conc-start-percent", on_key: Some("CRATONVM_GC_CONC_START_PERCENT"), off_key: None, off_word: None, since: "2026-09-24" },
    // gen r4w4/concmark4: the generational concurrent marker drains through a
    // marker-local stack (default); `-gen-conc-mark-local-stack` is the A/B arm
    // that sends every grey object through the sharded queue again.
    E { group: Group::GC, token: "gen-conc-mark-local-stack", on_key: None, off_key: Some("CRATONVM_GEN_CONC_MARK_NO_LOCAL_STACK"), off_word: None, since: "2026-09-24" },
    // gen r4w4/concmark4: the generational initial-mark pause scans the old-gen
    // objects frozen threads hold (closes the JIT SATB gate takeover window);
    // `-gen-conc-frozen-eager-scan` is the bisection kill switch.
    E { group: Group::GC, token: "gen-conc-frozen-eager-scan", on_key: None, off_key: Some("CRATONVM_GEN_CONC_NO_FROZEN_EAGER_SCAN"), off_word: None, since: "2026-09-24" },
    // gen r4w5/concmark5: opt-in concurrent-cycle SERVICE thread for the
    // generational heap (the cycle runs on it, not on the mutator whose
    // young-collection epilogue found it due). `non_empty_non_zero`.
    E { group: Group::GC, token: "gen-conc-service-thread", on_key: Some("CRATONVM_GEN_CONC_SERVICE_THREAD"), off_key: None, off_word: None, since: "2026-09-24" },
    // gen r4w5/concmark5: opt-in no-op remark-time reference-processing hook
    // (the call site only; counts itself, clears nothing). `non_empty_non_zero`.
    E { group: Group::GC, token: "gen-conc-remark-refproc-hook", on_key: Some("CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // gen r5w1/refs5: opt-in generational SATB young-value filter (the barrier
    // logs old-gen old values only). `non_empty_non_zero`.
    E { group: Group::GC, token: "gen-satb-old-only", on_key: Some("CRATONVM_GEN_SATB_OLD_ONLY"), off_key: None, off_word: None, since: "2026-09-26" },
    // gen r5w1/refs5: the allocation-failure collection door asks the
    // generational concurrent start trigger (and runs a due cycle inline), as
    // `maybe_gc`'s epilogue does. DEFAULT-ON since gen r5w2/conc6
    // (`on_unless_zero`), so a KILL SWITCH and `off_word` is `"0"`.
    E { group: Group::GC, token: "gen-conc-alloc-fail-door", on_key: Some("CRATONVM_GEN_CONC_ALLOC_FAIL_DOOR"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    // gen r5w3/unload7: opt-in concurrent class unloading on the generational
    // concurrent cycle (initial mark and remark under the class-unload root
    // licence, side-table edges in the marker, unload at remark). Needs
    // `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`. `non_empty_non_zero`.
    E { group: Group::GC, token: "gen-conc-class-unload", on_key: Some("CRATONVM_GEN_CONC_CLASS_UNLOAD"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    // gen r5w1/refs5: HotSpot `LRUMaxHeapPolicy` inputs for the SoftReference
    // policy (soft clock and free heap as of the previous collection). Every
    // backend. gen r5w4/defaults8: PER-BACKEND default
    // (`flags::alloc_policy_defaults::SOFTREF_HOTSPOT_LRU`: ON on the
    // Generational heap, OFF on G1 and ZGC), parsed by `backend_policy_switch`,
    // so the minus token must write an explicit `0` — unsetting the key would
    // leave the Generational default on. Hence `off_word: Some("0")`.
    E { group: Group::GC, token: "softref-hotspot-lru", on_key: Some("CRATONVM_SOFTREF_HOTSPOT_LRU"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    // gen r4w3/oldgen3: opt-in give-back of large free old-gen granules after
    // an in-place sweep (RSS vs re-fault cost; A/B pending). `non_empty_non_zero`.
    E { group: Group::GC, token: "old-give-back", on_key: Some("CRATONVM_GC_OLD_GIVE_BACK"), off_key: None, off_word: None, since: "2026-09-23" },
    // gen r4w4/oldgen4: default-ON decommit of the old generation's free tail
    // after a stop-the-world old-gen collection (HotSpot's MaxHeapFreeRatio);
    // `=0` is the kill switch. `on_unless_zero`.
    E { group: Group::GC, token: "old-shrink", on_key: Some("CRATONVM_GC_OLD_SHRINK"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // gen r4w4/oldgen4: opt-in PROACTIVE fragmentation-triggered old-gen
    // compaction (compaction itself is opt-in). `non_empty_non_zero`.
    E { group: Group::GC, token: "old-frag-compact", on_key: Some("CRATONVM_GC_OLD_FRAG_COMPACT"), off_key: None, off_word: None, since: "2026-09-24" },
    // gen r4w4/oldgen4: default-ON compaction of a fragmented old generation
    // after an old-gen allocation was refused for want of a contiguous block
    // (the false-OutOfMemoryError path); `=0` off. `on_unless_zero`.
    E { group: Group::GC, token: "old-oom-compact", on_key: Some("CRATONVM_GC_OLD_OOM_COMPACT"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // gcd d4/n: default-ON guard. A MOVING young cycle's Phase 5 major does not
    // slide old gen on a fragmentation request while compiled frames are live
    // (their unmapped words could name old objects); `=0` restores the slide.
    // `runtime_flag_default_on` (`gc::gen_heap::moving_major_jit_guard_enabled`).
    E { group: Group::GC, token: "moving-major-jit-guard", on_key: Some("CRATONVM_GC_MOVING_MAJOR_JIT_GUARD"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    // gcd d4/n: default-ON. A REQUESTED Generational major (`System.gc()`, the
    // allocation ladder) seeds young from the true roots instead of from every
    // young survivor, so dead old data a young object names is freed in the
    // same collection; `=0` keeps the legacy seed. `runtime_flag_default_on`
    // (`gc::gen_heap::full_gc_true_roots_enabled`).
    E { group: Group::GC, token: "full-gc-true-roots", on_key: Some("CRATONVM_GC_FULL_GC_TRUE_ROOTS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    // gcd d9/a: default-ON. The true-root seed above also on a PROMOTING cycle
    // (it resolves each promotion's young source to its destination instead
    // of rooting every destination, and drops a dead one from the pointer
    // map) and under a planned pinned compaction; `=0` restores the d4/n..d8
    // fallbacks to the legacy seed there. `runtime_flag_default_on`
    // (`gc::gen_heap::full_gc_true_roots_wide_enabled`).
    E { group: Group::GC, token: "full-gc-true-roots-wide", on_key: Some("CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    // gen r4w4/oldgen4: default-ON walk-gap recovery for the in-place old-gen
    // sweep (conservative scan of the unwalked bytes); `=0` restores the
    // fail-closed sweep. `on_unless_zero`.
    E { group: Group::GC, token: "old-walk-gap-recovery", on_key: Some("CRATONVM_GC_OLD_WALK_GAP_RECOVERY"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // gen r4w5/pinwords5: opt-in. The young collector's conservative-JIT-root
    // divert (term 4) stands down when this pause's young pin-word ledger is
    // complete and empty (`gc_quiescence::young_pin_ledger_clears_term4`).
    E { group: Group::GC, token: "gen-young-pin-ledger-term4", on_key: Some("CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4"), off_key: None, off_word: None, since: "2026-09-24" },
    // gen r4w5/oldcompact5: opt-in compaction AROUND pinned objects on the
    // non-moving old-gen path, answering a fragmentation request there
    // instead of discarding it. `non_empty_non_zero`.
    E { group: Group::GC, token: "old-pinned-compact", on_key: Some("CRATONVM_GC_OLD_PINNED_COMPACT"), off_key: None, off_word: None, since: "2026-09-24" },
    // gen r4w6/oldpin6: opt-in committed-size resize at the young pause after
    // a CONCURRENT old-gen cycle's sweep (the stop-the-world collections
    // already resize under `CRATONVM_GC_OLD_SHRINK`). `non_empty_non_zero`.
    E { group: Group::GC, token: "old-shrink-after-concurrent", on_key: Some("CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT"), off_key: None, off_word: None, since: "2026-09-24" },
    // gen r4w6/oldpin6: opt-in — a concurrent old-gen cycle whose walk
    // desynced sweeps anyway, treating the unwalked bytes as conservative
    // roots and rescanning every marked object. `non_empty_non_zero`.
    E { group: Group::GC, token: "conc-walk-gap-recovery", on_key: Some("CRATONVM_GC_CONC_WALK_GAP_RECOVERY"), off_key: None, off_word: None, since: "2026-09-24" },
    // gen r5w3/oldgen7: the `-Xms` startup commit split the way HotSpot
    // Serial sizes its initial heap (from-space + old first, the to-space only
    // one survivor's worth). DEFAULT-ON since gen r5w4/defaults8
    // (`flags::alloc_policy_defaults::GEN_XMS_USABLE_FIRST`, parsed by
    // `alloc_policy_switch`), so a KILL SWITCH and `off_word` is `"0"`.
    E { group: Group::GC, token: "gen-xms-usable-first", on_key: Some("CRATONVM_GEN_XMS_USABLE_FIRST"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    // gen r5w3/oldgen7: opt-in — decommit the old generation's large INTERIOR
    // free runs, not only its free tail, after a stop-the-world old-gen
    // collection. `non_empty_non_zero`.
    E { group: Group::GC, token: "old-interior-decommit", on_key: Some("CRATONVM_GC_OLD_INTERIOR_DECOMMIT"), off_key: None, off_word: None, since: "2026-09-26" },
    // gen r5w3/oldgen7: opt-in — the old generation may grow past its default
    // share to HotSpot's `MaxHeapSize - MaxNewSize` after a refused
    // allocation, the young trigger ceding the budget. `non_empty_non_zero`.
    E { group: Group::GC, token: "old-borrow-young", on_key: Some("CRATONVM_GC_OLD_BORROW_YOUNG"), off_key: None, off_word: None, since: "2026-09-26" },
    // gen r5w3/oldgen7, default-ON since the GC defects round: humongous arrays
    // are carved from the TOP of the old generation, committing only their own
    // pages. `on_unless_off_word_cased`.
    E { group: Group::GC, token: "old-humongous-top", on_key: Some("CRATONVM_GC_OLD_HUMONGOUS_TOP"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    // gen r5w5/old9: opt-in — the O(live) stop-the-world old-gen collection:
    // the block-offset table answers the mark's "is this an object base?",
    // the kept set is recorded by the mark, and the dead runs between kept
    // objects are freed without walking the generation. `runtime_flag_on`.
    E { group: Group::GC, token: "old-live-sweep", on_key: Some("CRATONVM_GC_OLD_LIVE_SWEEP"), off_key: None, off_word: None, since: "2026-09-27" },
    // gen r5w5/old9: with `old-live-sweep`, walk the generation anyway and
    // check the oracle and the kept set against it; a disagreement hands the
    // collection to the walked path. Prints `[old-live-sweep] verify`.
    E { group: Group::DBG, token: "old-live-sweep-verify", on_key: Some("CRATONVM_DBG_OLD_LIVE_SWEEP_VERIFY"), off_key: None, off_word: None, since: "2026-09-27" },
    // gen r5w5/old9: DEBUG — `=<n>` plants one unsizable header in a fresh,
    // unreferenced old-gen block at the end of the first stop-the-world
    // old-gen collection at or past the n-th, so the walk-gap recoveries can
    // be exercised on demand. Deliberately makes a heap anomaly; the token
    // form (`CRATONVM_DBG=old-plant-walk-break`) plants at the first.
    E { group: Group::DBG, token: "old-plant-walk-break", on_key: Some("CRATONVM_DBG_OLD_PLANT_WALK_BREAK"), off_key: None, off_word: None, since: "2026-09-27" },
    // gen r5w6/conc10: opt-in — the generational concurrent cycle's two pauses
    // take their young→old seeds from the LIVE young set (the young
    // collector's own, widened to every old holder), and, when the cycle
    // processes references at remark, not through the referent slot of an
    // active young `Reference`. Any doubt falls back to the all-young walk.
    // `runtime_flag_on` (`gen_heap::y2o_live_seed_enabled`).
    E { group: Group::GC, token: "gen-y2o-live-seed", on_key: Some("CRATONVM_GEN_Y2O_LIVE_SEED"), off_key: None, off_word: Some("0"), since: "2026-09-27" },
    // gen r5w6/conc10: opt-in — a Generational young-collection root scan under
    // the loader-conditional licence defers a YOUNG user-loader mirror as well
    // (reached through its loader's `mirror_pin` row), and forces the young
    // cycle in place when it did, so the mirror is no longer a pinned root
    // value and can be promoted. `runtime_flag_on`
    // (`vm::memory::roots::young_mirror_defer_licensed`).
    E { group: Group::GC, token: "gen-young-mirror-defer", on_key: Some("CRATONVM_GEN_YOUNG_MIRROR_DEFER"), off_key: None, off_word: Some("0"), since: "2026-09-27" },
    // gcd d2/g: opt-in — the non-moving young sweep's selective promotion no
    // longer pins a young root value whose every occurrence in the root list
    // came from a precise table the post-collection fix-up rewrites (statics,
    // interned strings, class mirrors, JNI globals), so such an object can be
    // promoted under a live JIT. `runtime_flag_on`
    // (`gc::gen_heap::precise_root_promote_enabled`).
    E { group: Group::GC, token: "gen-precise-root-promote", on_key: Some("CRATONVM_GEN_PRECISE_ROOT_PROMOTE"), off_key: None, off_word: None, since: "2026-09-27" },
    // gcd d9/e; default ON since gce (2026-09-29), `=0` is the kill switch —
    // an open Generational concurrent cycle takes precedence
    // over the STW old-gen collection until an old-gen allocation actually
    // fails (no 90 % ceiling), and a measured start prediction is no longer
    // held back by the start's growth hysteresis. `runtime_flag_on`
    // (`gc::concurrent_mark::gen_conc_precedence_enabled`).
    E { group: Group::GC, token: "gen-conc-precedence", on_key: Some("CRATONVM_GEN_CONC_PRECEDENCE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    // gcd d9/e; default ON since gce (2026-09-29), `=0` is the kill switch —
    // the Generational concurrent cycle's Phase-2 slices
    // judge markability against the initial mark's object-start snapshot
    // instead of re-walking the old generation after every promotion.
    // `runtime_flag_on` (`gc::concurrent_mark::gen_conc_mark_tams_starts_enabled`).
    E { group: Group::GC, token: "gen-conc-mark-tams-starts", on_key: Some("CRATONVM_GEN_CONC_MARK_TAMS_STARTS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    // gce e1/c: opt-in -- with no concurrent service attached, a direct
    // old-gen allocation latches a concurrent-start request that the next
    // interpreter allocation poll runs. `runtime_flag_on`
    // (`gc::gen_heap::poll_concurrent_start_after_direct_old_alloc`).
    E { group: Group::GC, token: "gen-conc-inline-start", on_key: Some("CRATONVM_GEN_CONC_INLINE_START"), off_key: None, off_word: None, since: "2026-09-29" },
    // gcd d9/e; default ON since gce (2026-09-29), `=0` is the kill switch —
    // the non-moving young sweep's selective promotion pins
    // only the finalizables it RESURRECTED, so a live finalizable object can
    // be tenured under the JIT. Declared by d9/e; the read site is the
    // young-copy owner's `sweep_young_non_moving`
    // (`docs/internal/gc/gcd-d9e-non-moving-sweep-pins-every-live-finalizable-young-DONE-20260929.md`).
    E { group: Group::GC, token: "gen-promote-live-finalizables", on_key: Some("CRATONVM_GEN_PROMOTE_LIVE_FINALIZABLES"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::GC, token: "overhead-limit", on_key: Some("CRATONVM_GC_OVERHEAD_LIMIT"), off_key: None, off_word: None, since: "2026-06-21" },
    // gen r4w5/thrash5: default-ON kill switch over the mutator-progress half
    // of the GC-overhead limit and the Generational "full collection, then the
    // error" exit of the interpreter's allocation ladder; `=0` restores both
    // (`runtime::interpreter::gc_overhead_progress_enabled`).
    E { group: Group::GC, token: "overhead-progress", on_key: Some("CRATONVM_GC_OVERHEAD_PROGRESS"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // gcd d2/j: default-ON liveness fix, `=0` restores the forced young cycle
    // per object-door entry; Generational only
    // (`runtime::interpreter::gc_and_alloc::futile_young_backoff_skips`).
    E { group: Group::GC, token: "futile-young-backoff", on_key: Some("CRATONVM_GC_FUTILE_YOUNG_BACKOFF"), off_key: None, off_word: Some("0"), since: "2026-09-27" },
    // gcd d10/o: opt-in (default OFF). The compiled refill trigger's occupancy
    // cycles (`tlab-alloc-shaped`) skip the GC-overhead productivity verdict,
    // as the interpreter's occupancy cycles do, so only allocation failures
    // feed the streak. `runtime_flag_on`
    // (`runtime::interpreter::gc_and_alloc::refill_trigger_cycles_unjudged`).
    E { group: Group::GC, token: "refill-trigger-unjudged", on_key: Some("CRATONVM_GC_REFILL_TRIGGER_UNJUDGED"), off_key: None, off_word: None, since: "2026-09-28" },
    // gce e2/o: default ON, `=0` restores the gen r4w5 exit. The latched
    // GC-overhead exit tries the allocation once more after its deciding major,
    // at most 8 times per latched episode (`gc_and_alloc::latched_overhead_grace`).
    E { group: Group::GC, token: "latched-grace", on_key: Some("CRATONVM_GC_LATCHED_GRACE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    // gcd d2/j: opt-in — a preallocated `OutOfMemoryError` per VM-raised
    // message (`vm::realms::heap_realm::PreallocatedOome`).
    E { group: Group::GC, token: "preallocated-oome-kinds", on_key: Some("CRATONVM_GC_PREALLOCATED_OOME_KINDS"), off_key: None, off_word: None, since: "2026-09-27" },
    E { group: Group::GC, token: "owner-class-filter", on_key: Some("CRATONVM_OWNER_CLASS_FILTER"), off_key: None, off_word: None, since: "2026-08-01" },
    E { group: Group::GC, token: "par-min-bytes", on_key: Some("CRATONVM_GC_PAR_MIN_BYTES"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::GC, token: "par-evac", on_key: Some("CRATONVM_GC_PAR_EVAC"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    // gce e2/o: `par-evac-bufferless`, `promote-pressure-expires` (gen r4w2)
    // and the three evac7 timing levers (`par-evac-targeted-wake`,
    // `par-evac-old-plab-hint`, `gen-evac-scan-prefetch`) are REMOVED -- no
    // gain in two rounds, bisection levers nobody runs (`e1-p-triage.md`
    // table 2B). `system-gc-moving-young` (below) is the one survivor of the
    // youngpolicy trio.
    // gen r5w3/evac7 (2026-09-26): three more opt-ins on the young copy,
    // presence-parsed, default OFF. `par-evac-card-seed` splits the dirty-card
    // roots across the parallel copy's workers (W2 of the scaling page); the
    // two `gen-pinned-*` rows are limits L1 (a parallel pinned in-place cycle,
    // which needs `gen-pinned-young-copy` too) and L2 (the tail window sized
    // from the allocated bytes) of `gengc-r4w5-pinned5-in-place-copy-limits`.
    E { group: Group::GC, token: "par-evac-card-seed", on_key: Some("CRATONVM_GC_PAR_EVAC_CARD_SEED"), off_key: None, off_word: None, since: "2026-09-26" },
    E { group: Group::GC, token: "gen-pinned-young-copy-parallel", on_key: Some("CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL"), off_key: None, off_word: None, since: "2026-09-26" },
    E { group: Group::GC, token: "gen-pinned-tail-window-live", on_key: Some("CRATONVM_GEN_PINNED_TAIL_WINDOW_LIVE"), off_key: None, off_word: None, since: "2026-09-26" },
    // gcd d3/m: the pinned in-place young copy (which needs
    // `gen-pinned-young-copy` too) also takes a young cycle whose only divert
    // is the take-over licence: every unrewritable peer a helper window the
    // pass pinned whole, each window's band read raw into the young pin
    // ledger. Opt-in (default OFF).
    E { group: Group::GC, token: "gen-pinned-young-copy-takeover", on_key: Some("CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER"), off_key: None, off_word: None, since: "2026-09-27" },
    E { group: Group::GC, token: "system-gc-moving-young", on_key: Some("CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG"), off_key: None, off_word: None, since: "2026-09-23" },
    // gen r4w6/young6 (2026-09-24): opt-in HotSpot-style adaptive tenuring for
    // the generational young collector (presence-parsed, default OFF). See
    // `gc/src/gen_heap_tenuring.rs`.
    E { group: Group::GC, token: "adaptive-tenuring", on_key: Some("CRATONVM_GC_ADAPTIVE_TENURING"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    E { group: Group::GC, token: "par-threads", on_key: Some("CRATONVM_GC_PAR_THREADS"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::GC, token: "sync-young-wipe", on_key: Some("CRATONVM_GC_SYNC_YOUNG_WIPE"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::GC, token: "jit-ref-store-gates", on_key: Some("CRATONVM_GC_JIT_REF_STORE_GATES"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    // gen r4w3/cards3 (2026-09-23): three default-ON A/B levers for the
    // generational remembered set, each `on_unless_zero`, so
    // `CRATONVM_GC=-token` writes `0`. `card-summary`: the card table's
    // one-byte-per-64-cards summary map. `old-bot`: the old generation's
    // block-offset table behind the dirty-card object walk. `precise-array-
    // cards`: `set_array_element` dirties the written element's card, not the
    // array header's. G1 and ZGC are unaffected by all three.
    E { group: Group::GC, token: "card-summary", on_key: Some("CRATONVM_GC_CARD_SUMMARY"), off_key: None, off_word: Some("0"), since: "2026-09-23" },
    E { group: Group::GC, token: "old-bot", on_key: Some("CRATONVM_GC_OLD_BOT"), off_key: None, off_word: Some("0"), since: "2026-09-23" },
    E { group: Group::GC, token: "precise-array-cards", on_key: Some("CRATONVM_GC_PRECISE_ARRAY_CARDS"), off_key: None, off_word: Some("0"), since: "2026-09-23" },
    // gcd d5/s: opt-IN. On an element-precise table, the young pause's
    // dirty-card scan trusts ELEMENT cards for a reference array whose header
    // card is dirty too: it reads only the dirty ranges the array overlaps,
    // not the whole array (cards3 item 3's consumer). `runtime_flag_on`
    // (`gc::gen_heap::scan_dirty_cards_inner`).
    E { group: Group::GC, token: "precise-array-header-card", on_key: Some("CRATONVM_GC_PRECISE_ARRAY_HEADER_CARD"), off_key: None, off_word: None, since: "2026-09-28" },
    // gcd d9/c: default ON, `=0` is the kill switch. After a young sweep whose
    // promotion was gated off (a refusing helper window), the Generational
    // heap reports young's drain as BLOCKED, so the young trigger's
    // anti-livelock floor and the futile-young verdict stop assuming that old
    // gen's room lets a young cycle progress. `runtime_flag_default_on`
    // (`gc::gen_heap::GenerationalHeap::young_drain_blocked`).
    E { group: Group::GC, token: "young-drain-blocked", on_key: Some("CRATONVM_GC_YOUNG_DRAIN_BLOCKED"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
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
    // Opt-in (gce e1/y, 2026-09-29): a moving young cycle keeps the next
    // to-space's survivor prefix (x 1.25) committed; read through
    // `runtime_flag_on` (`gen_heap::uncommit_keep_for_survivors`).
    E { group: Group::GC, token: "gen-uncommit-keep-survivors", on_key: Some("CRATONVM_GEN_UNCOMMIT_KEEP_SURVIVORS"), off_key: None, off_word: None, since: "2026-09-29" },
    // gce e2/o: `gen-pristine-chunks` (subsumed by zero-once), `tlab-size-retired`
    // and `tlab-waste-shrink` (superseded by the share sizer) are REMOVED
    // (`e1-p-triage.md` table 2B).
    // Opt-in (gen r4w3/alloc3, 2026-09-23), two TLAB arms, each default OFF
    // until its A/B (protocols in
    // `docs/internal/reviews/gengc-round4-w3-alloc3-20260923.md`):
    // * `tlab-filler-skip-zero` -- a Java thread's retire writes the tail
    //   filler header but not the memset of its (already zero) data area;
    // * `tlab-gate-bump-floor` -- the JIT refill gate probes the young bump
    //   tail for the fragmentation floor instead of the full request.
    E { group: Group::GC, token: "tlab-filler-skip-zero", on_key: Some("CRATONVM_TLAB_FILLER_SKIP_ZERO"), off_key: None, off_word: None, since: "2026-09-23" },
    E { group: Group::GC, token: "tlab-gate-bump-floor", on_key: Some("CRATONVM_TLAB_GATE_BUMP_FLOOR"), off_key: None, off_word: None, since: "2026-09-23" },
    // gen r4w4/alloc4 (2026-09-24). Every allocation-policy arm (the rows
    // above and these three) now parses against its default in
    // `flags::alloc_policy_defaults`; a default flip is that constant plus
    // this table's `off_word` (`Some("0")` for a default-ON arm).
    // * `tlab-share-sizer` -- the HotSpot-shaped per-thread TLAB sizer
    //   (share of young allocation per collection, exponentially averaged,
    //   1/50 of it per refill) and the refill-waste limit. gen r5w4/defaults8
    //   made its default PER-BACKEND (`alloc_policy_defaults::TLAB_SHARE_SIZER`);
    //   the Generational flip was reverted at the r5w4 merge, so it is OFF on
    //   every backend and `off_word` is `"0"` only so that a future
    //   Generational flip needs no row change;
    // * `gen-zero-once` -- default ON: a young hand-out from the bump tail is
    //   not zeroed again (the young wipe or a fresh commit already did);
    // * `gen-humongous-zero-unlocked` -- default ON: a humongous primitive
    //   array's body is zeroed after the old-generation lock drops.
    E { group: Group::GC, token: "tlab-share-sizer", on_key: Some("CRATONVM_TLAB_SHARE_SIZER"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // gen r5w5/sizer9 (2026-09-27), a VALUE knob read like
    // `CRATONVM_GEN_CONC_MARK_SLICE`: the share sizer's largest desired size in
    // KiB (clamped to 8..1024; unset = 1024, the historical cap). A bisection
    // lever for the share-sizer page's mechanism A, which shows only once the
    // sizer grows buffers past the ladder's 256 KiB baseline.
    E { group: Group::GC, token: "tlab-share-sizer-max-kib", on_key: Some("CRATONVM_TLAB_SHARE_SIZER_MAX_KIB"), off_key: None, off_word: None, since: "2026-09-27" },
    E { group: Group::GC, token: "gen-zero-once", on_key: Some("CRATONVM_GEN_ZERO_ONCE"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // Opt-in (gce e1/y, 2026-09-29): the zero-once skip also covers a
    // free-list hand-out inside a span the last sweep zeroed
    // (`GcFlags::gen_zero_once_free_list`).
    E { group: Group::GC, token: "gen-zero-once-free-list", on_key: Some("CRATONVM_GEN_ZERO_ONCE_FREE_LIST"), off_key: None, off_word: None, since: "2026-09-29" },
    E { group: Group::GC, token: "gen-humongous-zero-unlocked", on_key: Some("CRATONVM_GEN_HUMONGOUS_ZERO_UNLOCKED"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // gen r4w6/tlab6 (2026-09-24), opt-in, an allocation-policy arm (default in
    // `flags::alloc_policy_defaults::GEN_TLAB_TAIL_SINK`): a retiring TLAB's
    // unused tail goes back to the generational young from-space (cursor
    // retraction or the young free list) instead of under an `int[]` filler.
    E { group: Group::GC, token: "gen-tlab-tail-sink", on_key: Some("CRATONVM_GEN_TLAB_TAIL_SINK"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // gen r5w1/oldgen5 (2026-09-26), opt-in, an allocation-policy arm (default
    // in `flags::alloc_policy_defaults::GEN_HUMONGOUS_REF_ZERO_UNLOCKED`): a
    // humongous REFERENCE array's body is zeroed after the old-generation lock
    // drops, behind a same-footprint `long[]` header until it is zero.
    E { group: Group::GC, token: "gen-humongous-ref-zero-unlocked", on_key: Some("CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED"), off_key: None, off_word: None, since: "2026-09-26" },
    // gen r4w5/pinned5 (2026-09-24): opt-in, presence-parsed, default OFF. A
    // young cycle the conservative-JIT-root term would divert copies instead,
    // leaving the pages its unrewritable conservative words land in in place.
    E { group: Group::GC, token: "gen-pinned-young-copy", on_key: Some("CRATONVM_GEN_PINNED_YOUNG_COPY"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
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
    // BOTH became DEFAULT-ON on 2026-08-13, which is when both grew an
    // `off_word` -- without it `CRATONVM_GC=-zgc-parmark` expands to
    // UNSETTING the key, and an unset key now means ON. `parmark` is a
    // worker count whose parser reads 0 as serial; `relocate` reads
    // 0/off/false/no as off.
    //
    // `parmark` did not stay there. `Z_PARMARK_DEFAULT_WORKERS` (`gc/src/zgc.rs`)
    // was reverted to `0` on 2026-08-14 -- one day after this row was
    // written -- when the driven-cycle measurement came back +31% at one
    // worker and +153% at four; `docs/GC.md` and the code's own
    // `relocation_requested_by_default` neighbour comment have said "opt-in"
    // ever since. This row and the kill-switch test below did not follow:
    // `off_word: Some("0")` is what makes the generated inventory call an
    // opt-in "default-on", so it comes off here, the same shape as
    // `zgc-conc-start` and `zgc-generational` below, which are also
    // numeric/boolean opt-ins with no negative spelling needed. `relocate`
    // never reverted and keeps its `off_word`.
    E { group: Group::GC, token: "zgc-parmark", on_key: Some("CRATONVM_ZGC_PARMARK"), off_key: None, off_word: None, since: "2026-08-13" },
    E { group: Group::GC, token: "zgc-relocate", on_key: Some("CRATONVM_ZGC_RELOCATE"), off_key: None, off_word: Some("0"), since: "2026-08-13" },
    E { group: Group::GC, token: "zgc-relocate-cost-gate", on_key: Some("CRATONVM_ZGC_RELOCATE_COST_GATE"), off_key: None, off_word: Some("0"), since: "2026-09-18" },
    E { group: Group::GC, token: "zgc-readable-give-back", on_key: Some("CRATONVM_ZGC_READABLE_GIVE_BACK"), off_key: None, off_word: Some("0"), since: "2026-09-19" },
    E { group: Group::GC, token: "zgc-movable-commit-screen", on_key: Some("CRATONVM_ZGC_MOVABLE_COMMIT_SCREEN"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::GC, token: "zgc-pristine-chunks", on_key: Some("CRATONVM_ZGC_PRISTINE_CHUNKS"), off_key: None, off_word: Some("0"), since: "2026-09-19" },
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
    E { group: Group::GC, token: "zgc-conc-start", on_key: Some("CRATONVM_ZGC_CONC_START"), off_key: None, off_word: None, since: "2026-08-16" },
    E { group: Group::GC, token: "zgc-conc-workers", on_key: Some("CRATONVM_ZGC_CONC_WORKERS"), off_key: None, off_word: None, since: "2026-08-16" },
    // Phase G. `zgc-generational` is a boolean with a real `off` word; the other
    // two are numeric tunables with no negative spelling, like `zgc-parmark`.
    E { group: Group::GC, token: "zgc-generational", on_key: Some("CRATONVM_ZGC_GENERATIONAL"), off_key: None, off_word: None, since: "2026-08-17" },
    E { group: Group::GC, token: "zgc-gen-promotion-age", on_key: Some("CRATONVM_ZGC_GEN_PROMOTION_AGE"), off_key: None, off_word: None, since: "2026-08-17" },
    E { group: Group::GC, token: "zgc-gen-minors-per-major", on_key: Some("CRATONVM_ZGC_GEN_MINORS_PER_MAJOR"), off_key: None, off_word: None, since: "2026-08-17" },
    E { group: Group::GC, token: "zgc-gen-nursery-percent", on_key: Some("CRATONVM_ZGC_GEN_NURSERY_PERCENT"), off_key: None, off_word: Some("0"), since: "2026-08-17" },
    E { group: Group::GC, token: "zgc-alloc-trigger", on_key: Some("CRATONVM_ZGC_ALLOC_TRIGGER"), off_key: None, off_word: Some("0"), since: "2026-09-03" },
    E { group: Group::GC, token: "zgc-pause-target-ms", on_key: Some("CRATONVM_ZGC_PAUSE_TARGET_MS"), off_key: None, off_word: Some("0"), since: "2026-09-03" },
    E { group: Group::GC, token: "zgc-bitmap-bounds", on_key: Some("CRATONVM_ZGC_BITMAP_BOUNDS"), off_key: None, off_word: Some("0"), since: "2026-09-03" },
    // The 2026-09-20 ZGC design-and-performance round. Nine knobs that existed,
    // parsed and took effect for a whole round before they were declared here --
    // `flags::runtime_var` falls back to `std::env::var` for an undeclared name,
    // so nothing failed and nothing said so. What they lacked was the
    // `CRATONVM_GC=+<token>` spelling, a row in `--help` and the two generated
    // docs, and a machine-readable `since:`. Two of them change how often the
    // DEFAULT collector runs, and `zgc-vm-tlab-share` is the kill switch for a
    // fix to a whole-heap-exhaustion regression -- a kill switch nobody can find
    // is not a kill switch.
    //
    // `zgc-vm-tlab-share` is the only default-ON member (`runtime_flag_
    // default_on`), so it is the only one taking the `off_word: Some("0")`
    // shape its default-on neighbours above use. EVERY OTHER ROW HERE IS
    // default-OFF and therefore takes `off_word: None` -- "off is expressed by
    // unsetting `on_key`", per the field's own doc. This matters because
    // `off_word` is what `render-inventory.py` reads to print the Default
    // column: a default-OFF knob given `Some("0")` is published as
    // "default-on", which is the opposite of true. The mistake was made once
    // in this very round (all eight rows landed as `Some("0")` and were
    // corrected on 2026-09-21) and once before it -- see the note on the
    // `ir-drop-unreachable-homes` row above, which records round 9 wave 2
    // making it and fixing it. `zgc-mark-ref-chunk` and `zgc-tlab-reserved-bytes` are
    // numeric rather than boolean, like `zgc-conc-workers` above.
    E { group: Group::GC, token: "zgc-vm-tlab-share", on_key: Some("CRATONVM_ZGC_VM_TLAB_SHARE"), off_key: None, off_word: Some("0"), since: "2026-09-20" },
    E { group: Group::GC, token: "zgc-headroom-bypasses-rearm", on_key: Some("CRATONVM_ZGC_HEADROOM_BYPASSES_REARM"), off_key: None, off_word: None, since: "2026-09-20" },
    E { group: Group::GC, token: "zgc-census", on_key: Some("CRATONVM_ZGC_CENSUS"), off_key: None, off_word: None, since: "2026-09-20" },
    E { group: Group::GC, token: "zgc-census-access", on_key: Some("CRATONVM_ZGC_CENSUS_ACCESS"), off_key: None, off_word: None, since: "2026-09-20" },
    E { group: Group::GC, token: "zgc-pause-target-includes-relocate", on_key: Some("CRATONVM_ZGC_PAUSE_TARGET_INCLUDES_RELOCATE"), off_key: None, off_word: None, since: "2026-09-20" },
    E { group: Group::GC, token: "zgc-metrics", on_key: Some("CRATONVM_ZGC_METRICS"), off_key: None, off_word: None, since: "2026-09-20" },
    E { group: Group::GC, token: "zgc-trigger-shadow", on_key: Some("CRATONVM_ZGC_TRIGGER_SHADOW"), off_key: None, off_word: None, since: "2026-09-20" },
    E { group: Group::GC, token: "zgc-tlab-reserved-bytes", on_key: Some("CRATONVM_ZGC_TLAB_RESERVED_BYTES"), off_key: None, off_word: None, since: "2026-09-20" },
    E { group: Group::GC, token: "zgc-mark-ref-chunk", on_key: Some("CRATONVM_ZGC_MARK_REF_CHUNK"), off_key: None, off_word: None, since: "2026-09-20" },
    // Wave 3. The page-based evacuation arm: an `impl ZRelocateContext for
    // ZgcRealHeap` now exists, so `zgc::relocate` is no longer unreachable, but
    // nothing drives it -- `alloc_in` returns `None` pending a bump-only
    // to-space, because `Arena::alloc` serves the free list the sweep has just
    // filled with the relocation set's own holes, so a destination could alias
    // its source. Declared so the knob is discoverable and dated; it currently
    // switches nothing, and that is said at its read site too.
    E { group: Group::GC, token: "zgc-page-evac", on_key: Some("CRATONVM_ZGC_PAGE_EVAC"), off_key: None, off_word: None, since: "2026-09-21" },
    // Wave 4, C0 stage 0. `page.rs` builds a SURVEY over the arena's existing
    // reservation -- an allocator over a window it does not own, which indexes
    // and reports but refuses to hand out memory (`ZPageError::SurveyOnly`).
    // It exists so the page-based allocator becomes reachable and measurable
    // before it becomes load-bearing; stage 1 routes the TLAB chunk source
    // through it. Default OFF, hence `off_word: None`.
    E { group: Group::GC, token: "zgc-page-survey", on_key: Some("CRATONVM_ZGC_PAGE_SURVEY"), off_key: None, off_word: None, since: "2026-09-21" },
    // Wave 4. One extra streaming pass over the mark and object-start bitmaps
    // (no header reads, no writes) reporting the WHOLLY-DEAD-PAGE FRACTION on
    // `[GC] zgc-sweep-pages:`. That number is the one proposal B's O(pages)
    // sweep rests on and it has never been measured -- the sweep is the
    // dominant pause component, so whether page-granular reclamation can help
    // is currently an assumption. Default OFF, hence `off_word: None`.
    E { group: Group::GC, token: "zgc-sweep-page-census", on_key: Some("CRATONVM_ZGC_SWEEP_PAGE_CENSUS"), off_key: None, off_word: None, since: "2026-09-21" },
    // Wave 4. Arms the extra-root bloom ACROSS the concurrent phase instead of
    // only across the stop-the-world mark. The filter's `false` is a PROOF that
    // an object owns no extra roots, so arming it while roots can still appear
    // would drop live edges -- which is why one load gates all four halves
    // (arm, harvest, repair, refusal) and you cannot set one without the
    // others. The per-drain revalidation (stage 3c) is deliberately absent: it
    // is a bound on the repair set, not a proof, and wiring it needs a
    // REQUIRED `ZMarkContext` method rather than a defaulted one -- a defaulted
    // one on a delegating wrapper answers "still valid" forever, which here is
    // a dropped live edge. Default OFF, hence `off_word: None`.
    E { group: Group::GC, token: "zgc-mark-root-filter-conc", on_key: Some("CRATONVM_ZGC_MARK_ROOT_FILTER_CONC"), off_key: None, off_word: None, since: "2026-09-21" },
    // Wave 5. The slide's EVACUATION BUDGET, in MiB of live bytes -- the only
    // assignable spelling of `ZRelocationPolicy::max_evacuation_bytes`, which
    // was a hard 64 MiB constant that nothing in the workspace ever wrote.
    // `ZRelocationSet::select` is a prefix rule over it, so on a workload whose
    // low-region live set exceeds the budget a "compaction cycle" compacts a
    // prefix and stops, and the constant that decides how much is unrelated to
    // `-Xmx`. Numeric, so `off_word: None` like `zgc-conc-workers`; `0` means
    // UNLIMITED rather than "select nothing", because a no-compaction arm is
    // already `zgc-relocate=0` and spelling it twice would produce a silent
    // whole-heap decline attributed to nothing. THE DEFAULT IS UNCHANGED at
    // `ZFWD_DEFAULT_MAX_EVACUATION_BYTES`; this is a knob before a default
    // change, so the A/B is a same-binary pair of arms. Read
    // `reloc_budget_truncated_cycles` on `[GC] zgc-features:` before setting
    // it -- a zero there means the budget is not binding and the knob measures
    // nothing.
    E { group: Group::GC, token: "zgc-relocate-budget-mb", on_key: Some("CRATONVM_ZGC_RELOCATE_BUDGET_MB"), off_key: None, off_word: None, since: "2026-09-21" },
    // Wave 5. The machine-readable half of `zgc::metrics`. `to_tsv_row` and
    // `tsv_header` (58 columns) existed from the module's first day and were
    // emitted by nothing, which is why the full-suite ZGC-versus-Generational
    // join `tsv_header`'s doc describes has never been run. `...METRICS_TSV` is
    // the path to append one row per VM to; `...METRICS_RUN` is the `run`
    // column, i.e. the join key. Both take values, so `off_word: None`.
    // Separate from `zgc-metrics`, which only controls the two HUMAN-readable
    // lines: a harness wants the row without 1975 logs' worth of per-cycle
    // stderr, and an operator reading a pause wants the lines without a file.
    E { group: Group::GC, token: "zgc-metrics-tsv", on_key: Some("CRATONVM_ZGC_METRICS_TSV"), off_key: None, off_word: None, since: "2026-09-21" },
    E { group: Group::GC, token: "zgc-metrics-run", on_key: Some("CRATONVM_ZGC_METRICS_RUN"), off_key: None, off_word: None, since: "2026-09-21" },
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
    // gcd d5/q: opt-in. The Generational non-moving young SWEEP is triggered
    // by its own pause loop (90 % start, `capacity/16` floor) instead of the
    // moving collector's pause-goal-lowered threshold; latched per heap.
    // `runtime_flag_on` (`gc::gen_heap::gen_sweep_trigger_own_goal_enabled`).
    E { group: Group::GC, token: "gen-sweep-trigger-own-goal", on_key: Some("CRATONVM_GEN_SWEEP_TRIGGER_OWN_GOAL"), off_key: None, off_word: None, since: "2026-09-28" },
    // gcd d9/b: default ON, `0` off. While the old generation is wedged, the
    // Generational young trigger's post-collection floor rises to the
    // non-moving (90 %) trigger instead of re-firing at the moving 50 % one
    // after a TLAB carve; latched per heap. `runtime_flag_default_on`
    // (`gc::gen_heap::gen_wedged_young_trigger_enabled`).
    E { group: Group::GC, token: "gen-wedged-young-trigger", on_key: Some("CRATONVM_GEN_WEDGED_YOUNG_TRIGGER"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    // Default-ON: the key's PRESENCE makes the trusted `*_validated` header
    // accessors re-validate, which is the pre-"validate once" behaviour.
    E { group: Group::GC, token: "validate-once", on_key: None, off_key: Some("CRATONVM_GC_NO_VALIDATE_ONCE"), off_word: None, since: "2026-08-20" },
    // Round 9 wave 5 (2026-09-21): the two GC-side halves of the three names
    // `flag_declaration_guard` was red on. Both were ALREADY read through
    // `flags::runtime_var_os` -- they were simply named nowhere, so the
    // fall-through in `runtime_var_os` served each from a live `getenv`
    // instead of the latched snapshot: `CRATONVM_GC=token` could not reach
    // them and `with_process_overrides` could not arrange one in a test.
    // Declaring them changes the LOOKUP, not the answer, and the answer is
    // unchanged here for a checkable reason: neither name is written by
    // anything in this tree. `set_var` is never called on either (nor is
    // either set by any script, CI file or test fixture), so the snapshot and
    // a live `getenv` agree at every read, and with both unset each site keeps
    // the default it had.
    //
    // Both take the `validate-once` shape directly above rather than an
    // `off_word`: each is a `NO_`-spelled OPT-OUT whose consumer tests the
    // key's PRESENCE, so `off_key` is the name and `off_word` must stay `None`
    // -- writing `"0"` into a presence-parsed key leaves it set and would make
    // the grouped spelling that means "off" turn the knob ON, which is the
    // 23-row defect this round already fixed elsewhere in this table.
    //
    // `read-cursor` is the GC row its own call site asks for: `vm_exec.rs`
    // calls the region "the validate-once read region" and says in as many
    // words that it is "the same shape as `CRATONVM_GC_NO_VALIDATE_ONCE`".
    // What it gates is the `is_object_address` heap-membership probe inside
    // `load_and_forward` -- a GC question -- so it belongs here and not with
    // the other `NATIVE`-prefixed rows, which live in `JIT` because they are
    // native-intrinsic dispatch knobs and share nothing but a prefix. (The
    // prefix is what makes this row worth a comment: picking the group from
    // the variable's NAME would have put a heap-membership switch in `JIT`.)
    // Default ON (the cursor is
    // trusted); `CRATONVM_GC=-read-cursor` makes every cursor untrusted and
    // restores one membership probe per accessor call, which is the A/B a
    // check added for a reproduced crash is entitled to without a rebuild.
    E { group: Group::GC, token: "read-cursor", on_key: None, off_key: Some("CRATONVM_NATIVE_NO_READ_CURSOR"), off_word: None, since: "2026-09-21" },
    // `refproc-prepass` is the kill switch for the 2026-09-21 reordering that
    // runs `remove_collected` BEFORE reference processing on the generational
    // path. Default ON; `CRATONVM_GC=-refproc-prepass` restores the old order,
    // in which a `SoftReference` whose own instance died in the cycle again
    // roots the weak-clearing closure. A bisect lever, not a tuning knob --
    // and scoped to the generational path, with G1 and ZGC byte-for-byte
    // unchanged either way.
    E { group: Group::GC, token: "refproc-prepass", on_key: None, off_key: Some("CRATONVM_GC_NO_REFPROC_PREPASS"), off_word: None, since: "2026-09-21" },
    // gen r4w2/concmark (2026-09-23): objects per Phase-2 slice of the
    // GENERATIONAL concurrent old-gen cycle (`gc/src/concurrent_mark.rs::
    // gen_conc_mark_slice`). Default ON (sliced, 32 Ki objects, with the
    // old-gen lock released and a safepoint polled between slices and a lost
    // remark retried); `CRATONVM_GC=-gen-conc-mark-slice` writes `0`, which
    // restores the one-lock-hold driver as the A/B arm. The bare token writes
    // `1` — one object per slice, a stress setting; set the variable itself to
    // tune. G1 and ZGC are unaffected either way.
    E { group: Group::GC, token: "gen-conc-mark-slice", on_key: Some("CRATONVM_GEN_CONC_MARK_SLICE"), off_key: None, off_word: Some("0"), since: "2026-09-23" },
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
    E { group: Group::LOADER, token: "unretire-native-shadow", on_key: Some("CRATONVM_UNRETIRE_NATIVE_SHADOW"), off_key: None, off_word: None, since: "2026-09-11" },
    E { group: Group::LOADER, token: "compat-collectors-unmodifiable", on_key: Some("CRATONVM_COMPAT_COLLECTORS_UNMODIFIABLE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::LOADER, token: "compat-copyof-real-immutable-identity", on_key: Some("CRATONVM_COMPAT_COPYOF_REAL_IMMUTABLE_IDENTITY"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::LOADER, token: "compat-lhm-reversed-real-view", on_key: Some("CRATONVM_COMPAT_LHM_REVERSED_REAL_VIEW"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::LOADER, token: "compat-lhm-reversed-live-views", on_key: Some("CRATONVM_COMPAT_LHM_REVERSED_LIVE_VIEWS"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::LOADER, token: "compat-lhm-reversed-live-values", on_key: Some("CRATONVM_COMPAT_LHM_REVERSED_LIVE_VALUES"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::LOADER, token: "compat-lhs-reversed-real-view", on_key: Some("CRATONVM_COMPAT_LHS_REVERSED_REAL_VIEW"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::LOADER, token: "compat-optional-real-bytecode", on_key: Some("CRATONVM_COMPAT_OPTIONAL_REAL_BYTECODE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::LOADER, token: "retired-shadow-masks-ancestor-bridges", on_key: Some("CRATONVM_RETIRED_SHADOW_MASKS_ANCESTOR_BRIDGES"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
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
    // 2026-09-17, from the quarkus lane. Takes a VALUE, not a presence: the
    // number of jar archives the manifest reader keeps open, default 256, for
    // hosts whose `ulimit -n` is much higher or lower. A value that does not
    // parse, or parses to 0, falls back to the default rather than uncapping.
    E { group: Group::IO, token: "canon-openfile", on_key: Some("CRATONVM_CANON_OPENFILE"), off_key: None, off_word: None, since: "2026-06-30" },
    E { group: Group::IO, token: "http-max-body", on_key: Some("CRATONVM_HTTP_MAX_BODY"), off_key: None, off_word: None, since: "2026-06-17" },
    E { group: Group::IO, token: "netty-queue-bridge", on_key: Some("CRATONVM_NETTY_QUEUE_BRIDGE"), off_key: None, off_word: Some("0"), since: "2026-07-08" },
    E { group: Group::IO, token: "resolve-outbound-host", on_key: Some("CRATONVM_RESOLVE_OUTBOUND_HOST"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::IO, token: "select-max-block-ms", on_key: Some("CRATONVM_SELECT_MAX_BLOCK_MS"), off_key: None, off_word: None, since: "2026-06-18" },
    E { group: Group::IO, token: "selector-connect-probe", on_key: None, off_key: Some("CRATONVM_NO_SELECTOR_CONNECT_PROBE"), off_word: None, since: "2026-06-24" },
    E { group: Group::IO, token: "net-event-waits", on_key: Some("CRATONVM_NET_EVENT_WAITS"), off_key: None, off_word: Some("0"), since: "2026-09-12" },
    E { group: Group::IO, token: "net-jdk-backlog", on_key: Some("CRATONVM_NET_JDK_BACKLOG"), off_key: None, off_word: Some("0"), since: "2026-09-12" },
    E { group: Group::IO, token: "jar-archive-cache-cap", on_key: Some("CRATONVM_JAR_ARCHIVE_CACHE_CAP"), off_key: None, off_word: None, since: "2026-09-17" },
    E { group: Group::IO, token: "httpsrv-keepalive", on_key: Some("CRATONVM_HTTPSRV_KEEPALIVE"), off_key: None, off_word: Some("0"), since: "2026-09-12" },
    E { group: Group::IO, token: "net-close-skip-shutdown", on_key: Some("CRATONVM_NET_CLOSE_SKIP_SHUTDOWN"), off_key: None, off_word: Some("0"), since: "2026-09-12" },
    E { group: Group::IO, token: "socket-capture", on_key: Some("CRATONVM_SOCKET_CAPTURE"), off_key: None, off_word: None, since: "2026-06-12" },
    E { group: Group::IO, token: "uri-strict-chars", on_key: Some("CRATONVM_URI_STRICT_CHARS"), off_key: None, off_word: Some("0"), since: "2026-06-24" },
    E { group: Group::IO, token: "zip-max-entry-bytes", on_key: Some("CRATONVM_ZIP_MAX_ENTRY_BYTES"), off_key: None, off_word: None, since: "2026-05-22" },
    // `ThreadMXBean.getLockedSynchronizers` support; DEFAULT-ON, so a kill
    // switch. THREADS rather than a JMX group because the group vocabulary
    // has no JMX and this is a threading capability the bean exposes.
    E { group: Group::THREADS, token: "jmx-owned-synchronizers", on_key: Some("CRATONVM_JMX_OWNED_SYNCHRONIZERS"), off_key: None, off_word: Some("0"), since: "2026-08-27" },
    // 2026-09-08. Default-ON kill switch over the uncontended monitorenter /
    // monitorexit fast path (peek-first opcode operand, per-thread cached JMX
    // monitor book). `0` restores the previous path in the SAME binary, which is
    // what `probes/SyncCost.java` needs: a sequential pair of builds on a shared
    // host is not a measurement. Behaviour is identical either way --
    // `probes/JmxMonitorOwnership.java` is green on both.
    E { group: Group::THREADS, token: "monitor-fastpath", on_key: Some("CRATONVM_MONITOR_FASTPATH"), off_key: None, off_word: Some("0"), since: "2026-09-08" },
    E { group: Group::THREADS, token: "monitor-quiet-release", on_key: Some("CRATONVM_MONITOR_QUIET_RELEASE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::THREADS, token: "monitor-spin-backoff", on_key: Some("CRATONVM_MONITOR_SPIN_BACKOFF"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::THREADS, token: "monitor-spin-handover-abort", on_key: Some("CRATONVM_MONITOR_SPIN_HANDOVER_ABORT"), off_key: None, off_word: None, since: "2026-09-29" },
    // gen r4w5/thrash5 (2026-09-24). Default-ON kill switch over the bounded
    // spin a contended `monitorenter` takes before it parks (and retires its
    // TLAB); `0` restores the park-at-once path
    // (`threading::monitor::monitor_enter_spin_enabled`).
    E { group: Group::THREADS, token: "monitor-enter-spin", on_key: Some("CRATONVM_MONITOR_ENTER_SPIN"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    E { group: Group::THREADS, token: "monitor-spin-after-wake", on_key: Some("CRATONVM_MONITOR_SPIN_AFTER_WAKE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::THREADS, token: "monitor-wait-reacquire-spin", on_key: Some("CRATONVM_MONITOR_WAIT_REACQUIRE_SPIN"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::THREADS, token: "monitor-cached-notify", on_key: Some("CRATONVM_MONITOR_CACHED_NOTIFY"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::THREADS, token: "monitor-thin-notify", on_key: Some("CRATONVM_MONITOR_THIN_NOTIFY"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::THREADS, token: "monitor-index-prune", on_key: Some("CRATONVM_MONITOR_INDEX_PRUNE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::THREADS, token: "monitor-lazy-spinners", on_key: Some("CRATONVM_MONITOR_LAZY_SPINNERS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
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
    E { group: Group::THREADS, token: "monitor-wait-single-park", on_key: Some("CRATONVM_MONITOR_WAIT_SINGLE_PARK"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::THREADS, token: "monitor-wait-enrol-interrupt-check", on_key: Some("CRATONVM_MONITOR_WAIT_ENROL_INTERRUPT_CHECK"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::THREADS, token: "monitor-interrupt-wakes-target", on_key: Some("CRATONVM_MONITOR_INTERRUPT_WAKES_TARGET"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::THREADS, token: "monitor-wait-poll-backoff", on_key: Some("CRATONVM_MONITOR_WAIT_POLL_BACKOFF"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::THREADS, token: "monitor-notify-one-waiter", on_key: Some("CRATONVM_MONITOR_NOTIFY_ONE_WAITER"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
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
    E { group: Group::COMPAT, token: "mh-find-special-jdk", on_key: Some("CRATONVM_MH_FIND_SPECIAL_JDK"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "mh-invoke-declared-cast", on_key: Some("CRATONVM_MH_INVOKE_DECLARED_CAST"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "mh-invokeexact-inner-cast", on_key: Some("CRATONVM_MH_INVOKEEXACT_INNER_CAST"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "mh-special-caller-cast", on_key: Some("CRATONVM_MH_SPECIAL_CALLER_CAST"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "mh-return-cast", on_key: Some("CRATONVM_MH_RETURN_CAST"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "crc32c-range-check", on_key: Some("CRATONVM_CRC32C_RANGE_CHECK"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "jni-findclass-loader", on_key: Some("CRATONVM_JNI_FINDCLASS_LOADER"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "jni-findclass-raises", on_key: Some("CRATONVM_JNI_FINDCLASS_RAISES"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "throwable-fill-override-decides", on_key: Some("CRATONVM_THROWABLE_FILL_OVERRIDE_DECIDES"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "throwable-ctor-honours-fill-override", on_key: Some("CRATONVM_THROWABLE_CTOR_HONOURS_FILL_OVERRIDE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "mh-cast-chain", on_key: Some("CRATONVM_MH_CAST_CHAIN"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "mh-explicit-interface-uncast", on_key: Some("CRATONVM_MH_EXPLICIT_INTERFACE_UNCAST"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "proxy-types-through-loader", on_key: Some("CRATONVM_PROXY_TYPES_THROUGH_LOADER"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "native-invoke-strict-selection", on_key: Some("CRATONVM_NATIVE_INVOKE_STRICT_SELECTION"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "throwable-ctor-fill-override-exact", on_key: Some("CRATONVM_THROWABLE_CTOR_FILL_OVERRIDE_EXACT"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "jni-findclass-initializes", on_key: Some("CRATONVM_JNI_FINDCLASS_INITIALIZES"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "awt-events-real-bytecode", on_key: Some("CRATONVM_AWT_EVENTS_REAL_BYTECODE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "reflective-dispatch-selects", on_key: Some("CRATONVM_REFLECTIVE_DISPATCH_SELECTS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "throwable-fill-override-bridges", on_key: Some("CRATONVM_THROWABLE_FILL_OVERRIDE_BRIDGES"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "mh-cast-chain-converts", on_key: Some("CRATONVM_MH_CAST_CHAIN_CONVERTS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "mh-invoke-declared-cast-skips-typed", on_key: Some("CRATONVM_MH_INVOKE_DECLARED_CAST_SKIPS_TYPED"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-nonpublic-in-iface-loader", on_key: Some("CRATONVM_PROXY_NONPUBLIC_IN_IFACE_LOADER"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-map-jdk-order", on_key: Some("CRATONVM_COMPAT_MAP_JDK_ORDER"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-chm-value-equals", on_key: Some("CRATONVM_COMPAT_CHM_VALUE_EQUALS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-boolean-element-hash", on_key: Some("CRATONVM_COMPAT_BOOLEAN_ELEMENT_HASH"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-boolean-key-hash", on_key: Some("CRATONVM_COMPAT_BOOLEAN_KEY_HASH"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-map-load-factor", on_key: Some("CRATONVM_COMPAT_MAP_LOAD_FACTOR"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "bigint-str-helpers-limb", on_key: Some("CRATONVM_BIGINT_STR_HELPERS_LIMB"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "bigint-unicode-digits", on_key: Some("CRATONVM_BIGINT_UNICODE_DIGITS"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "bigint-p71-limb-road", on_key: Some("CRATONVM_BIGINT_P71_LIMB_ROAD"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "bigint-mag-to-decimal-chunked", on_key: Some("CRATONVM_BIGINT_MAG_TO_DECIMAL_CHUNKED"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "liquibase-date-gettime", on_key: Some("CRATONVM_LIQUIBASE_DATE_GETTIME"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "bigint-from-decimal-fast", on_key: Some("CRATONVM_BIGINT_FROM_DECIMAL_FAST"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "native-create-string-hit-or-fresh", on_key: Some("CRATONVM_NATIVE_CREATE_STRING_HIT_OR_FRESH"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "exc-message-propagate", on_key: Some("CRATONVM_EXC_MESSAGE_PROPAGATE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "compat-hashset-build-treeify-levels", on_key: Some("CRATONVM_COMPAT_HASHSET_BUILD_TREEIFY_LEVELS"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "compat-lhm-clone-fresh-capacity", on_key: Some("CRATONVM_COMPAT_LHM_CLONE_FRESH_CAPACITY"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "compat-lhm-load-factor", on_key: Some("CRATONVM_COMPAT_LHM_LOAD_FACTOR"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "exc-tostring-localized", on_key: Some("CRATONVM_EXC_TOSTRING_LOCALIZED"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "compat-map-long-chains", on_key: Some("CRATONVM_COMPAT_MAP_LONG_CHAINS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-chm-cia-single-walk", on_key: Some("CRATONVM_COMPAT_CHM_CIA_SINGLE_WALK"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-set-of-start-table", on_key: Some("CRATONVM_COMPAT_SET_OF_START_TABLE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-hashset-build-treeify", on_key: Some("CRATONVM_COMPAT_HASHSET_BUILD_TREEIFY"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-lhm-clear-in-place", on_key: Some("CRATONVM_COMPAT_LHM_CLEAR_IN_PLACE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-lhm-remap-grow-on-entry", on_key: Some("CRATONVM_COMPAT_LHM_REMAP_GROW_ON_ENTRY"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-lhm-remap-evict-last", on_key: Some("CRATONVM_COMPAT_LHM_REMAP_EVICT_LAST"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-lhm-tree-put-if-absent", on_key: Some("CRATONVM_COMPAT_LHM_TREE_PUT_IF_ABSENT"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-lhm-ctor-capacity", on_key: Some("CRATONVM_COMPAT_LHM_CTOR_CAPACITY"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-chm-remap-single-walk", on_key: Some("CRATONVM_COMPAT_CHM_REMAP_SINGLE_WALK"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-overlay-materialize-cap", on_key: Some("CRATONVM_COMPAT_OVERLAY_MATERIALIZE_CAP"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-values-remove-jdk", on_key: Some("CRATONVM_COMPAT_VALUES_REMOVE_JDK"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-lhm-grow-on-insert", on_key: Some("CRATONVM_COMPAT_LHM_GROW_ON_INSERT"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-lhm-tree-bins", on_key: Some("CRATONVM_COMPAT_LHM_TREE_BINS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-hashmap-tree-bins", on_key: Some("CRATONVM_COMPAT_HASHMAP_TREE_BINS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-map-single-walk", on_key: Some("CRATONVM_COMPAT_MAP_SINGLE_WALK"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-hashset-build-jdk", on_key: Some("CRATONVM_COMPAT_HASHSET_BUILD_JDK"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-lhm-single-hash", on_key: Some("CRATONVM_COMPAT_LHM_SINGLE_HASH"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-lhm-bucket-order", on_key: Some("CRATONVM_COMPAT_LHM_BUCKET_ORDER"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-chm-single-hash", on_key: Some("CRATONVM_COMPAT_CHM_SINGLE_HASH"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-chm-table-high-water", on_key: Some("CRATONVM_COMPAT_CHM_TABLE_HIGH_WATER"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-lhs-node-ops", on_key: Some("CRATONVM_COMPAT_LHS_NODE_OPS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-set-bulk-jdk", on_key: Some("CRATONVM_COMPAT_SET_BULK_JDK"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-map-remap-cme", on_key: Some("CRATONVM_COMPAT_MAP_REMAP_CME"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-map-compute-jdk", on_key: Some("CRATONVM_COMPAT_MAP_COMPUTE_JDK"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-map-foreach-live", on_key: Some("CRATONVM_COMPAT_MAP_FOREACH_LIVE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-itr-remove-by-node", on_key: Some("CRATONVM_COMPAT_ITR_REMOVE_BY_NODE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-lhm-reversed-by-node", on_key: Some("CRATONVM_COMPAT_LHM_REVERSED_BY_NODE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-entry-set-value-in-place", on_key: Some("CRATONVM_COMPAT_ENTRY_SET_VALUE_IN_PLACE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-map-equals-jdk", on_key: Some("CRATONVM_COMPAT_MAP_EQUALS_JDK"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-hashset-ctor-jdk", on_key: Some("CRATONVM_COMPAT_HASHSET_CTOR_JDK"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-view-build-by-node", on_key: Some("CRATONVM_COMPAT_VIEW_BUILD_BY_NODE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-view-remove-by-node", on_key: Some("CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-entry-set-nodes", on_key: Some("CRATONVM_COMPAT_ENTRY_SET_NODES"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-overlay-remap-order", on_key: Some("CRATONVM_COMPAT_OVERLAY_REMAP_ORDER"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-lhm-reversed-sees-overwrites", on_key: Some("CRATONVM_COMPAT_LHM_REVERSED_SEES_OVERWRITES"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-chm-remap-single-hash", on_key: Some("CRATONVM_COMPAT_CHM_REMAP_SINGLE_HASH"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "compat-treemap-value-equals-order", on_key: Some("CRATONVM_COMPAT_TREEMAP_VALUE_EQUALS_ORDER"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "thread-mirror-daemon", on_key: Some("CRATONVM_THREAD_MIRROR_DAEMON"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "thread-inherits-daemon", on_key: Some("CRATONVM_THREAD_INHERITS_DAEMON"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "mh-cast-chain-cross-variant", on_key: Some("CRATONVM_MH_CAST_CHAIN_CROSS_VARIANT"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "mh-invoke-boxes-declared-return", on_key: Some("CRATONVM_MH_INVOKE_BOXES_DECLARED_RETURN"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-one-dispatch-model", on_key: Some("CRATONVM_PROXY_ONE_DISPATCH_MODEL"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-annotation-body", on_key: Some("CRATONVM_PROXY_ANNOTATION_BODY"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "annotation-jdk-semantics", on_key: Some("CRATONVM_ANNOTATION_JDK_SEMANTICS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "annotation-foreign-equals", on_key: Some("CRATONVM_ANNOTATION_FOREIGN_EQUALS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "annotation-member-conformance", on_key: Some("CRATONVM_ANNOTATION_MEMBER_CONFORMANCE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-invoke-default-checks", on_key: Some("CRATONVM_PROXY_INVOKE_DEFAULT_CHECKS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-varargs-flag", on_key: Some("CRATONVM_PROXY_VARARGS_FLAG"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-dynamic-module", on_key: Some("CRATONVM_PROXY_DYNAMIC_MODULE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-nonexported-package", on_key: Some("CRATONVM_PROXY_NONEXPORTED_PACKAGE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-module-descriptor", on_key: Some("CRATONVM_PROXY_MODULE_DESCRIPTOR"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "annotation-carrier-is-handler", on_key: Some("CRATONVM_ANNOTATION_CARRIER_IS_HANDLER"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "annotation-reflective-alias-copy", on_key: Some("CRATONVM_ANNOTATION_REFLECTIVE_ALIAS_COPY"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-final-object-methods", on_key: Some("CRATONVM_PROXY_FINAL_OBJECT_METHODS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-body-handler-virtual", on_key: Some("CRATONVM_PROXY_BODY_HANDLER_VIRTUAL"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-body-null-primitive-npe", on_key: Some("CRATONVM_PROXY_BODY_NULL_PRIMITIVE_NPE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-jdk-method-order", on_key: Some("CRATONVM_PROXY_JDK_METHOD_ORDER"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-nonpublic-refusals", on_key: Some("CRATONVM_PROXY_NONPUBLIC_REFUSALS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-module-access-refusal", on_key: Some("CRATONVM_PROXY_MODULE_ACCESS_REFUSAL"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-nonpublic-class-flags", on_key: Some("CRATONVM_PROXY_NONPUBLIC_CLASS_FLAGS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-jdk-class-flags", on_key: Some("CRATONVM_PROXY_JDK_CLASS_FLAGS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-merged-throws", on_key: Some("CRATONVM_PROXY_MERGED_THROWS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-init-at-creation", on_key: Some("CRATONVM_PROXY_INIT_AT_CREATION"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-vtable-fast-path", on_key: Some("CRATONVM_PROXY_VTABLE_FAST_PATH"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "proxy-user-subclass-ordinary", on_key: Some("CRATONVM_PROXY_USER_SUBCLASS_ORDINARY"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "override-shadow-per-signature", on_key: Some("CRATONVM_OVERRIDE_SHADOW_PER_SIGNATURE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "jni-findclass-native-holder", on_key: Some("CRATONVM_JNI_FINDCLASS_NATIVE_HOLDER"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "mh-combinator-catches-native-throws", on_key: Some("CRATONVM_MH_COMBINATOR_CATCHES_NATIVE_THROWS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "throwable-cause-ctor-fill-first", on_key: Some("CRATONVM_THROWABLE_CAUSE_CTOR_FILL_FIRST"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "throwable-subclass-fields-after-fill", on_key: Some("CRATONVM_THROWABLE_SUBCLASS_FIELDS_AFTER_FILL"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "override-package-by-loader", on_key: Some("CRATONVM_OVERRIDE_PACKAGE_BY_LOADER"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "jni-defineclass-loader", on_key: Some("CRATONVM_JNI_DEFINECLASS_LOADER"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "jni-upcall-typed-errors", on_key: Some("CRATONVM_JNI_UPCALL_TYPED_ERRORS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::COMPAT, token: "mockito-legacy-selectors", on_key: Some("CRATONVM_MOCKITO_LEGACY_SELECTORS"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::COMPAT, token: "stackwalker-jdk-walk", on_key: Some("CRATONVM_SW_JDK_WALK"), off_key: None, off_word: None, since: "2026-08-24" },
    E { group: Group::COMPAT, token: "jdk-random", on_key: Some("CRATONVM_JDK_RANDOM"), off_key: None, off_word: None, since: "2026-08-30" },
    E { group: Group::COMPAT, token: "random-subclass-yield", on_key: Some("CRATONVM_RANDOM_SUBCLASS_YIELD"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "random-serial-state", on_key: Some("CRATONVM_RANDOM_SERIAL_STATE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "random-layout-memo", on_key: Some("CRATONVM_RANDOM_LAYOUT_MEMO"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "random-real-seed-field", on_key: Some("CRATONVM_RANDOM_REAL_SEED_FIELD"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "fabricated-provider-map", on_key: Some("CRATONVM_FABRICATED_PROVIDER_MAP"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "xbootclasspath-append", on_key: Some("CRATONVM_XBOOTCLASSPATH_APPEND"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "boot-append-dir-census", on_key: Some("CRATONVM_BOOT_APPEND_DIR_CENSUS"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "agent-boot-class-path-bootstrap", on_key: Some("CRATONVM_AGENT_BOOT_CLASS_PATH_BOOTSTRAP"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "module-mirror-layer-slot-guard", on_key: Some("CRATONVM_MODULE_MIRROR_LAYER_SLOT_GUARD"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "throwable-native-trace-field", on_key: Some("CRATONVM_THROWABLE_NATIVE_TRACE_FIELD"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "throwable-print-cause-loop", on_key: Some("CRATONVM_THROWABLE_PRINT_CAUSE_LOOP"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "stack-trace-module-prefix", on_key: Some("CRATONVM_STACK_TRACE_MODULE_PREFIX"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "stack-trace-element-exact-class", on_key: Some("CRATONVM_STACK_TRACE_ELEMENT_EXACT_CLASS"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "stack-trace-element-full-origin", on_key: Some("CRATONVM_STACK_TRACE_ELEMENT_FULL_ORIGIN"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "throwable-native-standin-frames", on_key: Some("CRATONVM_THROWABLE_NATIVE_STANDIN_FRAMES"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "throwable-receiver-npe-screen", on_key: Some("CRATONVM_THROWABLE_RECEIVER_NPE_SCREEN"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "throwable-standin-virtual-wait-frames", on_key: Some("CRATONVM_THROWABLE_STANDIN_VIRTUAL_WAIT_FRAMES"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "throwable-native-leaf-frames", on_key: Some("CRATONVM_THROWABLE_NATIVE_LEAF_FRAMES"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "throwable-standin-arg-check-frames", on_key: Some("CRATONVM_THROWABLE_STANDIN_ARG_CHECK_FRAMES"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "dump-threads-unformatted-elements", on_key: Some("CRATONVM_DUMP_THREADS_UNFORMATTED_ELEMENTS"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "thread-stack-callee-lines", on_key: Some("CRATONVM_THREAD_STACK_CALLEE_LINES"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "compat-join-zero-interruptible", on_key: Some("CRATONVM_COMPAT_JOIN_ZERO_INTERRUPTIBLE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "throwable-thread-run-lambda-screen", on_key: Some("CRATONVM_THROWABLE_THREAD_RUN_LAMBDA_SCREEN"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "thread-stack-parked-leaf-frames", on_key: Some("CRATONVM_THREAD_STACK_PARKED_LEAF_FRAMES"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "throwable-standin-owner-resolution", on_key: Some("CRATONVM_THROWABLE_STANDIN_OWNER_RESOLUTION"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "throwable-standin-join-frames", on_key: Some("CRATONVM_THROWABLE_STANDIN_JOIN_FRAMES"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "throwable-standin-virtual-join-decline", on_key: Some("CRATONVM_THROWABLE_STANDIN_VIRTUAL_JOIN_DECLINE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "throwable-standin-arg-check-site-hint", on_key: Some("CRATONVM_THROWABLE_STANDIN_ARG_CHECK_SITE_HINT"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "throwable-standin-timed-join-frames", on_key: Some("CRATONVM_THROWABLE_STANDIN_TIMED_JOIN_FRAMES"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "throwable-standin-array-frames", on_key: Some("CRATONVM_THROWABLE_STANDIN_ARRAY_FRAMES"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "stack-walk-thread-run-frames", on_key: Some("CRATONVM_STACK_WALK_THREAD_RUN_FRAMES"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "throwable-thread-run-middle-frame", on_key: Some("CRATONVM_THROWABLE_THREAD_RUN_MIDDLE_FRAME"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "stack-trace-element-origin-memo", on_key: Some("CRATONVM_STACK_TRACE_ELEMENT_ORIGIN_MEMO"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "throwable-thread-run-frame", on_key: Some("CRATONVM_THROWABLE_THREAD_RUN_FRAME"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "throwable-thread-run-memo", on_key: Some("CRATONVM_THROWABLE_THREAD_RUN_MEMO"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "beans-real-change-support", on_key: Some("CRATONVM_BEANS_REAL_CHANGE_SUPPORT"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
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
    E { group: Group::COMPAT, token: "vh-indirect-refuse", on_key: Some("CRATONVM_VH_INDIRECT_REFUSE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "vh-indirect-serve", on_key: Some("CRATONVM_VH_INDIRECT_SERVE"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "vh-unsupported-mode-uoe", on_key: Some("CRATONVM_VH_UNSUPPORTED_MODE_UOE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::COMPAT, token: "vh-read-only-handle-uoe", on_key: Some("CRATONVM_VH_READ_ONLY_HANDLE_UOE"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::COMPAT, token: "vh-arity-wmte", on_key: Some("CRATONVM_VH_ARITY_WMTE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
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
/// filter: it drops synthetic-stub natives and nothing else. JDK-only mode is
/// the same rule plus the two halves the environment token structurally cannot
/// reach — class fabrication and the native-vs-bytecode dispatch decision — and
/// it records structured provenance for each refusal instead of dropping
/// silently. Contract §9 asks for the note; the token itself is untouched.
///
/// Since 2026-09-20 that mode is the launcher default, so the note the token
/// earns is no longer "pass a flag" but "you already have it unless you opted
/// out". The launcher only prints it on a run that resolved to `Compatible`
/// (i.e. one that passed `--compatible` / `--real-jdk` / `--synthetic-jdk`)
/// and set the token anyway — which is the only configuration where the
/// advice still changes anything.
pub const SUPERSEDED: &[Superseded] = &[Superseded {
    group: Group::REAL,
    token: "stubs",
    on: false,
    prefer: "the default jdk-only policy (i.e. dropping --compatible)",
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

/// The eight flags of contract §9, in the order §9 lists them.
///
/// Help text is copied verbatim from the contract: it is the user-visible
/// surface the design pinned, and re-wording it here would silently fork the
/// documentation from the binary.
pub const JDK_ONLY_CLI_FLAGS: &[CliFlag] = &[
    CliFlag {
        flag: "--jdk-only",
        takes_value: false,
        help: "Real JDK, reject compatibility stubs and fabricated classes (default).",
    },
    CliFlag {
        flag: "--compatible",
        takes_value: false,
        help: "Real JDK with the pre-2026-09-20 compatibility behaviour.",
    },
    CliFlag {
        flag: "--real-jdk",
        takes_value: false,
        help: "Deprecated alias for --compatible.",
    },
    CliFlag {
        flag: "--synthetic-jdk",
        takes_value: false,
        help: "Standalone synthetic library; implies --compatible, conflicts with --jdk-only.",
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
/// `--synthetic-jdk` selects a class library that is nothing but
/// substitutions, which is the one combination that cannot mean anything
/// (contract §1, §6).
///
/// `--compatible` and its deprecated alias `--real-jdk` joined this list on
/// 2026-09-20. They did not belong here before: `--real-jdk` then meant only
/// "use the real image", which `--jdk-only` already implied, so the pair
/// agreed. It now names the opposite *policy*, and a pair that states both
/// policies has no reading — so the launcher refuses instead of letting
/// argument order decide.
pub const JDK_ONLY_CONFLICTS_WITH: &[&str] = &["--synthetic-jdk", "--compatible", "--real-jdk"];

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

// ---------------------------------------------------------------------------
// `CRATONVM_*` names that no knob claims (gen r4w3/obs2, 2026-09-23)
// ---------------------------------------------------------------------------
//
// A misspelt GROUPED token fails closed (`unknown_tokens`, fatal in `vm-cli`).
// A misspelt PER-FLAG variable used to fail open, silently: `resolve` only ever
// asks about names some `INVENTORY` row declares, so `CRATONVM_ZGC_GEN_HEADER_ZERO=0`
// (the real knob is `CRATONVM_ZGC_SWEEP_HEADER_ZERO`) configured nothing and said
// nothing, and the run measured the default while reading as the A/B arm
// (`docs/internal/gc/gengc-plumbing2-legacy-flag-typos-fail-silently-FIXED-20260923.md`).
//
// The namespace is open — harness scripts and embedders export their own
// `CRATONVM_*` variables around a VM run — so this can only ever be ADVICE:
// one stderr line from the launcher, never a refusal. What keeps it free of
// false positives is that "claimed" is decided from lists, not guessed:
//
// * every name a knob reads (an `INVENTORY` key, a `SCALARS` entry, a group
//   variable — exactly `types/tests/flag-surface.txt`);
// * [`UNDECLARED_READS`] — the names `types/tests/flag_declaration_guard.rs`
//   exempts from declaration (read raw by the flag machinery itself, by test
//   binaries, by build scripts). That test locks the two lists together;
// * [`HARNESS_VARS`] and [`HARNESS_PREFIXES`] — what this repository's own
//   scripts set around a VM run.
//
// Both lists are spelled WITHOUT the `CRATONVM_` prefix on purpose: the
// declaration guard treats every whole `"CRATONVM_…"` literal under a `src/` as
// a read site, so full spellings here would make every one of its exemptions
// look live forever and blind its dead-row check.

/// The names `types/tests/flag_declaration_guard.rs::ALLOWED` exempts from
/// declaration, without their `CRATONVM_` prefix. Read raw by the flag
/// machinery itself, by test binaries and by build scripts, so they can be in
/// an environment on purpose although no knob claims them.
/// `the_runtime_unclaimed_list_is_the_guard_allowlist` in that test keeps the
/// two equal: a row added there must be added here.
pub const UNDECLARED_READS: &[&str] = &[
    "ALLOW_UNKNOWN_TOKENS",
    "COMPATIBILITY_JDK_ONLY",
    "DBG_FLAGREADS",
    "DIFF_HOTSPOT",
    "FUZZ_BOOTCP",
    "JIT_BASELINE_FAST",
    "JIT_BASELINE_NO_SPEC",
    "NONEXISTENT_VAR_12345",
    "NO_MISPLACED_FLAG_WARNING",
    "RATCHET_ROWS",
    "REAL_RAF",
    "REGEN_DEAD_CITATION_BASELINE",
    "REGEN_HEADER",
    "REQUIRE_E2E",
    "RUN_EXTENDED_INTERPRETER_TESTS",
    "SOMETHING_BRAND_NEW",
    "SPRING_BOOT_FATJAR",
    "TEST_CLASSES_DIR",
    "TEST_JAVA_HOME",
    "TEST_NPE_OPTIMIZING_BODY_CHILD",
    "TEST_PIPE_FLOOD",
    "ZGC_MARKEND_LOG",
];

/// Variables this repository's own scripts export around a VM run and the VM
/// never reads, without their `CRATONVM_` prefix: `regression-suite/run.sh`'s
/// and CI's `ARGS`, `scripts/cratonvm-prefix-args.sh`'s `PREFIX_BIN` /
/// `PREFIX_ARGS`, the Tomcat harness's `EXE`, `tools/compact-tlab-soak.sh`'s
/// deliberately inert `AB_NOOP`, and the regression reporters' `BUGS` /
/// `CRASHES` / `KEEP_SCRIPT`.
pub const HARNESS_VARS: &[&str] = &[
    "AB_NOOP",
    "ARGS",
    "BUGS",
    "CRASHES",
    "EXE",
    "KEEP_SCRIPT",
    "PREFIX_ARGS",
    "PREFIX_BIN",
];

/// Name prefixes (after `CRATONVM_`) owned by test, benchmark and soak
/// harnesses.
pub const HARNESS_PREFIXES: &[&str] = &["TEST_", "BENCH_", "SOAK_"];

/// Every name a knob reads: `INVENTORY` keys, `SCALARS`, group variables.
fn claimed_by_a_knob(name: &str) -> bool {
    SCALARS.contains(&name)
        || Group::ALL.iter().any(|g| g.var() == name)
        || INVENTORY
            .iter()
            .any(|e| e.on_key == Some(name) || e.off_key == Some(name))
}

/// Is the environment name `name` claimed — read by a knob, or present by
/// design? Names outside the `CRATONVM_` namespace are always claimed.
pub fn env_name_is_claimed(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("CRATONVM_") else {
        return true;
    };
    claimed_by_a_knob(name)
        || UNDECLARED_READS.contains(&rest)
        || HARNESS_VARS.contains(&rest)
        || HARNESS_PREFIXES.iter().any(|p| rest.starts_with(p))
}

/// The `CRATONVM_*` names among `names` that nothing claims — each one set,
/// and IGNORED. Sorted, de-duplicated. Pure: the launcher passes the process
/// environment's names.
pub fn unclaimed_env_names<'a, I: IntoIterator<Item = &'a str>>(names: I) -> Vec<String> {
    let mut out: Vec<String> = names
        .into_iter()
        .filter(|n| n.starts_with("CRATONVM_") && !env_name_is_claimed(n))
        .map(str::to_string)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Levenshtein distance, for [`closest_knob_name`].
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = Vec::with_capacity(b.len() + 1);
        cur.push(i + 1);
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != *cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The knob-read name closest to `name`, when one is close enough to be a
/// plausible misspelling (at most a fifth of the name's length, and at least
/// 2, edits away) and no other name is equally close. `None` rather than a
/// coin toss: a wrong hint invites a second inert run (the same rule
/// [`suggest`] follows for tokens).
pub fn closest_knob_name(name: &str) -> Option<&'static str> {
    let limit = (name.len() / 5).max(2);
    let mut best: Option<(usize, &'static str)> = None;
    let mut tied = false;
    let candidates = INVENTORY
        .iter()
        .flat_map(|e| [e.on_key, e.off_key].into_iter().flatten())
        .chain(SCALARS.iter().copied())
        .chain(Group::ALL.iter().map(|g| g.var()));
    for cand in candidates {
        let d = edit_distance(name, cand);
        if d == 0 || d > limit {
            continue;
        }
        match best {
            Some((bd, bn)) if d == bd && bn != cand => tied = true,
            Some((bd, _)) if d >= bd => {}
            _ => {
                best = Some((d, cand));
                tied = false;
            }
        }
    }
    if tied {
        None
    } else {
        best.map(|(_, n)| n)
    }
}

/// The one advisory line for `unclaimed` names, or `None` when there are none.
/// Names each variable and, where one is close, the knob it probably meant.
pub fn unclaimed_env_advisory(unclaimed: &[String]) -> Option<String> {
    if unclaimed.is_empty() {
        return None;
    }
    let shown: Vec<String> = unclaimed
        .iter()
        .map(|n| match closest_knob_name(n) {
            Some(k) => format!("{n} (did you mean {k}?)"),
            None => n.clone(),
        })
        .collect();
    Some(format!(
        "[cratonvm] {} CRATONVM_* variable(s) set but claimed by no knob, so IGNORED \
         (this run is NOT configured by them): {}. Silence with \
         CRATONVM_DBG=-deprecations.",
        unclaimed.len(),
        shown.join(", ")
    ))
}

#[cfg(test)]
mod unclaimed_env_tests {
    use super::*;

    /// `CRATONVM_<suffix>`. The made-up names below are built, not spelled:
    /// `types/tests/flag_declaration_guard.rs` reads every whole
    /// `"CRATONVM_..."` literal under a `src/` as a read site.
    fn var(suffix: &str) -> String {
        format!("CRATONVM_{suffix}")
    }

    #[test]
    fn the_documented_typo_is_reported_with_the_real_name() {
        // `docs/gc-tuning.md`'s instance: the knob is `..._SWEEP_HEADER_ZERO`.
        let typo = var("ZGC_GEN_HEADER_ZERO");
        let got = unclaimed_env_names([typo.as_str(), "PATH"]);
        assert_eq!(got, [typo.clone()]);
        let line = unclaimed_env_advisory(&got).unwrap();
        let expected = format!("{typo} (did you mean {}?)", var("ZGC_SWEEP_HEADER_ZERO"));
        assert!(line.contains(&expected), "{line}");
    }

    #[test]
    fn claimed_names_are_never_reported() {
        let knob = INVENTORY
            .iter()
            .find_map(|e| e.on_key)
            .expect("the inventory declares keys");
        let names: Vec<String> = vec![
            knob.to_string(),
            SCALARS[0].to_string(),
            Group::GC.var().to_string(),
            var(UNDECLARED_READS[0]),
            var(HARNESS_VARS[0]),
            var(&format!("{}ANYTHING", HARNESS_PREFIXES[0])),
            "JAVA_HOME".to_string(),
        ];
        assert!(unclaimed_env_names(names.iter().map(String::as_str)).is_empty());
        assert_eq!(unclaimed_env_advisory(&[]), None);
    }

    #[test]
    fn a_name_far_from_every_knob_gets_no_guess() {
        let far = var("QQQQQQQQQQQQQQQQQQQQQQQQQQQ");
        assert_eq!(closest_knob_name(&far), None);
        let unknown = var("NOT_A_REAL_KNOB");
        let got = unclaimed_env_names([unknown.as_str(), unknown.as_str()]);
        assert_eq!(got, [unknown], "one entry per name");
    }

    #[test]
    fn edit_distance_is_levenshtein() {
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("same", "same"), 0);
    }
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

    /// Each of these knobs is default-ON: its reader treats an UNSET key as on and
    /// `0` as off. Without an `off_word` the minus token unset the key and the
    /// feature stayed on (2026-09-12), so each must write `0`.
    #[test]
    fn default_on_knobs_minus_token_writes_the_off_word() {
        for (group, token, key) in [
            ("CRATONVM_JIT", "deopt-real", "CRATONVM_DEOPT_REAL"),
            ("CRATONVM_JIT", "bg-compile", "CRATONVM_BG_COMPILE"),
            (
                "CRATONVM_GC",
                "compact-ref-fields",
                "CRATONVM_COMPACT_REF_FIELDS",
            ),
            (
                "CRATONVM_GC",
                "layout-scan-cache",
                "CRATONVM_GC_LAYOUT_SCAN_CACHE",
            ),
            (
                "CRATONVM_GC",
                "pack-fields-by-width",
                "CRATONVM_PACK_FIELDS_BY_WIDTH",
            ),
            ("CRATONVM_JIT", "osr-newarray", "CRATONVM_OSR_NEWARRAY"),
            ("CRATONVM_JIT", "rootsnap-cache", "CRATONVM_ROOTSNAP_CACHE"),
            (
                "CRATONVM_JIT",
                "rootsnap-cache-survive-gc",
                "CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC",
            ),
            (
                "CRATONVM_JIT",
                "direct-callee-calls",
                "CRATONVM_JIT_DIRECT_CALLEE_CALLS",
            ),
            (
                "CRATONVM_JIT",
                "dispatch-cache-direct-entry",
                "CRATONVM_JIT_DISPATCH_CACHE_DIRECT_ENTRY",
            ),
            (
                "CRATONVM_JIT",
                "dispatch-cache-virtual-direct-entry",
                "CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY",
            ),
            ("CRATONVM_JIT", "final-devirt", "CRATONVM_JIT_FINAL_DEVIRT"),
            (
                "CRATONVM_JIT",
                "final-devirt-native-screen",
                "CRATONVM_JIT_FINAL_DEVIRT_NATIVE_SCREEN",
            ),
            ("CRATONVM_JIT", "inline-calls", "CRATONVM_JIT_INLINE_CALLS"),
            ("CRATONVM_JIT", "inline-nest", "CRATONVM_JIT_INLINE_NEST"),
            (
                "CRATONVM_JIT",
                "inline-self-guard",
                "CRATONVM_JIT_INLINE_SELF_GUARD",
            ),
            (
                "CRATONVM_JIT",
                "inline-splice-devirt",
                "CRATONVM_JIT_INLINE_SPLICE_DEVIRT",
            ),
            ("CRATONVM_JIT", "ir-call", "CRATONVM_JIT_IR_CALL"),
            (
                "CRATONVM_JIT",
                "ir-call-special",
                "CRATONVM_JIT_IR_CALL_SPECIAL",
            ),
            (
                "CRATONVM_JIT",
                "ir-direct-call",
                "CRATONVM_JIT_IR_DIRECT_CALL",
            ),
            ("CRATONVM_JIT", "ir-fp", "CRATONVM_JIT_IR_FP"),
            ("CRATONVM_JIT", "ir-long", "CRATONVM_JIT_IR_LONG"),
            (
                "CRATONVM_JIT",
                "ir-selfrec-direct",
                "CRATONVM_JIT_IR_SELFREC_DIRECT",
            ),
            (
                "CRATONVM_JIT",
                "lambda-adapter",
                "CRATONVM_JIT_LAMBDA_ADAPTER",
            ),
            (
                "CRATONVM_JIT",
                "lambda-capture-adapter",
                "CRATONVM_JIT_LAMBDA_CAPTURE_ADAPTER",
            ),
            ("CRATONVM_JIT", "lambda-site", "CRATONVM_JIT_LAMBDA_SITE"),
            (
                "CRATONVM_JIT",
                "lambda-tierup",
                "CRATONVM_JIT_LAMBDA_TIERUP",
            ),
            ("CRATONVM_JIT", "licm", "CRATONVM_JIT_LICM"),
            (
                "CRATONVM_JIT",
                "local-handlers",
                "CRATONVM_JIT_LOCAL_HANDLERS",
            ),
            (
                "CRATONVM_JIT",
                "osr-exc-table",
                "CRATONVM_JIT_OSR_EXC_TABLE",
            ),
            (
                "CRATONVM_JIT",
                "safepoint-polls",
                "CRATONVM_JIT_SAFEPOINT_POLLS",
            ),
            ("CRATONVM_JIT", "scalar-new", "CRATONVM_JIT_SCALAR_NEW"),
            (
                "CRATONVM_JIT",
                "virtual-tierup",
                "CRATONVM_JIT_VIRTUAL_TIERUP",
            ),
        ] {
            let off = case(&[(group, &format!("-{token}"))]);
            assert_eq!(
                off.resolve().get(key),
                Some(OsString::from("0")),
                "{group}=-{token} must write {key}=0; its reader treats an unset key as on"
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
            (
                "CRATONVM_JIT",
                "xt-helper-window-pin-resolve",
                "CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE",
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
        // The preferred spelling stopped being a flag on 2026-09-20: the
        // policy the token approximates is the launcher default, so the advice
        // is to stop opting out rather than to opt in.
        assert_eq!(
            row.prefer,
            "the default jdk-only policy (i.e. dropping --compatible)"
        );
        assert_eq!(row.spelling(), "CRATONVM_REAL=-stubs");
        assert!(row.note().contains("CRATONVM_REAL=-stubs"));
        assert!(row.note().contains("jdk-only"));
        assert!(row.note().contains("--compatible"));
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
        assert_eq!(
            r.superseded[0].prefer,
            "the default jdk-only policy (i.e. dropping --compatible)"
        );
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
    fn the_jdk_only_cli_surface_is_the_eight_flags_of_the_contract() {
        let flags: Vec<&str> = JDK_ONLY_CLI_FLAGS.iter().map(|f| f.flag).collect();
        assert_eq!(
            flags,
            vec![
                "--jdk-only",
                "--compatible",
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
    fn jdk_only_conflicts_with_the_synthetic_library_and_the_compatible_policy() {
        assert_eq!(
            JDK_ONLY_CONFLICTS_WITH.to_vec(),
            vec!["--synthetic-jdk", "--compatible", "--real-jdk"]
        );
        for name in JDK_ONLY_CONFLICTS_WITH {
            assert!(
                jdk_only_flag(name).is_some(),
                "{name} is not a flag this table knows about"
            );
        }
        // `--real-jdk` joined this list on 2026-09-20. Before the default
        // flipped it named only the image, which `--jdk-only` already implied,
        // so the two agreed; it is now an alias for `--compatible` and names
        // the opposite policy.
        assert!(JDK_ONLY_CONFLICTS_WITH.contains(&"--real-jdk"));
        assert!(!JDK_ONLY_CONFLICTS_WITH.contains(&"--jdk-only"));
    }

    // ── The 2026-08-01 declaration audit ───────────────────────────────────
    //
    // Four JIT capabilities landed in this wave with their consumers committed
    // and their declarations impossible, because this file was owned by another
    // lane. The tests below pin what was verified at each consumer, not merely
    // that a row exists — a row with the wrong polarity is worse than no row,
    // since it reads as coverage while doing the opposite of what it says.

    /// The default-OFF JIT capability rows below are switched off by **removing**
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
    ///
    /// (`vectorize` left this list on 2026-09-18, round 9 wave 4, when it went
    /// default-ON with `off_word: Some("0")`.)
    #[test]
    fn default_off_capabilities_are_switched_off_by_unsetting_them() {
        // (token, the consumer tests presence only)
        let rows = [("range-bce", true), ("activation-global-mutex", true)];
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
    /// standalone `CompilationBroker` it would have gated was deleted
    /// 2026-09-12 without ever being wired (`docs/jit/compilation-broker.md`).
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
            // `zgc-parmark` joined this set on 2026-08-13 and left it again on
            // 2026-08-14 when `Z_PARMARK_DEFAULT_WORKERS` was reverted to `0`
            // -- it is an opt-in now (see the `INVENTORY` row's comment), not
            // a kill switch, so it is no longer pinned here.
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
                "{token} gates default-ON machinery; without a falsey word \
                 `CRATONVM_GC=-{token}` unsets the key and leaves it ON"
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
    fn the_whole_surface_is_twenty_variables() {
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
        //
        // 18 -> 19 on 2026-09-26: `CRATONVM_MONITOR_LAZY_PARK_US` (JIT round 11
        // wave 19), argued at its entry in `SCALARS`: it carries a VALUE (a
        // park bound in microseconds), which the presence-only `E` model
        // cannot express. That change did not update this count.
        //
        // 19 -> 20 on 2026-09-27: `CRATONVM_MONITOR_MAX_SPINNERS` (JIT round 12
        // wave 1), a cap on contended spinners per monitor: it carries a VALUE,
        // which the presence-only `E` model cannot express. That change did not
        // update this count either; gen round 5 wave 5 did.
        assert_eq!(Group::ALL.len() + SCALARS.len(), 20);
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

    // ── The 2026-09-21 declaration audit (round 9 wave 5) ──────────────────
    //
    // The three names `flag_declaration_guard` had been red on, each declared
    // after reading its consumer rather than after reading its NAME. The same
    // reasoning as the 2026-08-01 block above, and the same reason for pinning
    // it: a row with the wrong polarity is worse than no row, because it reads
    // as coverage while doing the opposite of what it says.

    /// All three consumers test the key's PRESENCE, so none may carry an
    /// `off_word`.
    ///
    /// This is the 23-row defect of this very round, restated for three more
    /// rows. `off_word: Some("0")` writes `"0"` into the key; `is_some()` and
    /// `is_none()` both still see a key that is SET, so the grouped spelling
    /// that means "off" would turn each of these knobs **on**. For
    /// `read-cursor` that is not cosmetic: `-read-cursor` is the spelling an
    /// operator reaches for to restore the per-accessor `is_object_address`
    /// probe after a suspected miss, and an `off_word` would hand them back
    /// the trusted cursor they were trying to rule out.
    ///
    /// The exact edit that trips it: give any of the three rows an `off_word`.
    #[test]
    fn the_2026_09_21_rows_are_all_presence_parsed() {
        for (group, token) in [
            (Group::GC, "read-cursor"),
            (Group::GC, "refproc-prepass"),
            (Group::DBG, "afc-pending-future"),
        ] {
            let e = lookup(group, token).unwrap_or_else(|| panic!("{token} is undeclared"));
            assert!(
                e.off_word.is_none(),
                "{token} carries off_word {:?}, but its consumer only tests the \
                 key's *presence* — that value would switch the knob ON",
                e.off_word
            );
            assert_eq!(e.since, "2026-09-21", "{token} states the wrong arrival");
        }
    }

    /// The two GC rows are DEFAULT-ON knobs with a dedicated `NO_` opt-out
    /// name, so `-token` **sets** that name and the bare token **clears** it.
    ///
    /// Clearing matters as much as setting: both consumers latch in a
    /// `OnceLock` on first read, so a stale `CRATONVM_NATIVE_NO_READ_CURSOR=1`
    /// inherited from a parent shell would otherwise stick for the whole run
    /// with no documented spelling able to undo it.
    #[test]
    fn the_two_default_on_gc_rows_are_opt_outs_named_by_their_key() {
        for (token, off_key) in [
            ("read-cursor", "CRATONVM_NATIVE_NO_READ_CURSOR"),
            ("refproc-prepass", "CRATONVM_GC_NO_REFPROC_PREPASS"),
        ] {
            let e = lookup(Group::GC, token).unwrap_or_else(|| panic!("{token} is undeclared"));
            assert_eq!(e.off_key, Some(off_key));
            assert!(
                e.on_key.is_none(),
                "{token} gained a positive key no consumer reads"
            );

            // Off: `-token` sets the `NO_` name, which is what the consumers'
            // `is_some()` / `is_none()` tests read.
            let spec = format!("-{token}");
            let c = case(&[("CRATONVM_GC", spec.as_str())]);
            assert_eq!(c.resolve().get(off_key), Some(OsString::from("1")));

            // On: the bare token clears a stale export from a parent shell.
            let c = case(&[("CRATONVM_GC", token), (off_key, "1")]);
            assert_eq!(
                c.resolve().get(off_key),
                None,
                "CRATONVM_GC={token} must clear {off_key}, not give it a value"
            );
        }
    }

    /// `afc-pending-future` is an OPT-IN debug print: `-token` unsets its key.
    ///
    /// Its read site is the one of the three that also had to move, from a raw
    /// `std::env::var_os` in `native-io/src/lib.rs` to `runtime_var_os`. A
    /// declared name read raw is served by a live `getenv` no matter what this
    /// table says, so the row would have been a claim about a lookup that was
    /// not happening.
    #[test]
    fn the_afc_debug_print_is_an_opt_in() {
        let e = lookup(Group::DBG, "afc-pending-future").expect("declared");
        assert_eq!(e.on_key, Some("CRATONVM_DBG_AFC_PENDING_FUTURE"));
        assert!(e.off_key.is_none(), "a debug print gained an opt-out name");

        let c = case(&[("CRATONVM_DBG", "afc-pending-future")]);
        assert_eq!(
            c.resolve().get("CRATONVM_DBG_AFC_PENDING_FUTURE"),
            Some(OsString::from("1"))
        );
        // Off is expressed by UNSETTING the key: `"0"` would leave
        // `is_some()` answering true and the print armed.
        let c = case(&[
            ("CRATONVM_DBG", "-afc-pending-future"),
            ("CRATONVM_DBG_AFC_PENDING_FUTURE", "1"),
        ]);
        assert_eq!(c.resolve().get("CRATONVM_DBG_AFC_PENDING_FUTURE"), None);
    }
}
