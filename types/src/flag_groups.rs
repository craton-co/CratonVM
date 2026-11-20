// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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

#![forbid(unsafe_code)]

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
    // FFM3W-2 (round 15 wave 2): MB of auto-arena bytes allocated without a collection before one is requested; unset/0 = off
    "CRATONVM_FFM_AUTO_ARENA_GC_BUDGET_MB",
    // Phase 5 boot image WP0 (2026-10-28): a file path; the launcher appends the
    // boot-image census (threads, new mappings, fds, .data/.bss diff) there.
    "CRATONVM_DBG_IMAGE_CENSUS",
    // Phase 5 boot image WP5 (2026-10-28): `off` (default) | `capture` | `auto`
    // (restore on a key hit, else cold boot and capture), plus `verbose` /
    // `census` tokens. A scalar because it carries a value; read by vm-cli's
    // `boot_image_launch` (Linux x86_64 only; elsewhere always off).
    "CRATONVM_BOOT_IMAGE",
    // Phase 5 boot image WP5: the image cache directory (a path); unset =
    // `$XDG_CACHE_HOME/cratonvm/boot-images` or `$HOME/.cache/...`.
    "CRATONVM_BOOT_IMAGE_DIR",
];

/// `(child, parent)`: an opt-in row whose reader, unset, follows `parent`,
/// so the effective default is the parent's, not necessarily `off`.
/// `render-inventory.py` prints it in the Default column; `flag_docs_generated`
/// checks the cell. The JIT readers are named in
/// `docs/feature-designs/jit-ir-optimizations.md`;
/// the ZGC profile is covered by `docs/feature-designs/jit-ir-optimizations.md`.
pub const FOLLOWS: &[(&str, &str)] = &[
    // ZGC's profile defaults (default ON, `CRATONVM_ZGC_CONCURRENT=0` kills the
    // whole profile). Load-barrier marking is an independent opt-in.
    // An explicit ZGC_CONC_START still overrides this profile fallback.
    // The concurrent-relocation stack (opt-in under the profile,
    // `CRATONVM_ZGC_CONCURRENT_RELOCATE=1`): each piece, unset, follows the
    // one switch.
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
    /// as false. `None` means "off is expressed by unsetting `on_key`",
    /// except for [`FOLLOWS`] children, which must write `0` to override a parent.
    ///
    /// A row with BOTH keys may carry one too when `on_key` has its own
    /// default-ON reader (JIT round 18 wave 11: `unroll`, whose `on_key` is the
    /// optimizing tier's unroller and whose `off_key` the single-pass one's):
    /// switching the token off then sets `off_key` AND writes this to
    /// `on_key`, instead of unsetting it.
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
    /// enforces the policy. `docs/feature-designs/real-jdk-mode.md`
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
    // B6: one line per compiled aastore SITE naming the gate verdict, so a
    // zero in the census is readable as "consulted and correctly declined"
    // rather than as an instrument armed where it cannot fire.
    // JIT round 18 wave 10 (lane ffmcost): diagnostic, the FFM direct rows'
    // cycle census (`panama::ffm_dbg_direct_split`).
    // Declared 2026-08-06: these nine were read by `runtime_var`/`runtime_var_os`
    // but named nowhere, so each was served by a live `getenv` instead of the
    // latched `VmFlags` snapshot -- `CRATONVM_DBG=token` could not reach them
    // and `flags::with_thread_overrides` could not arrange one in a test.
    // Interpreter round i1 wave 49 (lane L3): the optimizing tier's census of
    // the calls a splice emits without an inline-frame row.
    // Interpreter round i2 wave 66 (lane L1): the census of blocking regions
    // that open with no recorded wait kind, by the Rust call site of the door
    // (`ThreadRegistry::note_unrecorded_wait_region`), printed at exit; `trace`
    // also prints each site once, `assert` fails a debug build at the door.
    // Interpreter round i1 wave 50 (lane L1): the first-call guards of compiled
    // sites whose owner the compile door left unresolved.
    // The enforcement dial's per-door census. The TOTALS print on any armed
    // run without this; the token adds the per-triple rows.
    // Name the receiver, slot and compiled method for a field store the JIT
    // helper family discarded (implausible receiver, or slot past num_slots).
    // The counters are always on; this is the per-event dump.
    // JIT round 21 lane refs (`jr20-unreachable-weak-referent-survives-system-gc-in-some-runs-20261013.md`):
    // `=<class substring>` (`1` = every class) names what kept a weak/phantom referent alive
    // through a collection (`[referent-holder]` lines from the post-GC restore pass).
    // JIT round 21 lane stress (`docs/feature-designs/jit-ir-optimizations.md`): debug stress
    // modes, all default OFF and inert unless set. FUZZ21-1 `=<N>[:seed]`: every Nth arrival at
    // each optimizing back-edge poll takes the slow path, and a method-entry body leaves there for
    // the interpreter through its poll mode exit (uncharged) -- deopt-a-lot at precise states.
    E { group: Group::DBG, token: "jit-deopt-a-lot", on_key: Some("CRATONVM_DBG_JIT_DEOPT_A_LOT"), off_key: None, off_word: None, since: "2026-10-14" },
    // FUZZ21-7 `=<N>[:seed]`: the same forced arrival requests a collection at the poll.
    E { group: Group::DBG, token: "gc-at-back-edge", on_key: Some("CRATONVM_DBG_GC_AT_BACK_EDGE"), off_key: None, off_word: None, since: "2026-10-14" },
    // FUZZ21-3 `=1`: the optimizing tier's dead-home clears write a non-canonical poison word
    // instead of 0, so a premature clear faults at the reading instruction.
    // FUZZ21-2 `=1`: the IR verifier after every optimizer pass's cleanup, naming the pass site.
    // JIT round 21 lane reflect4 (REFLECT4-21-3, `jit-r21-reflect4-proposals.md`): `=1` counts every
    // compiled signature-polymorphic link (`invokeExact` / `invokeBasic` / `linkTo*`, i.e. each
    // `Method.invoke`) by outcome -- served, or the step that sent it to its door -- printed at exit.
    E { group: Group::DBG, token: "mh-link-census", on_key: Some("CRATONVM_DBG_MH_LINK_CENSUS"), off_key: None, off_word: None, since: "2026-10-14" },
    // JIT round 21 lane reflect4 (REFLECT4-21-1): `=1` counts the bytecode-only door's per-call
    // outcomes (compiled body entered, or why the body was interpreted), printed at exit.
    E { group: Group::DBG, token: "bytecode-only-body", on_key: Some("CRATONVM_DBG_BYTECODE_ONLY_BODY"), off_key: None, off_word: None, since: "2026-10-14" },
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
    // Per-phase timing for `annotation_proxy_dispatch_impl` (total / walk /
    // flagread / namecmp), printed every 100k dispatches. Reading one annotation
    // attribute costs ~10.5 us against ~10 ns on HotSpot; this splits it.
    // gci i6/mv: the audit control -- the oop-map frame rewrite gets an empty map (only with the audit armed).
    // gen r5w1/crash5: every raw root write-back (blocked-peer native-stack
    // word, shadow-stack indirect home, and the JIT frame remaps once wired)
    // logs where its target lies relative to the writing thread's own stack
    // (`cratonvm_gc::root_write_audit`). Presence-tested: no `off_word`.
    // The native-invocation census. Already read through `flags::runtime_var_os`
    // in `vm_exec.rs`; it was simply never declared, so `CRATONVM_DBG=census-    // exact-invocations` could not reach it.
    // Declared 2026-08-22 with the fix it was written for. When a VM-side field
    // read decodes a `Value` cell with an out-of-range discriminant, name the
    // RECEIVER and the Java frames that reached it. The collector's own guard
    // reports the CELL and can report nothing else -- it runs in the collector
    // crate and cannot see a frame -- which is why the producer behind
    // `corrupt-value-cell-is-fatal-on-three-of-four-collectors` stayed open for
    // two days. Costs one relaxed load per `NativeContext::get_field` while
    // armed and nothing at all while it is not.
    // Declared 2026-08-23 with the widening it verifies. Fabricates one
    // corrupt-cell hit at a site that HAS a read door and one at a site that
    // does not, so a run can prove the door reporter and the safepoint backstop
    // both actually speak. A silent diagnostic and a broken one look identical
    // from the outside, and this instrument was read the wrong way round once
    // already -- it reported nothing through a Spring Boot sweep that had
    // tripped the collector's guard, and the silence was taken for "clean".
    // The per-occurrence, backtrace-carrying arm of the descriptor-coercion
    // guard. The `gc::guard` WARN text tells operators to set this by name, so
    // an undeclared spelling of it was a live diagnostic that the grouped form
    // could not reach.
    E { group: Group::DBG, token: "deopt-eager", on_key: Some("CRATONVM_DEOPT_EAGER"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::DBG, token: "deopt-verify", on_key: Some("CRATONVM_DEOPT_VERIFY"), off_key: None, off_word: None, since: "2026-06-18" },
    // Default-ON: the launcher prints one line when it sees a legacy per-flag
    // variable set directly. `CRATONVM_DBG=-deprecations` silences it.
    E { group: Group::DBG, token: "diag-properties", on_key: Some("CRATONVM_DIAG_PROPERTIES"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::REAL, token: "props-unrooted-receivers", on_key: Some("CRATONVM_PROPS_UNROOTED_RECEIVERS"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::DBG, token: "diag-serviceloader", on_key: Some("CRATONVM_DIAG_SERVICELOADER"), off_key: None, off_word: None, since: "2026-05-20" },
    // Names every "already defined" defineClass backend error and the verdict
    // `classify_duplicate_define` gave it. Declared rather than left to a live
    // getenv so `CRATONVM_DBG=dupdef` reaches it and a test can arrange it:
    // both arms print, which is how a probe proves it exercised the one it claims.
    E { group: Group::DBG, token: "eintr-inject", on_key: Some("CRATONVM_DBG_EINTR_INJECT"), off_key: None, off_word: None, since: "2026-08-06" },
    E { group: Group::DBG, token: "eintr-no-retry", on_key: Some("CRATONVM_DBG_EINTR_NO_RETRY"), off_key: None, off_word: None, since: "2026-08-06" },
    // The socket transfer / selector engagement census
    // (`native-io::socket_fast_io::stats`). Diagnostic only: it prints counters
    // at exit and changes nothing a program can observe.
    E { group: Group::DBG, token: "force-moving", on_key: Some("CRATONVM_DBG_FORCE_MOVING"), off_key: None, off_word: None, since: "2026-06-03" },
    // gci i6/jd: one line per foreign-Path call the carrier natives used to answer wrongly (vm_exec redirect / nio_file yield).
    // gci i39/cv: diagnostic, default OFF, reads only.
    // gci i41/hc: diagnostic (default OFF, reads only), the root entry holding each kept-finalizable closure member.
    // G1 collection-overlay owner edges (`gc/src/g1_overlay.rs`): one
    // `[g1][overlay]` line per evacuation pause.
    // gci i22/g1rk: one `[g1-addr-keyed] mark-exact asked= dead=` line per mark-exact address-keyed sweep.
    // gci i27/gr: one `[g1-pinned-referents]` line per armed G1 pause (candidates, unheld, regions / bytes walked, or the decline reason).
    // G1 near-full proxy row: when the in-place pass declines on a heap with at most two Free regions, report what keeps its live set (by class; by JIT roots, pins and holder regions; each pin word's own retention and provenance). Diagnostic, at most 60 reports.
    // With the line above: the shortest path from a root to the first two objects whose class name contains this value (`Zs3Nd3UpcallMints$1`). Diagnostic.
    // The reachability oracle that judges the three narrowings above.
    E { group: Group::GC, token: "g1-only-jit-pins", on_key: Some("CRATONVM_GC_G1_ONLY_JIT_PINS"), off_key: None, off_word: None, since: "2026-09-06" },
    // The three conservative-JIT-root narrowings the register oop maps
    // license, each its own lever because each rests on a DIFFERENT claim and a
    // regression has to be attributable to one of them: the compiler's register
    // model, the operand-spill cursor, and the outgoing-argument reserve.
    E { group: Group::GC, token: "queued-finalizables-are-roots", on_key: Some("CRATONVM_GC_QUEUED_FINALIZABLES_ARE_ROOTS"), off_key: None, off_word: Some("0"), since: "2026-10-25" },
    // gen r5w2/oomjit6: opt-in (default OFF). A safepoint that staged a
    // reference where no map names it keeps its register-oop mask instead of
    // withdrawing it; the method's outgoing-reserve claim is withdrawn instead
    // (`jit/src/x64/safepoint.rs`, `live_oop_register_mask`; `x64/driver.rs`).
    // G1's pin set honouring the movable/rewritable partition the
    // generational path has always honoured.
    // DEFAULT-ON since 2026-09-12, so a KILL SWITCH, and it grew an `off_word`
    // with the flip -- the same correction the ZGC rows above needed, for the
    // same reason: without it `CRATONVM_GC=-g1-movable-pins` expands to
    // UNSETTING the key, and an unset key now means ON. It shipped opt-in on
    // 2026-09-09 having measured itself worth 1 pin in 34; the deopt
    // `SavedRegisters` partition (`FrameLayout::deopt_gpr_lo`) is what made it
    // worth 6 of 7. See `gc/src/g1.rs::g1_movable_pins_enabled`.
    // The producer half: publishing the verifiable band partition as movable.
    // UNSAFE pricing lever for the A5 unregistered-frame span sweep.
    // Recover frames from the A5 band instead of sweeping it as a raw span.
    // Coverage oracle for the slot list `static-root-slots` builds: after the
    // fast path has patched the recorded slots, re-walk every static the slow
    // way and report any that still names a moved address. A miss there is not
    // a wrong number, it is a live static field left pointing at a vacated
    // address, and a unit test cannot answer the question on a real class set.
    // The GC-trigger publish's coverage oracle: read the lock-free triple, then
    // take the lock and assert the arena agrees. Under the lock the two MUST be
    // equal -- every writer holds that mutex and the guard republishes before
    // releasing it -- so a divergence is a mutation that reached the arena
    // without passing through `YoungFromGuard::drop`, which is the only way the
    // design can be wrong.
    // `MarkBitmap::clear`: calls, calls that actually cleared, words, nanos.
    // The pair `calls`/`worked` is the engagement half -- the thing being
    // measured is the `any_marked` early return, so `worked == calls` says it
    // never fired and the reading is about the clear loop instead.
    // G1's `is_object_address`: calls, acceptances, nanos. The question it
    // answers is whether G1 should get the exact object-start bitmap the
    // generational predicate got, and the answer is a share of a run.
    // `update_all_roots`: calls, pointer-map entries, nanos. What the
    // value-carrying root ABI costs per run -- `rootprof` prints only when one
    // call exceeds 20 ms, which cannot see a thousand 1 ms fix-ups.
    // Value-carrying: the `[gcpause]` report threshold in microseconds
    // (default 100000; `0` = every cycle). gen r4w2/obs.
    // The PER-SITE half of the JIT reference-load census: which of the five
    // read-helper arms served each reference-slot read. Off by default because
    // one ungated relaxed increment on `getfield_compact_ref` -- the hottest
    // read in the VM -- was measured at 2-3 ns of a 9 ns reference-field read.
    // The two colored-word tripwire counters print either way; they sit on a
    // `#[cold]` arm and cost nothing to keep on.
    // jit (dev 7b41ee4ca4): scan a published body for a baked `mov rax, imm64; call rax` whose target no callee pin lists.
    E { group: Group::DBG, token: "getresources", on_key: Some("CRATONVM_DBG_GETRESOURCES"), off_key: None, off_word: None, since: "2026-05-20" },
    // Per-snapshot trace of the typed operand stack (x64::stack_kinds): what
    // the analysis had at each deopt bci and whether the depth / oop-mark
    // agreement checks accepted it.
    // Tally every `real_protected_stub_class` question and its answer, so the
    // stub door can be priced rather than argued about.
    // Companion to `stub-door`: that one says which doors ASK the SyntheticStub
    // arbitration, this one tallies which code path actually INVOKES the native
    // funnel, by call site.
    E { group: Group::DBG, token: "iae-trace", on_key: Some("CRATONVM_IAE_TRACE"), off_key: None, off_word: None, since: "2026-05-20" },
    // `cast-memo-crosscheck` — re-run the full `instanceof` path on every
    // negative-memo hit and report a disagreement. Presence-parsed.
    E { group: Group::DBG, token: "invokestatic-loader-trace", on_key: Some("CRATONVM_INVOKESTATIC_LOADER_TRACE"), off_key: None, off_word: None, since: "2026-07-22" },
    // JIT r23 wave 11 (lane L2): fix, default ON (`0` = the unconditional bump). An optimizing
    // publish that displaced no body (a first publish, no other publication of the name in the
    // compile's window) leaves the VM supersede epoch alone, so it no longer evicts every `Jit`
    // invoke-cache entry of the VM (vm `supersede_epoch_bump_needed`).
    // gch h4/jm: opt-in. The single-pass GC maps (Stage 2, the local oracle, the register mask, the shadow push) drop every liveness-dead reference local, not only one whose register is shared (gch-h3fd proposal; d9d's spliced-direct case).
    // t1 w9 ARGREF (r21 OOM21-6 re-land), correctness, default ON (`=0` kills it): under `sp-map-local-liveness` the every-dead rule stays monotone -- it speaks inside splices and searches past the 64-byte next-instruction bound (the pc's own liveness answers with no later instruction).
    // gci i1/sp, default ON since gci i2/sp (`=0` kills it): a single-pass call zeroes the popped reference arguments' source slots (the words below the cursor its staging copied from) once the staging read them (gcg-g5jm's holder).
    // gci i3/il (opt-in): a splice return arm zeroes the callee's dead reference locals and operand words (the soft-referent holder of SoftRefBeforeOome).
    // The witness half of `jit/code-near-globals`: one line per placement
    // decision. That flag is a HINT to `mmap`, so "set" and "engaged" are
    // different facts and only this line separates them.
    // Which cell the layout-epoch counter ended up in, and its address. The
    // witness for `jit/epoch-cell`, whose install FAILS CLOSED and would
    // otherwise do so silently.
    // The trace half of `jit/ir-linear-scan`; same token name in the group that
    // owns tracing, exactly like `ir-long` and `xt-jit-root-scan` below.
    // Interpreter round i2 wave 63 (lane L2): one line per optimizing compile
    // counting the intervals `CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES` would
    // shorten (`jit::regalloc::token_trim_census`). Diagnostic only.
    // JIT r24 (CF residual page): one line per resolved spliced body: its never-taken prune rows and the branch counts they came from.
    // gci i5/jd: one `[JAVAC-INFER] ... moved` line per inferBinaryName call whose pinned objects moved under an upcall.
    // Scalar, not a boolean: `=<n>` raises the `--real-jdk` shadow-observation
    // sink above its 256-row default so an APPLICATION census is complete
    // rather than truncated. Diagnostic only — it changes nothing a program can
    // observe. Declared rather than read through bare `getenv` so it comes from
    // the latched snapshot like every other knob; see the doc on
    // `vm::jdk_only_native_shadow_cap`.
    E { group: Group::DBG, token: "native-shadow-sink-cap", on_key: Some("CRATONVM_NATIVE_SHADOW_SINK_CAP"), off_key: None, off_word: None, since: "2026-08-17" },
    // JIT r23 w2c close: names each array bounds check the IR range pass keeps (index op, reason).
    // Round rx27 wave 5, lane guards: one line per IR inline field site that keeps the full compact test / legacy
    // arm / epoch compare, with its receiver and the first reason the proven-receiver mode declined (jit
    // `Lowerer::proven_field_site_census`). Diagnostic only.
    // zs s7/vl: a comma-separated list of substrings of `Class.method` (internal class names, as CRATONVM_JIT_DENY) whose methods the optimizing tier refuses (method entry, OSR, C1->C2 supersede: every door that reaches `try_compile_inner`) and whose constant-pool call sites the optimizing tier does not splice, so they run their single-pass body. Measurement switch for single-pass emitters; unset = off; never changes the default tiering.
    // Interpreter round i6 wave 116 (lane L5): every `Inflater.inflate*` native
    // call tallied by stream kind and consumer frame, dumped at exit (the census
    // `i64-L5-proposal-serve-zipfile-entry-streams-from-the-vm-archive` asks for).
    // Traces a field resolution that matched on NAME only after the
    // name+descriptor lookup missed -- the separate-compilation shape where
    // the JVMS key and the name key disagree. Read presence-only
    // (`runtime_var_os(..).is_some()`) at vm/src/runtime/resolve/mod.rs, so no
    // off_word. Declared here after `flag_declaration_guard` caught it reading
    // through a live `getenv`, which is how a flag-dependent test ends up
    // measuring the developer's ambient environment.
    // Four more presence-only DBG traces from the 2026-08-27/28 JIT wave, all
    // read through a live `getenv` until this declaration.
    // 2026-09-17, from the quarkus lane. Two presence-only traces, both read
    // with `std::env::var(..).is_ok()` at their sites and neither able to
    // change a result: `cmctx` prints why SmallRye's config-mapping context
    // construction took the arm it took, `findclass` prints the lookup class
    // and loader `MethodHandles.Lookup.findClass` resolved against.
    // The LARGE-OBJECT end's compactor, one line per engaged cycle: what it
    // moved and what the high free list looked like on either side of it.
    // Declared 2026-08-29.
    // Round 16 wave 15 lane mh6: one stderr line per cold fill of a compiled
    // `MethodHandle` site -- the constant binds' engagement record.
    // Declared 2026-08-23. `jit-rootscan` reports the moving-young coverage
    // verdict as an aggregate `map_coverage=N` counter, which names neither the
    // METHOD nor WHICH of `fully_oop_covered`'s four terms said no. `oopcov`
    // prints both, per compile. See `jit/src/x64/driver.rs`.
    // Declared 2026-08-23 with the cross-thread JIT coverage handshake: the
    // per-cycle `peer_depth`/`proven` arithmetic and each peer's deposit. See
    // `vm/src/jit/conservative_roots.rs::publish_peer_jit_coverage_for_stw`.
    // Per-phase timing for `try_lambda_dispatch` (lookup / prep / target /
    // other), printed every 200k dispatches. Arms the timers; an unarmed run
    // pays one relaxed load per dispatch. See `runtime::interpreter::lambda::lambda_prof`.
    // JIT round 21 lane tiermgr (ACCEPT21-1 first step): one `[accept-census]` line per lowered optimizing
    // body -- what the evidence gate would answer under every `CRATONVM_C2_ACCEPT` policy, its priced
    // estimate, whether the body was kept. Diagnostic only.
    // nio21 wave 6 L1: diagnostic; prints why the HUC StreamingOutputStream wrapper was refused.
    // nio21 wave 15 L3: report drained methods above the C2 bar that are not promoted.
    // nio21 wave 8 L4: the exit-time census lines (dropped stores, statics scan, coercion, FFM / filechannel / vector summaries, System.exit and shutdown-hook lines); off by default for HotSpot stderr parity.
    // JIT round 23 wave 1 L11: CRATONVM_EXIT_REPORT_FILE=<path> appends the exit_report! lines (cratonvm_types::exit_report) to that file instead of stderr and turns the exit report on; unset = stderr under exit-census only.
    E { group: Group::DBG, token: "exit-report-file", on_key: Some("CRATONVM_EXIT_REPORT_FILE"), off_key: None, off_word: None, since: "2026-10-21" },
    E { group: Group::TEST, token: "jdk", on_key: Some("CRATONVM_TEST_JDK"), off_key: None, off_word: None, since: "2026-05-20" },
    // nio21 wave 8 L4: the launcher's narration lines (main-vm run() returned Ok, the jdk mode stamp, the --real-jdk banner); off by default for HotSpot stderr parity.
    // zt t7/cl2: a print native that lets a line go nowhere (closed stream, a sink that threw, a failed autoflush, no sink and no console fd) says so on the host's stderr: `[cratonvm] print-loss: <why> stream= thrown= bytes=`. Separates "output lost" from "never printed" (launcher-outcome's `main entered` / `main ended` lines separate "main never ran"). Diagnostics only, default off.
    // OSR round (2026-10-06): name why the loop field forward pass left a loop alone.
    // Restores the pre-fix load-time transform behaviour: offer every class to
    // the `ClassFileTransformer` chain on every constant-pool resolution rather
    // than once per name. The red control for the load-time-transform rescan
    // fix (see `runtime::instrument::LoadTimeOffered`) — with it set, a Spring
    // Boot `@ClassPathExclusions` test under Mockito's inline mock maker hangs
    // instead of passing.
    // The keySet-view rebuild-elision census, printed at exit by
    // `native_collections::report_map_view_cache_at_exit`. `resync_skipped` is
    // the ENGAGEMENT counter for that fast path: a wall-clock number quoted
    // without it cannot say whether the path ran at all.
    // gci i40/fg: one `[mhn-static-field]` line per static-kind `MemberName`
    // the `MethodHandleNatives` static-field natives could not read (which
    // read failed); diagnostic only.
    // Interpreter round i7 wave 136, lane L2 (stage 0 of `i25-L2`): the
    // interpreter-to-compiled door's own cycles against its body's, printed at exit.
    // JIT round 17 wave 2 (lane funnel): the JIT native-site door's cycle
    // split per crossing (front / funnel / tail), printed every 2^18
    // crossings as `[native-door-split]`.
    // JIT round 19 wave 2, lane funnel2: the SBR5 lean native door's census
    // (probes, served by wrapper, misses empty / other site, declines by
    // reason, front / call / tail cycles per served crossing), printed at exit
    // as `[native-site-door]`. The split census above never sees these crossings.
    // JIT round 19 wave 2, lane funnel2 (FUNNEL17-4): at exit, every registered
    // `Intrinsic` / `Bridge` whose loaded JDK class declares a concrete body for
    // the same triple, with that body's size, invoke count and this run's
    // dispatch count, as `[funnel-intrinsic-census]` (candidates to un-register).
    // The notification-credit census -- how much of this VM's Object.wait()
    // waking actually came from the CONDITION rather than from the condvar
    // signal. `credits_consumed` is the engagement counter for the netty
    // lost-wakeup fix.
    // The execution profiler (`runtime::exec_sampler`). A VALUE flag: the
    // millisecond sampling interval, off when unset or 0. It exists because
    // there was no way to ask this VM where a workload's time goes --
    // `jdk.ExecutionSample` is defined in the JFR crate with no caller, and
    // `wpr -start CPU` needs a privilege this host does not carry.
    // zs s3/cr: every span the evacuating slide would free is treated as screen-blocked, so the per-object free of moved survivors' old extents runs (its positive control). Debug only.
    // Declared 2026-08-17: both were read by `runtime_var_os` and named nowhere,
    // so `CRATONVM_DBG=mapgen` could not reach them and a test could not arrange
    // one with `flags::with_thread_overrides` -- and while the guard was red it
    // could not catch the NEXT undeclared flag, which is what it is for.
    // `mapgen` reports a safepoint waiter that resumed on a pointer map from a
    // different pause generation than the one it arrived for; `vacated-frames`
    // arms the vacated-address ledger that catches a frame slot still holding an
    // address the collector moved away from.
    // Pairs the per-cycle relocation verdict with `ThreadStateCensus::
    // relocation_blockers()`, which is the codebase's own statement of the
    // `mark_moving_young_coverage_incomplete_because` obligation. Off by default.
    // Diagnostic widening of the register-image remap to the whole unverifiable
    // frame tail, to TEST the four-region partition in `conservative_roots`'s
    // module comment rather than continue to argue it. Off by default; see
    // `register_image_remap_admits`.
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
    // gce e1/j: per-phase nanosecond census of a JNI native call (door, dispatch,
    // in-native deposit sub-phases); one `[GC] jni_phase:` line at exit.
    // gch h4/bp: UNSOUND positive control -- the pause does not scan the in-native bands
    // (`gc_and_alloc::append_in_native_band_roots`); only with `jni-in-native-band-at-pause`.
    E { group: Group::DBG, token: "nsee-trace", on_key: Some("CRATONVM_NSEE_TRACE"), off_key: None, off_word: None, since: "2026-05-20" },
    // gen r4w5/oomjit5: per-root-category attribution of what every STW
    // Generational major keeps alive, printed at the end of the major
    // (`gc/src/gen_heap_oldmark_census.rs`), plus the JIT catch doors' thread
    // line (`vm/src/jit/helpers.rs`).
    // Declared 2026-08-11. The probe `d1648f133` left behind in
    // `memory::native_roots` after fixing `scan_collection_overlays`'s gate --
    // it prints which of the four predicates the conditional actually turned
    // on, which is the question that fix got wrong. It reads through
    // `runtime_var_os` already; it was simply never named here, so the grouped
    // `CRATONVM_DBG=overlay-gate` spelling could not reach it.
    // Takes a VALUE, not a presence: a class-name substring restricting the
    // frame trace. The grouped spelling expands to `=1`, which matches no class
    // name, so `CRATONVM_DBG=osr-frame-trace` arms nothing on its own — which is
    // deliberate. An unfiltered frame trace emits a line per back edge in the
    // JDK. Set `CRATONVM_DBG_OSR_FRAME_TRACE=<substring>` directly. Declared
    // here anyway, because an undeclared name is served from live `getenv`
    // rather than the latched snapshot and is invisible to the override hook.
    // Interpreter round i6 wave 117, lane L2: one stderr line per OSR return of a remounted
    // virtual-thread leaf that owes a continuation return adapter (`try_osr_offer`; page
    // `i116-L9-osr-return-skips-continuation-return-adapters`). Diagnostic only.
    // Declared 2026-08-22. `view-kind` reports a map-view carrier whose CLASS
    // and whose head element disagree about what it holds, and every `vc_route`
    // rebuild; `view-resync` reports a TreeMap range view's rebuild -- pairs
    // seen, kept, bounds and the comparison verdict histogram. The pair
    // separated three defects that all present as one red vector: an entrySet
    // that came back holding values, a view that came back EMPTY, and a stale
    // element inside the scan. An empty view that saw no pairs and one whose
    // every comparison landed out of range are different defects and nothing
    // else can tell them apart.
    // `overlay-nodedup` — report EVERY model-slot access, not the first per
    // (class, slot, read/write). The default cap exists because the complete
    // list of disagreeing slots is already printed once per class by the
    // shadow-layout census, so a repeat line adds nothing but volume — and
    // `java/lang/String` slot 1 alone would bury the run. Turn it off when you
    // want per-site COUNTS rather than per-site presence.
    // `jfr::phase` — phase accounting. The enable gate takes a *level*
    // (`1`/`true`/`on`/`coarse`, or `fine`), and the two sinks take paths:
    // `CRATONVM_DBG=phase-accounting=fine,phase-accounting-out=/tmp/p.json`.
    E { group: Group::DBG, token: "phase-accounting", on_key: Some("CRATONVM_PHASE_ACCOUNTING"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::DBG, token: "phase-accounting-jfr", on_key: Some("CRATONVM_PHASE_ACCOUNTING_JFR"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::DBG, token: "phase-accounting-out", on_key: Some("CRATONVM_PHASE_ACCOUNTING_OUT"), off_key: None, off_word: None, since: "2026-07-31" },
    // JIT r20 wave 11 lane pProf (W15L2-P3): one `[prof-rows]` line per method slot that gets lock-free receiver rows.
    // gci i35/wy: diagnostic, UNSOUND (rows only).
    // gci i38/ds: diagnostic, changes no root.
    // zt t1/nd: print the cumulative `[GC] zgc-define-mirror: reserved= refused= adopted=` census on stderr at every refused define-mirror reservation (ZGC; the Zt1NdDefineRetry rows read it).
    // zt t18/zb1: the armed ZGC read-barrier census, `[GC] zgc-load-barrier: lb_*` at exit; "2" also makes the heap accessors skip their armed arm and count it (lb_unrouted's positive control). Diagnostic.
    // zt t23/zc5 (R1 of docs/feature-designs/zgc-architecture.md): one `[GC] zgc-rsel:` line per whole-heap ZGC cycle comparing the relocation set chosen from the mark (before the sweep) with the slide's own selection (rsel_regions= rsel_agree= rsel_empty= rsel_straddled= rsel_excluded= ...). Diagnostic, reads only.
    // zt t24/zc8: with CRATONVM_ZGC_MARK_CONTINUE acting, every ZGC Pause Mark End attempt answers "mark work left" until the cycle's 4 attempts are used, so each closing collection first releases the world 4 times (the positive control of the release path: `door=zgc-mark-end` pause lines, the `O: Pause Mark End` / `O: Concurrent Mark Continue` gc,phases lines, mcc_continues). Diagnostic.
    // zt t11/pl3: under CRATONVM_ZGC_METASPACE_EDGE_TALLY, also walk the class store and print `[metaspace-tally] differs tally=(used,committed) walk=(used,committed)` on stderr when the tally and the walk disagree.
    // zt t14/pl5: with CRATONVM_DBG_METASPACE_TALLY, add <n> bytes to the tally's used figure in its comparison with the walk only (the positive control of the `[metaspace-tally] differs` line).
    // zt df4 D8 (2026-10-23): =<ms> holds a Proxy bridge's fresh $ProxyN between its proxy-cache insert and its ClassLoader.addClass upcall for <ms> milliseconds (Thread.sleep, GC-safe), so a second thread's getProxyClass meets the cache entry while the upcall has not run (the race row of zgc-proxy-upcall-serialized, probe Zt31D8ProxyUpcallRace).
    // zt t14/pl5: the perf-data sampler's period in milliseconds (default 100); a row that must tell an event-time counter write from a sampler pass sets it beyond the run.
    // zt t9/df: print `[define] add-class orphan noted: class_id=` when a define's ClassLoader.addClass upcall fails after the publish, and `[define] add-class orphan adopted: <name> class_id= ok=` when a retry adopts it (zgc-define-add-class-retry; the Zt6Nd4AddClassBoundary rows read it).
    // zt t10/df2: print `[define] add-class door=<door> class=<name> ok=<b>` for each ClassLoader.addClass upcall a define door makes, naming the door (defineClass0/1/2, unsafe, lookup, proxy): which door served a Zt8X5AddClassDefiners / Zt10Df2AddClassDoors row.
    // zs s5/ta: log the next N `alloc_raw` hand-outs after each ZGC refusal, with what the span held before zeroing.
    // zr z4/rv4: fault injection -- every ZGC parallel stop-the-world mark claims its roots and fails, driving the serial fallback (positive control of CRATONVM_ZGC_PARMARK_FALLBACK_RESCAN).
    E { group: Group::DBG, token: "zgc-parmark-fail", on_key: Some("CRATONVM_DBG_ZGC_PARMARK_FAIL"), off_key: None, off_word: None, since: "2026-10-13" },
    // zwr: name the first referents each reference round kept (stderr).
    // zr z1/br: name each ZGC mark start the take-over cover refused (stderr).
    // gc-common w3-d: run finalize(), cleaner actions and the GC-enqueue
    // `ReferenceQueue` wake-ups on a per-VM delivery thread instead of the
    // allocating mutator (JLS 12.6). Default off until the battery has run it.
    // zwr: default ON (a stop-the-world pause links a cleared Reference into a real-JDK
    // ReferenceQueue itself when no thread holds the queue's monitors), =0 kills.
    // gci i42/rp: default ON (a remark that queued finalizer / cleaner work raises the ref-work hint), =0 kills.
    // gc-common w6-g: JNI local refs handed to native code as indirect
    // `(frame, slot)` handles into the remapped local-frame table instead of
    // raw addresses, so a native's own copy follows a moving collection.
    // Default on since the gc JNI retirement (2026-10-10); `=0` kills it and
    // the foreign transitions with it.
    E { group: Group::GC, token: "jni-indirect-locals", on_key: Some("CRATONVM_JNI_INDIRECT_LOCALS"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // gc-common w10-c: a foreign-attached thread runs every JNI function as a
    // counted mutator (idle -> running -> idle per function) and keeps the
    // locals it creates outside a Java call in an attach-level frame until
    // detach. Default on since the gc JNI retirement (2026-10-10), never
    // without `jni-indirect-locals`; `=0` kills it.
    E { group: Group::GC, token: "jni-foreign-transitions", on_key: Some("CRATONVM_JNI_FOREIGN_TRANSITIONS"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // gcd d3/k: a Java thread inside a JNI native method runs in native
    // (GC-safe, HotSpot's `_thread_in_native`) and every JNIEnv function it
    // calls transitions native -> VM -> native, so a native that blocks in C
    // holds no pause. gcd d10/j: the PACKAGE switch; since 2026-10-10 its two
    // companions `jni-indirect-locals` and `jni-foreign-transitions` are on
    // by default anyway; `jni-foreign-transitions` is refused without indirect
    // locals.
    // Default off until the JNI corpus / netty-tcnative / JNA runs.
    E { group: Group::GC, token: "jni-native-transitions", on_key: Some("CRATONVM_JNI_NATIVE_TRANSITIONS"), off_key: None, off_word: Some("0"), since: "2026-09-27" },
    // WARNING gci-user-flip-20261013: default ON since 2026-10-15 (vi19-jt E + S + A2 PASS); `=0` kills.
    // JIT round 16 wave 12 (lane inpkg): the staged flip of the package -- on
    // as if `jni-native-transitions` were set, unless that switch is set
    // explicitly (`native::jni::native_transitions_default_from`). Opt-in
    // until the orchestrator's A/B; the real flip makes it default on.
    E { group: Group::GC, token: "jni-native-transitions-default", on_key: Some("CRATONVM_JNI_NATIVE_TRANSITIONS_DEFAULT"), off_key: None, off_word: Some("0"), since: "2026-10-02" },
    // gcf f1/j: the package's in-native deposit drops compiled-frame words
    // outside the heap envelope, and frames with none, before the band claims
    // (exact). On with `jni-native-transitions`; `=0` is the kill switch.
    E { group: Group::GC, token: "jni-in-native-envelope-first", on_key: Some("CRATONVM_JNI_IN_NATIVE_ENVELOPE_FIRST"), off_key: None, off_word: Some("0"), since: "2026-10-01" },
    // gch h1/jw: an attached thread publishes its OS tid while it runs a Java
    // call (or a JNI function), with the defaults too, and withdraws it while
    // idle. Default on; `=0` restores publishing only with
    // `jni-foreign-transitions`.
    E { group: Group::GC, token: "jni-foreign-tid-publish", on_key: Some("CRATONVM_JNI_FOREIGN_TID_PUBLISH"), off_key: None, off_word: Some("0"), since: "2026-10-08" },
    // gch h2/jr (gch-h1jw-proposal-in-native-arrival-without-waiting): a thread going into
    // native during a pause arrives and does not park (`GcBarrier::arrive_in_native`). Opt-in,
    // only with the package and its no-signal mark.
    E { group: Group::GC, token: "jni-in-native-arrive-no-wait", on_key: Some("CRATONVM_JNI_IN_NATIVE_ARRIVE_NO_WAIT"), off_key: None, off_word: Some("0"), since: "2026-10-08" },
    // gch h4/bp (gcg-g4jn-proposal-in-native-band-scanned-at-the-pause): a JNI native called from
    // compiled code leaves its compiled band to the pause (`ThreadRegistry::scan_in_native_bands`)
    // instead of the deposit's coverage proof and chain scan. Only with the package and its
    // no-signal mark; Generational only. Default ON with the package since gci i1/jn; `=0` kills it.
    E { group: Group::GC, token: "jni-in-native-band-at-pause", on_key: Some("CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE"), off_key: None, off_word: Some("0"), since: "2026-10-09" },
    // gci i5g/g1r (default ON since vi6-g1r, `=0` kills): the in-native band at pause on G1 too (gcf-f1j G1 half; the band roots are pinned for the cycle).
    // gci i48 jl48 (default ON, `=0` kills): the JNI in-native leave's full wake keeps the snapshot its
    // window's pauses folded (interpreted frames only) and a VM-only JNIEnv function re-enters native
    // incrementally after it (`vm/src/native/jni.rs`, `check_post_block_gc_in_native`).
    // The named-writer arm of the punned-reference counter: on a NON-ZERO
    // payload word under a non-`Object` tag, print the class and field so the
    // writer can be found rather than inferred. Diagnostic only -- the counter
    // itself is unconditional; this names what it counted.
    // Per-event trace of the map/set VIEW comodification door: which check
    // fired, and whether the modCount stamp behind it is live or dead. A
    // pass/fail cell cannot tell a working door from a dead stamp, which is
    // what this exists to distinguish.
    // Was the bare `CRATONVM_DBG`, which is now the group variable itself. It
    // gated exactly one call site (`quarkus_staticinit.rs`), so it becomes an
    // ordinary topic. `CRATONVM_DBG=1` no longer enables it — that spelling now
    // reports `1` as an unknown token, which is the point.
    // Suppresses the "falling back to the process environment" notice that
    // `spring_startup_bootstrap` prints; a *silencer*, so it is stated
    // positively here and the topic it silences is the notice itself.
    // Interpreter round i2 wave 54, lane L3: one line per reflective listing
    // that resolved its members' signature types.
    // Interpreter round i2 wave 55, lane L5: one line per built-in loader
    // package listing that put new packages into the loader's map.
    // Interpreter round i2 wave 59, lane L5: one line when the launcher's
    // `LauncherHelper.checkAndLoadMain` answered the main class.
    // Interpreter round i2 wave 55, lane L5: one line per null-name
    // `ClassLoader` resource call a retired intercept left to dispatch.
    E { group: Group::DBG, token: "selector", on_key: Some("CRATONVM_DBG_SELECTOR"), off_key: None, off_word: None, since: "2026-06-18" },
    // Interpreter round i5 wave 106, lane L7: one `[shadow-rerun]` stderr line
    // each time a native shadow of library bytecode (ANTLR, BouncyCastle, H2,
    // JBoss, Lucene) hands a call back to that bytecode.
    E { group: Group::DBG, token: "soft-exit", on_key: Some("CRATONVM_SOFT_EXIT"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "stale-objref-cycles", on_key: Some("CRATONVM_DBG_STALE_OBJREF_CYCLES"), off_key: None, off_word: None, since: "2026-07-22" },
    // JIT round 18 wave 10, lane ffmeintr: `=N` seconds; a stop-the-world wait
    // or a claimed lazy in-native leave that outlives it prints every thread's
    // state (`thread_registry::stw_watchdog_secs`).
    // Same lane: debug builds only, the native no-GC allocation doors assert
    // that no pause completed inside them (`vm_exec::native_alloc_no_gc_point`).
    // Same lane: with `CRATONVM_XT_HELPER_UNPARKED_READ`, park and read every
    // unparked-readable peer too and compare (`xt_root_scan::unparked_read_audit_enabled`).
    E { group: Group::DBG, token: "surefire-ipc-dbg", on_key: Some("CRATONVM_SUREFIRE_IPC_DBG"), off_key: None, off_word: None, since: "2026-05-20" },
    // JIT round 19 proposal wave 8 (lane gc8): GC719-3, the moving-young coverage proof checks that each chain entry's innermost-frame mirror names a frame that called out of compiled code, fails closed and names a failure (debug, opt-in).
    // JIT round 19 proposal wave 9 (lane gc9): GC819-2, under the innermost-mirror check the found return address must be anchored to a single-pass frame's active safepoint (else the proof is refused); off, a disagreement only prints a census line (debug, opt-in).
    // One line per starvation-watchdog sample: the carrier pool's queue depth,
    // busy/live counts and dispatch counter, plus the per-state thread census.
    // This is the reading that showed the pool running away from its base 32 to
    // 233 on `VthreadGcStress` while `dispatch_count` sat frozen -- the shape a
    // wall clock reports only as "the VM hung". Sampled on the watchdog's own
    // interval, so it costs nothing when off and nothing hot when on.
    E { group: Group::DBG, token: "sweep-zero", on_key: Some("CRATONVM_DBG_SWEEP_ZERO"), off_key: None, off_word: None, since: "2026-06-16" },
    // JIT round 19 wave 9, lane handler9 (HANDLER8-1 census): diagnostic, default OFF. Counts C1 bodies published
    // outside the tiered manager that pass `c2_upgrade_would_engage`, and how many later cross the C2 bar anyway;
    // one line at exit (jit `tiered::OutsideManagerC2Census`).
    E { group: Group::DBG, token: "trace-sb-filter", on_key: Some("CRATONVM_TRACE_SB_FILTER"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::DBG, token: "watch-cell", on_key: Some("CRATONVM_DBG_WATCH_CELL"), off_key: None, off_word: None, since: "2026-07-03" },
    // JIT round 19 wave 6, lane throws6 (THROWS6-2): per-thread section
    // averages of the native `StackWalker.walk` and `initStackTraceElements`.
    // gce e2/t: debug -- a compiled poll declines to park for at most 200 ms per
    // pause, so the take-over freezes peers in compiled code (probe arm only).
    // gch h3/fd: diagnostic. One `[xt-refusal] arm= tid= reason=` stderr line per helper window that refused a collection (gcd-d9c step 1).
    // gch h4/jm: diagnostic. A compile-time `[sp-band]` line per single-pass safepoint map naming each operand-spill word's owner class; `1` = every method, else a method-label substring.
    // Declared 2026-08-06 with the DBG block above. The five `sp-ic-*` /
    // `sp-inline-*` knobs are the single-pass inline-cache bisection surface;
    // three are DEFAULT-ON and read `=0` to disable, so they carry `on_key`
    // and the "0" off-word rather than an `off_key`.
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
    // Consecutive below-min-work refusals after which a CALL SITE is allowed
    // into the invoke cache. Requires `invoke-cache-pc-key`: without it a
    // "site" is a method reference and promoting one caller deoptimises its
    // siblings (measured 18x on GpuHookOverheadBench).
    // Adds the call site's bytecode offset to the invoke-cache key, so two
    // sites invoking the same method stop sharing one entry.
    // Invoke-cache hit/miss counts at exit; the acceptance criterion for
    // `invoke-cache-pc-key`.
    E { group: Group::JIT, token: "sp-ic-deny", on_key: Some("CRATONVM_JIT_SP_IC_DENY"), off_key: None, off_word: None, since: "2026-08-05" },
    // A/B lever, never a supported configuration: restore the pre-fix
    // substitution of "slot 0, tagged int" for a field site the VM-side
    // resolver declined. Declared so the arm can be spelled, and so that
    // `flags` reports it as SET when somebody leaves it on by accident.
    E { group: Group::JIT, token: "sp-ic-only", on_key: Some("CRATONVM_JIT_SP_IC_ONLY"), off_key: None, off_word: None, since: "2026-08-05" },
    // gci i23/r6 (opt-in): the frozen-band memo answers every root deposit, not only JNI / FFM / blocking natives; never the collector's own root pass or coverage proof.
    // 2026-09-08. OPT-IN: the collection's own root pass also scans the
    // native-stack band ABOVE the JIT entry chain conservatively. Written as a
    // fix for the BindableTests ByteBuddy reclaim and measured NOT to be one —
    // kept as the lever that says so. See `above_chain_scan_enabled`.
    // A/B opt-in restoring the pre-2026-07-31 single global `Mutex` in
    // `types::jit_activation`; presence-parsed (`runtime_var_os(..).is_some()`),
    // so `=0` still enables it and `off_word` must stay `None`.
    // Sinking the per-safepoint blind GPR spill at an inline-TLAB `new` onto
    // the allocation's slow-path edges. Presence-parsed opt-out, so `=0` still
    // disables the sink and `off_word` must stay `None`.
    // Declared 2026-09-02 with the three codegen changes of
    // `array-element-load-baseline-codegen-20260901`. All three are default-ON
    // and all three change the EMITTED BYTES of code that runs on every
    // iteration of every array loop, so each gets its own lever at the level
    // the change is at.
    // The bytecode loop rewriter (`x64::plan_bytecode_loop_xform`: peel,
    // unroll, guarded versioning). Default-**OFF**, and arming it also turns
    // the native byte-copy unroller off — the two are exact complements, and
    // both firing on one loop would put `(k+1)^2` bodies behind one back-edge
    // poll. Sufficient on its own since `loop-02` retired the `deopt-real`
    // whole-compile refusal: this token alone now reaches loops under the
    // default configuration, and pairing it with `-deopt-real` measures the
    // deopt-real-off configuration rather than the transform. See
    // `docs/jit/loop-rewriter-wiring.md`.
    // Default-ON: `x64::escape_analysis::bulk_byte_loops_enabled` reads
    // `0`/`false`/`off` (trimmed, case-insensitive) as the kill switch.
    // Default-ON (PERF-01): the optimizing tier declines a method whose loops
    // the single-pass backend would VECTORISE, because an IR body for one of
    // those is a downgrade — measured at 6.4x on `CratonBench.sieve`.
    // `CRATONVM_JIT='-c1-vector-veto'` hands those methods back to the IR
    // tier, which is how the regression is reproduced and how a probe that
    // wants IR codegen for a sieve-shaped method gets it (this term sits
    // AFTER `CRATONVM_JIT_FORCE_C2` in the admission chain).
    // 2026-09-24. `Unsafe.compareAndSet{Int,Long}` thin direct-call bind
    // (AQS `compareAndSetState`, i.e. every `CountDownLatch.countDown` from
    // compiled code). Default ON; `=0` sends every site back to the funnel.
    // 2026-09-17. RT-8's cold-body sweeper: at the cap, retire the bodies whose
    // compiled entry counter has not moved since the previous sweep. Opt-in,
    // and it declines by itself unless `CRATONVM_JIT_ENTRY_COUNTER` is also on,
    // because that counter is the only per-artifact usage signal in the tree
    // and without it every body reads equally cold.
    // JIT round 22 wave 12, lane runtime (behaviour, default ON, `=0` the old
    // uncounted optimizing body): while the cold-body sweeper is armed, an
    // optimizing body counts its entries too, so the sweeper never takes a hot
    // C2 body for a cold one.
    // r22 review (wave 12): correctness, default ON, kill "0" (the wave-11 process-wide sample). The cap-refusal
    // witness is a refusal on the compiling thread itself, so a genuine codegen failure on one worker is not deferred
    // because some other compile was refused at the cap meanwhile. jit `tiered::note_compile_cap_refused_on_this_thread`.
    E { group: Group::JIT, token: "ctor-direct-call", on_key: None, off_key: Some("CRATONVM_NO_CTOR_DIRECT_CALL"), off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "osr-ctor-bind", on_key: None, off_key: Some("CRATONVM_NO_OSR_CTOR_BIND"), off_word: None, since: "2026-08-17" },
    // Default-ON: `deopt_real_enabled()` reads an unset key as on and only `0`/
    // `false`/`off`/`no` as off. With `off_word: None`, `-deopt-real` unset the key
    // and so left precise resume ON, which is why this takes `"0"` (2026-09-12).
    E { group: Group::JIT, token: "deopt-real", on_key: Some("CRATONVM_DEOPT_REAL"), off_key: None, off_word: Some("0"), since: "2026-06-17" },
    // The tier-up deopt sink's precise resume. Default-ON with a `0` opt-out:
    // off, the sink asks `can_deopt_resume` again and raises the hard
    // `InternalError: ... refusing side-effecting replay` an optimizing-tier
    // artifact could never avoid. Measurement lever, not a configuration.
    // The pin's BLIND case, split out so it can be A/B'd on its own. The pin
    // above answers "keep this method off the optimizing tier"; this one
    // answers what to do when the compile door supplied no constant-pool
    // invoke resolver, so the pin cannot read the callee names it needs.
    // Default-ON means fail CLOSED (stay interpreted); the key's PRESENCE
    // restores the historical fail-open promotion to a tier that has no
    // String intrinsic emitter at all. Read by
    // `jit::string_pin_fail_closed_enabled`.
    // Transitive eager callee compilation, so a body compiled bottom-up binds its
    // statically bound call sites to raw CALLs instead of the generic dispatch
    // helper. Default ON; `CRATONVM_JIT='-eager-callee-chain'` restores the
    // one-level behaviour, which is the A/B control for the ~6.5x measured on a
    // call-dense loop. Correct either way — the fallback is the checked helper.
    // Fewest calls a callee needs before the eager callee chain compiles it for its
    // caller (default: a sixteenth of `CRATONVM_JIT_THRESHOLD`, at least 1; `0` is
    // the unconditional chain). A colder callee leaves its site on the dispatch
    // helper until the caller's own re-tier binds it.
    E { group: Group::JIT, token: "force-c2", on_key: Some("CRATONVM_JIT_FORCE_C2"), off_key: None, off_word: None, since: "2026-07-31" },
    // The class-blind arm of the native-shadow seal. Default ON (correctness
    // guard); `-native-shadow-interface-blind` measures its cost.
    // The whole-method native-shadow caller seal at the eager and upgrade
    // doors. Retired to OPT-IN by interpreter round i1 wave 42 (lane L2): the
    // default worker never asked it and screens native shadows per site;
    // `native-shadow-caller-seal` restores it.
    // The POSITIVE half of `execute()`'s static-eligibility short-circuit.
    // Default ON; `-gate-pass-memo` restores the pre-fix re-run-every-entry
    // behaviour so the fix can be A/B'd in one binary. Correct either way.
    // Default-ON: `x64::licm::gc_inert_selfrec_enabled` reads `0`/`false`/`off`.
    // Presence-parsed kill switch for the inline (helper-free) compiled
    // `getstatic` load, exactly like `getfield-helper` above: setting it
    // routes every static read back through `jit_getstatic`.
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
    // PGO-02: guarded monomorphic-virtual-call inlining (splice the callee
    // body behind a receiver class-id guard, miss falls to normal dispatch,
    // never a deopt). **Default-ON since N8** — it is the devirtualization
    // this tree performs, and with it off the shipped configuration performs
    // none at all: `cha` above is ANDed with it, so `=0` takes class-hierarchy
    // analysis away as well. `off_word: "0"` is the one-variable bisect for
    // devirtualization as a whole; see `env_cache::jit_guarded_virtual_inline`
    // for the argument and the soak this flip still owes, and
    // docs/feature-designs/profile-guided-inlining.md for the lowering.
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
    E { group: Group::JIT, token: "inline-tlab-new", on_key: None, off_key: Some("CRATONVM_NO_JIT_INLINE_TLAB_NEW"), off_word: None, since: "2026-07-24" },
    E { group: Group::JIT, token: "inline-tlab-newarray", on_key: None, off_key: Some("CRATONVM_NO_JIT_INLINE_TLAB_NEWARRAY"), off_word: None, since: "2026-09-11" },
    E { group: Group::JIT, token: "intrinsics", on_key: None, off_key: Some("CRATONVM_DISABLE_INTRINSICS"), off_word: None, since: "2026-05-22" },
    E { group: Group::JIT, token: "ir-branchy", on_key: None, off_key: Some("CRATONVM_NO_IR_BRANCHY"), off_word: None, since: "2026-06-18" },
    E { group: Group::JIT, token: "ir-call", on_key: Some("CRATONVM_JIT_IR_CALL"), off_key: None, off_word: Some("0"), since: "2026-06-20" },
    // Default-ON, `"0"` turns it off: the optimizing tier's String access
    // expander (`length`/`isEmpty`/`charAt` lowered to IR nodes rather than
    // dispatched). ON is the honest default even though the emitter is
    // normally unreachable -- `string-intrinsic-pin`, also default-ON, keeps
    // any method with a String-intrinsic site on the single-pass backend -- so
    // a default-OFF spelling would be dead twice over. The A/B that means
    // something is `-string-intrinsic-pin` against
    // `-string-intrinsic-pin,-ir-string-intrinsics`, with the pinned default as
    // the third reading.
    // The attribution arm of `ir_optimize::LoopGuards::permits_hoist`. Default
    // ON, unlike this group's usual convention for a widening, because the arm
    // moves no guard (so it moves no deopt point) and everything it admits
    // clears a permission STRICTER than the unguarded path's. Setting it to `0`
    // restores the blanket refusal `control_token_has_guard` describes — the
    // control to bisect with if a guarded loop starts misbehaving.
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
    // The expander's census, printed on every decision and cumulative, so the
    // last line is the exit total. Cached in a `OnceLock` at the read site:
    // an uncached per-site flag read is how `CRATONVM_DBG_COMPACT_INLINE`
    // came to be 99.1% of all flag reads on a BigDecimal run.
    // A/B opt-out: `-ir-buffer-estimate` restores the legacy code-buffer sizing
    // (`ir_lower.rs`), whose under-estimate floods "code buffer estimate too
    // small" bails. Declared as the OFF half of a default-on knob, so the
    // supported spelling is `CRATONVM_JIT=-ir-buffer-estimate`.
    // Default-**OFF**, unlike its neighbour `ir-reloc-emit`. `ir_lower::
    // linear_scan_enabled` answers `false` for `Err(_)`, so unsetting the key is
    // already the off state and `off_word` stays `None`; writing `"0"` here
    // would work too but would mislabel the row as a default-ON knob, which is
    // the one thing this table is supposed to state unambiguously.
    // Increment 2 of the machine level. `ir-isel-emit` makes the selector's
    // tiles the emitted bytes; `ir-isel-verify` builds the same machine list and
    // checks it against the per-opcode arms byte for byte WITHOUT emitting it.
    // Both default-OFF for the same reason as `ir-isel-shadow` above:
    // `ir_lower::isel_emit_enabled` / `isel_verify_enabled` answer `false` for
    // `Err(_)`, so unsetting the key is already the off state.
    // Declared 2026-08-11 with the pre-push hook that would have caught it.
    // `jit::precise_getstatic_checkcast_enabled` withdraws the RBC.6 admission
    // of `getstatic`/`checkcast` so one binary can be A/B'd against its own
    // pre-change behaviour. Opt-out spelling only, same as its
    // `precise-field-ops` neighbour, so the token is stated positively and
    // enabling it means removing the key. The read site already goes through
    // `runtime_var_os`, which serves a DECLARED name from the latched snapshot
    // and only falls through to a live `getenv` for an undeclared one — so
    // this row is the whole fix.
    // `invokedynamic` (0xba). Bookkeeping, not a new lowering: the bridged arm
    // already runs `emit_post_invoke_exception_check` and every other arm
    // deopts unconditionally before the call. It was keeping
    // `MVMap.flushAppendBuffer` -- 15.2% of CPU on a contended H2 workload --
    // permanently interpreted.
    // Array loads and PRIMITIVE array stores. A real lowering change, not
    // bookkeeping: the AIOOBE pad and the array null-check stub both published
    // nothing, and now route to deopt-stub reasons 11 and 10. `aastore` is
    // excluded -- its ZGC-barrier fallback arm still does not publish.
    // Opt-in. The GP register file landed beside the FP one on 2026-09-02, but
    // the flip still wants a wall-clock measurement -- see
    // `ir_lower::linear_scan_enabled`. `since` stays 2026-08-01: the flag is the
    // same flag, and this column dates the KNOB, not its capability.
    // DEFAULT-ON since round 9 wave 4 (`runtime_flag_default_on`; 92 soak runs
    // with the bump engaged, outputs identical to HotSpot), so `0` is the kill
    // switch. Allocation-bearing methods reach the optimizing tier through the
    // entry counter (default-on since wave 3), which is what makes it live.
    // JIT round 17 wave 3, lane irlower (OD16-1 = OI-5, token-edge half):
    // `regalloc::build_live_model` stops counting a memory-token edge (a value
    // token's next memory op, a memory phi's incoming edge) as a read. Opt-in
    // until the `[ir-ls] peak_live` / spill A/B.
    // Interpreter round i2 wave 64 (lane L2): the same token-edge rule for
    // `ir_lower::plan_slots`' frame-home ranges (implies the line above and
    // the token-read guard below). Opt-in until the frame-word census and the
    // guard's zero-refusal run.
    // Interpreter round i2 wave 64 (lane L2): refuse an optimizing compile in
    // which a lowering arm reads an operand through a memory-token slot.
    // Compile-time check only. Opt-in.
    // JIT round 19 proposal wave 11 (lane lv11, TOK10-2): in a build with
    // debug_assertions the token-read guard is on unless the guard switch says
    // 0. No effect in release builds. Opt-in.
    // JIT round 19 proposal wave 11 (lane lv11, TOK10-3): the guard's identity
    // exemption (pinned / snapshot-named) also needs the home's range to cover
    // the reader's position. Compile-time check only. Opt-in.
    // JIT round 19 proposal wave 12 (lane lv12, LV11-4): a by-bci deopt point
    // a resume can arrive at must not name a shareable home past its
    // colouring range. Compile-time check only. Opt-in.
    // Interpreter round i2 wave 65 (lane L2): `ir_lower::plan_slots` and
    // `regalloc::build_live_model` run ONE liveness computation
    // (`regalloc::liveness_core`) instead of two hand-kept copies. A pure
    // refactor; opt-in until the host confirms byte-identical frames.
    // Interpreter round i2 wave 65 (lane L2): the slot plan applies the φ
    // edge-interference rule too, so `ranges_agree` can hold in a method with
    // a φ (implies the line above for the slot plan). Opt-in.
    // i64-L2 (2026-10-11): default ON (frame-neutral, opens the split gate); `0` restores the plan-only phi ranges.
    // JIT round 19 proposal wave 11 (lane lv11, LV10-6 / i64-L2): with the
    // split gate open, a GP split value that visits a register the
    // loop-carried reservation would hand out is demoted to its home instead.
    // Only removes split values. Opt-in.
    // JIT round 19 proposal wave 12 (lane lv12, LV11-2): the priced form of
    // the line above -- a colliding split value yields only when its register
    // reads do not outweigh the carried values it displaces. Opt-in.
    // JIT round 19 proposal wave 12 (lane lv12, LV11-3): the split arm refuses
    // a placement whose register segments serve no more reads than the loads
    // that fill them. Opt-in.
    // JIT round 19 proposal wave 12 (lane lv12, LV11-1): the scan pre-pin
    // lists a Ref as a certain refusal under LS_SPLITS too (reference
    // residency off). Opt-in.
    // Interpreter round i2 wave 66 (lane L2): the optimizing lowering entry
    // runs the liveness core once and hands it to both the slot plan and the
    // allocator's live model, when the two would ask the same question (in
    // practice with the line above on). Byte-identical answers; opt-in.
    // Interpreter round i2 wave 67 (lane L2): the liveness core numbers the
    // wanted values densely and sizes its bit sets by them, not by node
    // count. A pure refactor (byte-identical answer); opt-in. The second
    // line makes the work budget count values too (a behaviour change; it
    // implies the first). Opt-in.
    // Interpreter round i2 wave 69 (lane L2): the liveness core's backward
    // fixed point revisits only blocks whose successors' live-in changed (a
    // dirty-block worklist kept in the round-robin pass order). A pure
    // refactor (byte-identical answer, converged or cut off); opt-in.
    // Interpreter round i2 wave 70 (lane L2): the legacy liveness copies the
    // default JIT runs (the live model's, the slot plan's, the value-read and
    // chain-read ranges) use the same dirty-block driver. Builds on the line
    // above but is independent of it. A pure refactor (byte-identical answer,
    // converged or cut off); opt-in.
    // Interpreter round i3 wave 71 (lane L2): a block-count gate for the two
    // worklist lines above (`=<n>`: the worklist runs only on schedules of at
    // least n blocks; unset, 0 or junk is no gate), and the worklist reading
    // each block's own predecessor list instead of building a predecessor
    // index per fixed point. Both pure refactors (byte-identical answer);
    // opt-in.
    // Interpreter round i3 wave 71 (lane L2): where a worklist line above runs
    // the worklist, its passes visit the blocks in CFG postorder instead of
    // reverse layout index. The converged answer is unchanged (the least
    // fixed point); the pass count, and so a run cut off by the iteration
    // cap, can differ. Opt-in.
    // JIT round 19 proposal wave 10 (lane lv10), from the interpreter round's
    // i71-L2 / i70-L2 pages. Each a pure refactor (byte-identical answers,
    // pinned by the `lv10_*` unit tests); opt-in until the host measures them.
    // ONE_DRIVER: the four legacy liveness copies run their round robin
    // through the shared driver instead of their hand-written loops (i71-L2,
    // retire the four loops). SHARED_SCRATCH: one liveness scratch per
    // compile -- the position model and φ edge table built once, the bit sets
    // pooled (i71-L2, one scratch). DEAD_REF_LIVENESS_WORKLIST: the dead-ref
    // clear planner's three fixed points through the closure-transfer
    // dirty-block driver, with in-place compares (i70-L2).
    // Round 16, lane osrdoor: an OSR entry stub owes no seed to a memory token,
    // or to a primitive the dataflow says is not live into the entry block.
    // Default OFF (opt-in) until an A/B measures them.
    // JIT round 21 wave 17, lane osrrefs (OD16-2;
    // docs/feature-designs/jit-osr-exit-and-recompile.md):
    // the reference half of the dataflow excuse; the stub nulls the value.
    // Round 16 wave 14, lane osrir (MON3-2): a plain `getstatic` answered by an
    // earlier read of the same static nothing can have written since. Default
    // OFF (opt-in) until an A/B measures it.
    // t1 wave 7, lane NBC (STATICFWD21-3 in docs/feature-designs/jit-ir-optimizations.md): with ir-static-store-forward on, a reference static's stored value forwards too (lazy holders). Default OFF, opt-in "1".
    // t1 wave 7, lane NBC (STATICFWD21-2 in docs/feature-designs/jit-ir-optimizations.md): the IR builder's static-load memo also records and answers plain getstatics inside spliced bodies, depth-scoped like ir-field-memo-in-splices. Default OFF, opt-in "1".
    // 2026-09-16. `ir-osr-entry` emits the stubs; this one says emit them even
    // into a body `try_osr` cannot enter (one that is not
    // `ir_osr_sentinel_free`, i.e. any method containing a call). Default OFF —
    // those stubs are unreachable — and it exists so the skip can be A/B'd on
    // one binary. Opt-IN, hence `off_key`/`off_word` unset.
    // Round 11 wave 10 (lane irexc): IR reason-9 rethrow pads admit RBC.6 methods
    // (javac synchronized blocks) to the IR tier. OPT-IN until soaked.
    // 2026-09-16. Carve JIT code buffers out of a shared executable arena
    // instead of one `VirtualAlloc`/`mmap` per method (64 KiB of address space
    // per ~5 KB body on Windows). Default OFF: unmeasured, and the
    // address-reuse ordering it imposes on `CompiledMethod::drop` is stated
    // rather than enforced. Opt-in.
    // 2026-09-16. Shadow-compile every method the optimizing tier takes with
    // the single-pass backend too, and report any capability the optimizing
    // body LOST (VEX, REP-string, the sieve SWAR constants). A diagnostic:
    // with it on, every IR-tier compile pays a full single-pass compile it
    // throws away, which is fine for a corpus run and unacceptable in
    // production. Opt-in.
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
    // 2026-09-16. Apply `escape_analysis`'s lock-coarsening plans, which have
    // been computed on every EA run since the pass landed and never applied.
    // Opt-in: coarsening holds a lock for longer, which is a throughput trade
    // nobody has measured.
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
    // R2b. Splice a callee whose body BRANCHES. The builder re-verifies each
    // relocated body on its own and rebases the merge targets and loop headers
    // that come out, which is the analysis the caller's `verified_code` cannot
    // supply for a region past its `code_len`.
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
    // Splice a callee whose body carries a `checkcast` or an `instanceof`.
    // `IrBuilder` has had both arms since cov-05; the splice scanner refused
    // the shape because nothing resolved the callee's targets or rebased the
    // rows. An unresolved target refuses the CALLEE rather than being dropped,
    // because a missing row bails the whole method.
    // Default ON. A splice must not leave behind a call worse than the one it
    // replaced: a surviving statically-bound call with no `direct_entry` has
    // no inline cache to fall back on and lowers to a blind name resolution.
    // Setting this to 0 re-admits that trade, for measuring it.
    // Splice a callee whose body contains a `checkcast` or an `instanceof`.
    // The same rebase as the two above, and the refusal that page named as
    // the next one to take: every typed read out of an untyped container is a
    // `checkcast`. Opt-IN and default OFF -- the plumbing is here, the soak is
    // not, and a spliced type check is the first spliced site that can THROW
    // on a caller-produced value. Read site accepts `1`/`true`/`on`/`yes` and
    // nothing else, so there is no off-word: removing the key is the way back.
    // R1. Memoize the ACCEPTED optimizing OSR artifact, not only the refusals.
    // Without it one run recompiled the same method 502 times.
    // R3, R4 and R6: opt-IN, default OFF pending their soaks. Each read site
    // accepts `1`/`true`/`on`/`yes` and nothing else, so there is no off-word
    // to state -- removing the key is the way back.
    // JIT round 25 (item 1b): block-local values that cross no call take a
    // caller-saved GPR (R8-R11) instead of the callee-saved file. Opt-in.
    // Round 25 wave 2: under the row above, a value with no register keeps a
    // caller-saved one for its runs of reads between two opaque nodes. Opt-in.
    // The single-pass twin of the row above, and the arm that unrolls: the
    // `osr/sp` body of a four-field loop carries eight of these guards.
    // Default ON and UNSOUND when clear: it restores a baked compact body
    // offset that survives a layout replacement. It exists so the guard's
    // price is a number from one binary, not an argument.
    // Diagnosis lever, default OFF: put the layout-replacement epoch back in
    // `.data` so its LOCATION is A/B-able independently of the encoding it
    // shipped with. Must not be combined with `code-near-globals`, whose
    // placement anchor is this counter.
    // Default ON: the layout-epoch counter takes a cell from the JIT code
    // cache's own allocator, so the short guard encoding is in reach by
    // construction. `=0` puts it back on the Rust heap, which is the arm the
    // 2026-09-11 A/B was taken against.
    // Use the existing code-adjacent data arena as the placement anchor.
    // Diagnosis lever, value-taking: a comma-separated list of conservative
    // root-band CLASSES to skip (`operand-spill`,
    // `outgoing-args-or-deopt-regs`, `safepoint-gpr-spill-image`). Absent
    // means skip nothing, which is the only setting that is safe -- dropping a
    // root frees what it named, and the lever exists to MEASURE the ceiling a
    // real fix would reach, not to be run. Landed 2026-09-09 with the G1
    // pinned-regions work and undeclared; see `band_skip_classes`.
    // Round 11 wave 9, tier proposal 26: the background OSR task builds the
    // optimizing OSR body too, and the mutator's background arm enters a
    // cached one. DEFAULT-ON; `0` restores the pre-wave-9 routing, where the
    // optimizing route was asked only after a single-pass OSR body published.
    // Round 11 wave 10 (lane irexc, tier proposal 27): optimizing OSR bodies whose
    // call-exception exits are resumable are admitted. DEFAULT-ON since wave 11
    // (soaked: R11 battery, legacy probes, IR census); `0` is the kill switch.
    // Round 11 wave 12 (lane irexc, tier proposal 27 phase 3): optimizing OSR
    // bodies whose guard exits are exact or value-equivalent are admitted and
    // their guard exits resumed without a plan. DEFAULT-ON since wave 14
    // (soaked in waves 12 and 13: R11 battery, legacy probes, IR census);
    // `0` is the kill switch.
    // Interpreter round i5 wave 101 (lane L2): the optimizing OSR route leaves
    // a compile declined at a constant-pool resolver miss unmemoised, at most
    // four times per method (performance only; default OFF;
    // `VmConfig::jit_osr_optimizing_resolver_miss_retry`).
    // Interpreter round i6 wave 116, lane L2 (performance, opt-in): a background compile that
    // declined at a constant-pool resolver miss is parked by the tiered manager until a class is
    // defined, not charged to the method's three compile retries (`tiered::note_compile_waiting_on_class`).
    // JIT round 16 wave 13 (lane monitor3): an optimizing OSR body's exact /
    // value-equivalent guard exits may name the frame's own taken locks (a guard
    // inside a `synchronized` block), and the door admits such guards inside
    // javac's catch-all range when no entry is handler-reached. Opt-in until
    // the Linux arms (R16MonitorCrowd, R16Monitor3GuardExit).
    // Round 16, lane osrdoor: a burst of caught-exception exits from an optimizing OSR body
    // hands the loop to the single-pass OSR body (local handler stubs). Default ON.
    // Round 11 wave 14 (lane osrmerge, r11w11-irexc-osr-entry-as-a-real-predecessor):
    // loop headers get an opaque OSR entry merge; the optimizing OSR door then
    // admits handler-reached headers and speculative guard exits. OPT-IN.
    // The remaining three of the 2026-09-09 C2-cost switches. The rest of that
    // work is declared in the block above; these are the ones whose read sites
    // arrived on a different branch.
    //
    // A spliced body with more than one `return`. Read by BOTH halves -- the
    // scanner in `jit_bridge` and `IrBuilder::splice_return`. Was off because
    // it measured neutral (the OSR containment it once cost came off on
    // 2026-09-09; see `ir_splice_multi_return_enabled`); default ON since
    // round 16 through the chain arm (`ir-chain-arm`, measured: CratonBench
    // hashmap 645->564 ms, strings 3448->2632 ms), so unset means ON and
    // `"0"` is the kill switch. No longer implied by `ir-recursive-inline`
    // (round 11 wave 11); the builder recognises a multi-return body without it.
    E { group: Group::JIT, token: "ir-splice-multi-return", on_key: Some("CRATONVM_JIT_IR_SPLICE_MULTI_RETURN"), off_key: None, off_word: Some("0"), since: "2026-09-09" },
    // Netty exhaustive-loop round (2026-10-14): the optimizing OSR tier for
    // `HttpHeaderValidationUtilTest` / `HttpResponseStatusTest`. Default ON
    // with `0` as the kill switch, except `ir-home-reload-forward` (opt-in).
    // JIT round 25 item 1a (default ON since round 26, R26-FLIP): no definition-time home store for an
    // int/long value no reachable deopt names and whose one reader is lowered
    // right behind it (`Lowerer::plan_tail_carries`), and an `If`-fused
    // compare named only by unreachable frame states. The successor of
    // `ir-home-reload-forward`, which kept the store.
    E { group: Group::JIT, token: "ir-elide-unnamed-homes", on_key: Some("CRATONVM_JIT_IR_ELIDE_UNNAMED_HOMES"), off_key: None, off_word: Some("0"), since: "2026-10-24" },
    // Wave 3 of the same item (default ON since round 26, needs `ir-elide-unnamed-homes`): a
    // register-resident value named at calls and polls is stored into its home
    // right before them instead of at its definition.
    E { group: Group::JIT, token: "ir-elide-homes-spill-at-inspection", on_key: Some("CRATONVM_JIT_IR_ELIDE_HOMES_SPILL_AT_INSPECTION"), off_key: None, off_word: Some("0"), since: "2026-10-24" },
    // JIT round 25 wave 6, lane W6-B (opt-in, needs both switches above, which
    // are default ON since round 26): loop
    // headers are liveness-checked instead of hard-blocked, int/long loop phis
    // may be site-named, and a back-edge poll's inspection spills run in its
    // slow path only.
    E { group: Group::JIT, token: "ir-site-named-loop-values", on_key: Some("CRATONVM_JIT_IR_SITE_NAMED_LOOP_VALUES"), off_key: None, off_word: Some("0"), since: "2026-10-24" },
    // JIT round 25 wave 7, lane W7-A (opt-in): the IR register-allocator
    // increment. Implies the three switches above (each unless its own key is
    // set; since round 26 the first two are default ON anyway, so it adds
    // `ir-site-named-loop-values`), admits single-use int/long values the carry
    // window cannot reach to the residency plan, and site-names coalesced
    // back-edge phi sources.
    // JIT round 25 wave 13, lane W13-2 (default ON, `=0` kills): an optimizing
    // body keeps the VM-context hidden argument whenever the method's
    // single-pass body takes it, so a C2 supersede can forward the C1 body's
    // baked callers (the ABI refusal c30 named on `<init>` and `indexOf`).
    // JIT round 21, lane lambdacap (`3bd4214f82`, host verified on w3l),
    // re-landed in t1 wave 8 after merge `624fae0b49` dropped it: an
    // optimizing body that built a capturing lambda's proxy keeps the
    // VM-context entry ABI after escape analysis removed the proxy, so the
    // supersede forward and late-static cells can reach it
    // (`jit::ir_lower::ir_lambda_proxy_keeps_context_enabled`; a backstop to
    // the row above). Default ON; `=0` restores the context-free body.
    // t1 wave 8, lane LAMBDACAP (page
    // `t1-lambdacap-zgc-aaload-context-abi-mismatch-FIXED-20261027.md`): the
    // optimizing tier reserves the VM context for a reference-array load only
    // where it calls `jit_aaload` (the single-pass scan's predicate), so
    // under ZGC colored stores the C1 and C2 bodies agree on the entry ABI
    // (`jit::ir_lower::ir_zgc_aaload_context_exact_enabled`). Default ON;
    // `=0` restores the broad answer.
    // t1 wave 9, lane LCAP (page
    // `t1-lcap-context-abi-floor-misses-the-driver-context-reasons-20261026.md`):
    // the W10-2b context floor also counts a string/class `ldc`, which gives
    // the single-pass body the context form after the bytecode scan
    // (`jit::r25_w10_2_steady_c1::ldc_context_abi_floor`). Default OFF; `=1`
    // enables; needs `ir-context-abi-of-baseline` on.
    // t1 wave 10, lane LCAP2 (pages
    // `t1-lcap-context-abi-floor-misses-the-driver-context-reasons-20261026.md`,
    // `t1-lcap-implicit-exception-edges-give-c2-a-context-c1-lacks-20261026.md`):
    // the optimizing door hands the compile the published body's entry ABI;
    // context-taking, it widens the W10-2b floor (every single-pass reason at
    // once); context-free, the build plans no implicit exception edges
    // (`jit::r25_w10_2_steady_c1::installed_context_abi_floor`). Default OFF;
    // `=1` enables.
    // JIT round 21, lane lambda2 (`bf47329092`; TLOCAL21-1 / LAMBDATRAP21-3,
    // page `jr21-tlocal-capture-new-kernel-FIXED-20261014.md`; host verified
    // with lambda3 / lambdacap, capture-create-apply 15 vs 363 ns), re-landed
    // in t1 wave 8 (lane LAMBDA2) after merge `624fae0b49` dropped it: the
    // optimizing tier builds an `invokeinterface` SAM call whose receiver is
    // provably the value of the method's own lambda `invokedynamic` as an
    // `invokestatic` of the lambda's static impl, captures first
    // (`jit::ir_lambda_impl_plan`, vm `lambda_proxy_static_impl_call`).
    // Default ON; `=0` keeps the interface call.
    // JIT round 21, lane lambda3 (`cf680d7497`), re-landed in t1 wave 8 (lane
    // LAMBDA2): under `ir-lambda-impl-call`, a CAPTURING lambda's SAM call
    // becomes its impl call only where the impl splices; a build refused with
    // a lambda plan in force is retried without it (the unspliced route lost
    // 3.5x: 1451 vs 421 ns). Default ON; `=0` restores lane lambda2's plan.
    // JIT round 21, lane lambda3 (`88fd16fa0a`, flipped ON by `30e2e2a93f`
    // after lambdacap's context fix, host verified on w3l: capture-create-apply
    // 15 vs 363 ns), re-landed in t1 wave 8 after merge `624fae0b49` dropped
    // it: under `ir-lambda-impl-call`, plan CAPTURING lambda sites too.
    // Default ON; `=0` keeps capturing sites on the interface call.
    // JIT round 25 wave 11, lane W11-2 (default ON since 2026-10-27): a spliced body's surviving
    // `System.arraycopy` counts as bound for the unbindable-call rule (it is
    // bound after the build), so `Arrays.copyOfRange` splices into its callers.
    // JIT round 22 (lane memory, MEM20-8 residual): a spliced body's Reference.reachabilityFence(Object) row (built as an Op::KeepAlive, no call) is no unbindable call for the splice planner, so ByteBuffer accessors ending in a fence splice into their callers. Default ON, kill "0".
    // JIT round 23 close (the zki objarr3 / volatile-store rows of the r21 lean-allocation page): perf, default ON, kill
    // "0". A loop that writes memory still reads a trusted static final reference field once, in its pre-header
    // (jit `ir_optimize::loop_static_hoist::trusted_final_static_reads`); also asks the VM's trusted-final feed.
    // JIT round 16 (lane strings). STR16-1: the inline `String.hashCode()`
    // writes a non-zero computed hash back into a compact receiver's `hash`
    // field, as the JDK body does (default ON, `=0` kills it). STR16-3: the
    // inline `StringBuilder.append(int)` serves negative values too (opt-in).
    // JIT round 17 wave 1 (lane strings, STR16-P1): `StringBuilder.toString()`
    // of a LATIN1 compact builder built inline (fresh `byte[count]` + `String`
    // in one TLAB bump); everything else declines to the ordinary call
    // (opt-in). Round 17 wave 2 (lane sbzgc): emitted under ZGC (call-free
    // start-bit announce of both objects) and G1 (same-region check) too.
    // JIT round 17 wave 2 (lane sbzgc): the inline `toString()` also serves a
    // UTF16 builder holding a char above 0xFF (`byte[2*count]`, coder UTF16);
    // needs `sb-tostring-inline` (opt-in).
    // JIT round 17 wave 6 (lane sbzgc2, SBZGC17-2): the UTF16 arm compresses a
    // `maybeLatin1` builder with no char above 0xFF to a LATIN1 String inline
    // (the JDK constructor's COMPACT_STRINGS answer) instead of declining;
    // needs `sb-tostring-utf16` (opt-in).
    // In-process `javac` residual (2026-10-27): a compiled `super.clone()` that
    // reaches `Object.clone` calls a thin helper (a TLAB bump and the heap's
    // one-validation copy for a plain `Cloneable` instance) instead of the
    // inline-cache door into the native. Default ON; `=0` keeps the door.
    // Let a REFERENCE be register-resident, with the cached copy invalidated
    // at every point a collector could have run. Off because it does not pay,
    // not because it is unsafe.
    // The two halves of `ir-ref-residency` cost different things: crossing a
    // safepoint admits the shapes that matter but RESERVES a register the
    // invalidation then makes unreadable for most of the range. Separated so
    // both are timeable from one binary. Only meaningful with the above on.
    // Opt-IN: `ir_drop_unreachable_homes_enabled` is a `runtime_flag_on` read,
    // so unsetting the key IS the off state and `off_word` stays `None` (round
    // 9 wave 2; it said `Some("0")`, which rendered the row as default-on).
    // The 2026-09-10 instruction-count residue: the scheduler pairing that
    // feeds the carry, the second carry slot it fills, the fused compare that
    // reads its operands where they are, and the `LEA` that adds a constant
    // without routing through the accumulator. All four are default-ON levers
    // whose `0` is both the kill switch and the A/B arm.
    // Round 9 wave 2: put the `MFENCE` back after a volatile LOAD. Opt-IN —
    // `jit::runtime_lowering::volatile_load_fence_enabled` is a
    // `runtime_flag_on` read — so `off_word` stays `None`. The fence is gone by
    // default because an x86-TSO load is already an acquire and the JMM's
    // StoreLoad is the volatile STORE's fence; this is the one-release kill
    // switch for that memory-model change, on every tier that consults it.
    // Round 9 wave 2: zero every reference colour's frame word in the IR
    // prologue (and OSR entry stubs) so a safepoint map never publishes a word
    // this activation did not write. DEFAULT-ON (`runtime_flag_default_on`),
    // so a KILL SWITCH and `off_word` is exactly `"0"`.
    // Round 9 wave 2, all default OFF (`runtime_flag_on`), so no `off_word`.
    // IR escape analysis: answer a load in another block from the allocation
    // block's last store when every store sits in the allocation's own block.
    // Single-pass inline primitive `putfield` (no helper call on the fast path).
    // Default ON since round 9 wave 4's integration (measured, see
    // `x64::op_field::inline_primitive_putfield_enabled`); `0` is the kill switch.
    // IR optimizer: count only snapshots a deopt can consult as uses in DCE and
    // write-only store elimination.
    // IR optimizer: build volatile instance-field getfield/putfield as ordered
    // Load/Store nodes instead of refusing the method (stores also need the
    // ir_lower fence, ir::IR_LOWER_FENCES_VOLATILE_STORES).
    // Default ON since round 9 wave 7 (measured by irsnap7, see
    // `ir::ir_volatile_fields_enabled`); `0` is the kill switch.
    // Single-pass: place speculative BCE guards and the aaload / int-arith
    // hoists before a rotated loop's entry `goto`.
    // Default ON since round 9 wave 5 (measured, see
    // `x64::escape_analysis::rotated_preheader_enabled`); `0` is the kill switch.
    // Round 9 wave 3. DEFAULT-ON kill switches (`0` restores the old behaviour):
    // direct binds inside a compile-time recursion cycle once the callee
    // publishes; the IR tier's constant-divisor strength reduction; the
    // profile-guarded inline String hashCode for `Object.hashCode()` sites;
    // JIT round 20 wave 11, lane pLate (book docs/feature-designs/jit-ir-optimizations.md P1 = w18-L4 W18L4-3): every invokestatic the direct bind could not give an entry (callee compiles after the caller) gets the closing edge's JitCycleEdgeCell, filled by JitCache::put when the callee publishes. Default ON, kill "0".
    // Round 9 wave 4. Default-ON kill switch: compiled `invokespecial` sites use
    // the per-site native cache (e.g. an exception subclass's `super(msg)`).
    // Default ON since round 9 wave 5 (guarded by the duplicate-class-name
    // latch in `types`); `0` is the kill switch.
    // Round 9 wave 5: single-pass operand folding (ALU ops read operands in
    // place, `x = x OP y` stores fold). On by default; this is its kill switch.
    E { group: Group::JIT, token: "operand-fold", on_key: None, off_key: Some("CRATONVM_JIT_NO_OPERAND_FOLD"), off_word: None, since: "2026-09-18" },
    // Round 11: bounded self-recursive IR inlining, default ON (`CratonBench
    // fib`; dev flipped it independently for binary-trees on 2026-09-24);
    // `0` is the kill switch. A multi-return body is admitted under it only
    // as a copy of the method being compiled. Wave 11 reverted the flip
    // because the lowerer refused every spliced body; wave 12 (lane fib) fixed
    // the refusal in `ir_lower::Lowerer::phi_home_droppable`.
    // Round 11 wave 15 (lane fib): a second self-copy admission for bodies
    // whose replay repeats nothing visible (getfield / new / self call /
    // receiver-only constructor): binary-trees' `itemCheck`, `bottomUpTree`.
    // Default ON; `0` is the kill switch.
    // Maximum self-recursive copies along an inline chain (a number; unset = 3 for a method of at most 35 bytecode bytes, else the built-in bound).
    // Round 11 wave 15 (lane fib): a reference-free body's direct self-call
    // stores its safepoint id on the cold paths only. Default ON; `0` kills it.
    // Interpreter round i6 wave 117, lane L2 (performance, opt-in): a body of a method marked by
    // `JitCache::mark_reentry_only` (a JDK method behind an Intrinsic) publishes re-entry-only:
    // `JitCache::get` never answers it, only `interpreter::execute`'s own lookup does.
    // JIT r25 wave 2, lane regex rows: perf, opt-in. The deopt controller's eager re-queue after a trap evicted an
    // OPTIMIZING body asks for C2 again (the gated C2 retry door) instead of C1, so a leaf reached only from compiled
    // code does not stay on its single-pass body (jit `tiered::trap_recompile_keeps_c2_enabled`).
    // Interpreter round i6 wave 117, lane L3 (performance, opt-in; stage 3 of
    // proposal i14-L3): a scoped redefinition does not advance the VM's
    // code-state epoch, so unrelated OSR memo entries, loop budgets and
    // verdicts survive it.
    // Round 16 wave 13 lane wmte (MH16-1): the table also shares a type naming a
    // class a built-in loader is known to have defined. Opt-in (default OFF,
    // `runtime_flag_on`) until the orchestrator's A/B.
    E { group: Group::JIT, token: "mh-bound-type-table-app-classes", on_key: Some("CRATONVM_MH_BOUND_TYPE_TABLE_APP_CLASSES"), off_key: None, off_word: Some("0"), since: "2026-10-02" },
    // Round 16 wave 15 lane mhapp: a `MethodType` built from a descriptor is the
    // JDK's interned instance (`MethodType.methodType(Class, Class[], boolean)`),
    // so two equal types are one object as on HotSpot. A correctness fix,
    // default ON (`runtime_flag_default_on`), so a KILL SWITCH, `"0"`.
    E { group: Group::JIT, token: "mh-intern-descriptor-types", on_key: Some("CRATONVM_MH_INTERN_DESCRIPTOR_TYPES"), off_key: None, off_word: Some("0"), since: "2026-10-02" },
    // Round 16 wave 17 lane mhapp: a handle's `type` and `bindTo`'s bound type
    // resolve their names through the looked-up class's loader, so a user
    // loader's same-named class is not the application loader's. A correctness
    // fix, default ON (`runtime_flag_default_on`), so a KILL SWITCH, `"0"`.
    E { group: Group::JIT, token: "mh-loader-scoped-types", on_key: Some("CRATONVM_MH_LOADER_SCOPED_TYPES"), off_key: None, off_word: Some("0"), since: "2026-10-02" },
    // Round 16 wave 10 lane mhfloor (MH16-8): a constant-bound site enters its
    // target's cached compiled entry without re-walking the dispatch ladder.
    // Opt-in (default OFF) until the orchestrator's A/B.
    // Round 16 wave 11 lane mhbind: MHF-1, a bound `invokeExact` site also takes a
    // handle of the bound one's shape at another address (fresh `bindTo` results
    // of one source), and MHF-2, `bindTo`'s per-source fast mint. Both opt-in
    // (default OFF) until the orchestrator's A/B.
    // Round 16 wave 15 lane mh6 (MHB-1): a compiled site whose handle converts
    // Integer/Number/Object arguments into int parameters, or an int return into
    // such a reference, binds too; the helper unboxes and boxes around the call.
    // Opt-in (default OFF) until the orchestrator's A/B.
    // JIT round 19 wave 1, lane mhiface (MHRESID18-4): the optimizing tier compiles an
    // invokeExact/invoke on a static final interface findVirtual handle as a guarded
    // invokeinterface of its member (instanceof guard, deopt to the handle's door), and
    // an interface handle's bound helper site enters its target's cached compiled entry.
    // Both perf, default ON since JIT round 20 (480 -> 50 ns and 483 -> 432 ns on
    // R17Mh4Virtual exact-interface), `=0` off.
    // Round 16 wave 17 lane mh7 (MH16-8 at compile time): the optimizing tier
    // compiles a MethodHandle.invokeExact/invoke whose receiver is a static final
    // field holding an exact direct static handle as an invokestatic of its member.
    // Default ON since the w17a A/B (`=0` turns it off; comment corrected r17 w2).
    // Round 17 wave 1 lane mhconst: MH7-1, an optimizing compile that found no
    // admitted handle in a static final field is recompiled once the constant bind
    // admits that field's handle; MH7-2, a field-keyed row of the admission that
    // survives a collection. Both opt-in (default OFF) until the orchestrator's A/B.
    // The loader check (both loaders see the descriptor's classes as one) is a
    // correctness fix, default ON, `=0` kill switch.
    // Round 17 wave 2 lane mh2: MH7-3, a cold fill that finds an admitted handle
    // rewritten in place drops its memo and field rows (check, default ON, `=0`
    // kill switch); MHCONST17-1, the interpreter's invokeExact/invoke door fills
    // the constant-bind memo too (opt-in, default OFF until the A/B).
    // Round 17 wave 2 lane mh3: a body compiled from a constant handle records
    // its dependency on the handle and is withdrawn when the handle is found
    // rewritten in place, and a same-epoch field row whose field now holds an
    // unadmitted handle is refused (correctness, default ON, `=0` kill switch);
    // MHCONST17-3, package-private and private-nestmate static members compile
    // as plain calls too (opt-in, default OFF until the A/B).
    // JIT round 17 wave 3 (lane mh4): MHCONST17-2, an unbound virtual constant
    // handle compiles as a guarded invokevirtual of its member (opt-in, default
    // OFF until the A/B); a field row of an older collection epoch answers only
    // when its weak witness still names the field's value (correctness, default
    // ON, `=0` kill switch).
    // Default-ON A/B lever: `x64::gated_ref_store_enabled` reads `0`. Its
    // predecessor `CRATONVM_NO_JIT_INLINE_PUTFIELD` measured exactly zero under
    // the default collector, because the path it disabled was already
    // unreachable there.
    // gen r4w4/cards4: the inline GENERATIONAL post barrier (card check before
    // and after each compiled reference store, collector call only for a clean
    // card). Default ON since the gcg triage (tg1), `=0` restores the helper; `x64::objects::inline_card_mark_available`.
    E { group: Group::JIT, token: "inline-card-mark", on_key: Some("CRATONVM_JIT_INLINE_CARD_MARK"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // gcj j2/jt2 -- begin
    // gcj j2/jt2 (default ON since 2026-10-25, `=0` kills, Generational only; WARNING gcj-j2-residuals-flip-20261025): both tiers' inline card barrier drops its PRE card check (HotSpot's
    // store-then-mark shape); the BUG-03 take-over records each frozen peer's registers and, at resume, dirties the card
    // of every one in the old generation (`CardTable::complete_frozen_register_marks`); the next card scan reads those
    // cards' objects whole, so it composes with CRATONVM_GC_PRECISE_ARRAY_HEADER_CARD. `x64::objects::jit_post_only_card_mark_enabled`.
    // gcj j2/jt2 (default ON since 2026-10-25, `=0` kills, Generational only): the inline card check forms the card address as HotSpot's
    // `(addr >> 9) + byte_map_base` (one load) on a card-aligned table. `x64::objects::jit_card_byte_map_base_enabled`.
    // gcj j2/jt2 (default ON since 2026-10-25, `=0` kills, Generational only): the optimizing tier's gated reference putfield into an object the same
    // block just allocated (no call / poll / allocation between) emits no SATB pre gate (its old value is null).
    // `ir_lower::gen_fresh_store_no_pre_gate_for`.
    // gcj j2/jt2 -- end
    // t1 lane D (opt-in, Generational only): the card map covers the hull of the old generation's and both young
    // semispaces' reservations (summary windowed from a hull summary map, the old high-water mark published as the scan bound before each card scan),
    // so the JIT's card mark is one hull compare and an unconditional byte store. `gen_heap::whole_heap_card_table`.
    // t1 wave 8 lane CARDTBL (default ON, `=0` kills; inert unless CRATONVM_GC_WHOLE_HEAP_CARDS is on): the young semispaces reserve
    // up to the young maximum so young growth stays in place, inside the hull. `gen_heap::gen_whole_heap_young_headroom_enabled`.
    // t1 lane D (default ON, `=0` kills; inert unless CRATONVM_GC_WHOLE_HEAP_CARDS built a hull map): both tiers'
    // inline card mark uses the hull form. `x64::objects::jit_whole_heap_card_mark_enabled`.
    // t1 lane R (opt-in "1"; single-pass, baked hull, read-first): the whole-heap card mark forms its index in 64 bits
    // and reads the card as [map + idx] (SIB) -- no ADD before the load, exact range test. `x64::objects::jit_whole_heap_card_mark_sib_enabled`.
    // t1 lane R (diagnostic, value 0..=31): NOP bytes emitted before each single-pass whole-heap card mark, the A/B lever for
    // the code-alignment explanation of the single-pass regression. `x64::objects::dbg_whole_heap_card_mark_pad`.
    // t1 lane D (opt-in, any collector): the single-pass reference store's SATB pre-gate compare is one RIP-relative
    // `CMP BYTE [rip+d32], 0` when the gate byte is in reach (t1 SATB1P: G1 putfield skips too). `x64::objects::sp_satb_gate_once_enabled`.
    // The same gates on the OPTIMIZING tier, which had no arm to ask with until
    // 2026-09-02 and so reported neither gated nor declined. A separate lever
    // from the one above on purpose: one switch covering both emitters could not
    // separate "the plan is wrong" from "this emitter is wrong".
    // Diagnostic-only: the DYNAMIC split between the gated arm's inline path
    // and its helper fallback. The compile-time census counts emitted
    // sequences, which is not the same fact.
    // The single-pass twin. Two switches rather than one so a run can trace one
    // tier at a time when both arms are engaged.
    // Seeds `this` non-null at method entry. Kept separate from
    // `receiver-null-elim` below because the blast radii differ: this one
    // widens a fact THREE consumers already read (array null-check elision,
    // ifnull/ifnonnull branch elision, and the getfield receiver guard), while
    // that one only adds the third consumer. One switch for both would have
    // made them indistinguishable in a bisect.
    // Drops the getfield receiver TEST/JZ where the dataflow proves it dead.
    // DEFAULT-ON since its soak. Still the only switch in this backend whose
    // wrong arm is SILENT -- a stale or mis-shaped entry does not produce a
    // wrong answer, it resumes execution at an address the table chose -- which
    // is why the `0` opt-out stays: taking this mechanism out of the picture in
    // one run, on the same binary, is the first move when debugging an
    // unexplained crash in compiled code.
    // OPT-IN, and known to miscompile until the ARG_REGS audit lands -- see
    // `x64::operand_cache_enabled`. Declared so the two arms are measurable in
    // one binary, which is what the previous shape (no flag at all) prevented.
    // JIT round 19 wave 3, lane throws3 (THROWS3-1): opt-in perf, default OFF. A self-recursive static
    // VOID method's recursive call takes the optimizing tier's direct self-call (as int / reference /
    // wide returns already do) instead of `jit_invoke_dispatch`; needs the compile's loader-identity
    // proof (jit `selfrec_direct_void_enabled` / `selfrec_direct_void_admits`).
    // Default-ON: `conservative_roots::nested_trace_frames_enabled` treats the
    // key's PRESENCE as "restore the one-frame-per-chain-entry answer".
    // Default-ON: `stackwalker::osr_frame_dedupe_enabled` treats the key's
    // PRESENCE as "report the OSR continuation twice again".
    // Default-ON: an OSR entry pc must have an EMPTY abstract expression stack
    // (`x64::osr::osr_empty_stack_entry_enabled`, presence-parsed, so `=0`
    // still turns the rule OFF and `off_word` must stay `None`). A soundness
    // rule HotSpot also enforces — declared because it is default-on CODEGEN
    // that landed without a failure of its own, and the page that opened the
    // question named the missing switch as its last open item.
    // Retired: CRATONVM_JIT_NO_OSR_EMPTY_STACK_ENTRY (removed from INVENTORY when its reader was deleted).
    // Default-ON: `jit_bridge::osr_pc_refresh_enabled` treats the key's
    // PRESENCE as "stop publishing OSR continuations; report the back-edge".
    // Separate from the dedupe row above because the two answer different
    // questions -- which activation a frame belongs to, and where that
    // activation currently is -- and an OSR trace defect can be attributed to
    // one or the other only if they can be switched independently.
    // Default-ON: `conservative_roots::compiled_frame_bci_enabled` treats the
    // key's PRESENCE as "restore `byte_code_index: -1` / `(Unknown Source)`
    // for every JIT frame". A suspect line in a warmed-up stack trace can then
    // be attributed to this bci recovery or to the LineNumberTable inside ONE
    // binary, which a cross-binary comparison cannot do.
    // Default-ON, and the key's PRESENCE reverts BOTH halves: the emitter
    // (jit/src/x64/inlining.rs) stops recording the PC->inline-chain map, and
    // the walk (vm/src/jit/conservative_roots.rs) stops expanding it. One name
    // for two halves deliberately -- a half-switched feature is worse than
    // either state: metadata nobody reads, or a walk reading a map nobody
    // emitted. Sits beside `compiled-frame-lines`, the structurally identical
    // sibling added on the same branch for defect 1 of the same page.
    // Default-ON. The OPTIMIZING tier's safepoint-id slot holds a monotonic
    // counter, not a bci, so `activation_bci` refused the whole backend and
    // every C2 frame printed `(Unknown Source)` -- the largest population of
    // line-less compiled frames left after 2026-09-01. `ir_lower` now records
    // the id->bci translation (`CompiledMethod::safepoint_bci_table`) and the
    // walk reads through it. Separate from `compiled-frame-lines`, which
    // reverts the recovery on BOTH backends and so cannot attribute a suspect
    // line to the translation rather than to the slot read.
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
    // D2: a guarded-virtual site emits guard, splice AND miss edge under ONE
    // safepoint bci, and the miss edge records no inline-frame row -- so that
    // bci held exactly one chain, was never poisoned, and the innermost frame
    // could be handed the chain of a splice THAT DID NOT RUN. A fabricated
    // frame is worse than a missing one, because a reader cannot tell. The fix
    // emits an empty-chain row at the miss edge so the EXISTING disagreement
    // rule poisons the bci. Needs its own name rather than riding
    // `inline-frame-map`: that switch kills the whole producer and so cannot
    // separate "the poison took this frame" from "the map never had it".
    // B1: the IR tier grew a String-access expander (length/isEmpty/charAt),
    // but the invoke-planning gate still refused EVERY `java/lang/String`
    // invoke, so the expander was unreachable and its documented A/B measured
    // a different program. The carve-out asks `try_resolve_string_intrinsic`
    // whether THIS site is one the expander handles; every other String method
    // (hashCode, equals, compareTo, indexOf, and every CharSequence receiver)
    // still bails exactly as before. Presence restores the blanket refusal.
    // B5: a spliced direct call emitted its oop map AFTER the RBP republish, so
    // the map was filed under an offset 9-25 bytes past the return address and
    // `compiled_frame_bci`'s exact lookup missed EVERY time it was attempted,
    // silently falling back to the safepoint-id slot. Re-stamps the key; the
    // emitted bytes are byte-identical either way, so this is a pure metadata
    // A/B. Reordering the emission instead would be WRONG -- the map emitter
    // also emits shadow-reload code, and the republish can clobber RSI/RDI.
    // B6: the aastore emitter handled compressed oops itself but had no ZGC
    // coverage, so an armed cycle would read a colored word with no colour test
    // and write a plain pointer the classifier then truncates to 42 bits.
    // Routes to the `jit_aastore` helper only when the barrier is armed, which
    // nothing does today -- the gate is expected to fire ZERO times, which is
    // exactly why its census carries a denominator.
    // Default-ON: `stackwalker::call_frame_dedupe_enabled` treats the key's
    // PRESENCE as "report the duplicate frame again". This is the ORDINARY
    // compiled-activation half of `drop_osr_continuations`; the
    // `osr-frame-dedupe` row above still switches off BOTH halves, so the two
    // rules are separable in one binary only because this row exists.
    // Default-ON, `"0"` turns it off — same polarity as `ir-reloc-emit`, and
    // the row must spell it that way round. The consumer refuses to install a
    // compilation stamped older than the last cache flush; setting this to `0`
    // restores publish-anyway for bisection, which is an unsafe-on-purpose
    // lever. Added a wave after the declaration sweep closed at zero
    // offenders, which is exactly how the count creeps back up.
    // DEFAULT-ON since 2026-09-06 (`ir_aastore_enabled` reads `0`/`false`/
    // `off`/`no` as off, unset as on), so a KILL SWITCH: `off_word` must be
    // `"0"` or `CRATONVM_JIT=-ir-aastore` merely unsets the key, which is ON
    // (round 9 wave 2).
    // 2026-09-24 (round 11 wave 9). DEFAULT-ON kill switch for IR lock
    // coarsening (`ir_optimize::coarsen_adjacent_monitors`): an adjacent
    // `monitorexit(o); monitorenter(o)` on one control node is deleted.
    // JIT round 16 wave 12, lane syncco: loop lock coarsening, default ON (`=0` off),
    // (`ir_optimize::coarsen_loop_sync_windows`): a counted loop whose body is
    // one synchronized splice window takes the window once per 4 iterations.
    // JIT round 16 wave 16, lane monitor4 (MON3-1): loop lock coarsening of a
    // bytecode `synchronized (STATIC) { .. }` block (`ir_optimize::
    // plan_loop_block_coarsening`): one pair per 4 iterations, per-copy frames,
    // an identity re-read of the static per copy. Opt-in until the Linux arms.
    E { group: Group::JIT, token: "ir-loop-lock-coarsen-blocks", on_key: Some("CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_BLOCKS"), off_key: None, off_word: Some("0"), since: "2026-10-02" },
    // JIT round 17, lane monitor5 (MON4-1): the same loop block coarsening for an
    // instance monitor -- `synchronized (this.lock)` (re-read per copy, identity
    // exit) and `synchronized (this)` / a loop-invariant local. Opt-in until the
    // Linux arms; independent of the static kind's switch above.
    E { group: Group::JIT, token: "ir-loop-lock-coarsen-instance-blocks", on_key: Some("CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_INSTANCE_BLOCKS"), off_key: None, off_word: Some("0"), since: "2026-10-02" },
    // JIT round 17 wave 1, lane monitor: the two block switches above build the
    // IN-PLACE shape (`ir_optimize::apply_loop_block_coarsening_in_place`): the
    // original block is copy 0, copies 1..3 follow it behind a loop test and an
    // identity test each, no fork and no second copy of the block. Opt-in; inert
    // unless a block switch is on.
    // JIT round 17 wave 2, lane monitor2 (MON3-3 / MON4-3 / MON5-4): a block
    // coarsened on a trusted `static final` monitor of an initialised class
    // (`Graph::trusted_final_static_fields`) takes it as one object per loop:
    // no per-copy re-read and no identity exit. Opt-in; inert unless a block
    // switch coarsens the loop and the compile's feed lists the field.
    // JIT r20 wave 11 pIR (books jit-r20-nio-proposals-w6-L3 L3W6-2 / w6-L4 W6L4-5): arraylength of a trusted static final array of an initialised class folds to the length the VM filed at the end of <clinit> (`x % TABLE.length` -> constant divisor). Default ON, kill "0".
    // JIT round 17 wave 2, lane monitor3: a loop whose synchronized SPLICE window
    // the default-on window pass coarsened counts as evidence at the C1->C2
    // acceptance gate (`ir_evidence::is_worth_publishing`); the coarsening's node
    // growth hides the `simplified` bit such a body would otherwise earn. Opt-in.
    // (A coarsened bytecode BLOCK is evidence whenever a block switch built it.)
    // JIT round 17 wave 2, lane irfeed (MON3R17-1): the C1->C2 acceptance gate
    // also takes a shrink `ir_optimize::optimize_passes` measured BEFORE the
    // growing passes (partial unroll, loop coarsening) as `simplified`
    // evidence; the net count those passes' copies mask. Opt-in until the H2
    // A/B against `CRATONVM_C2_ACCEPT=always`.
    // JIT round 17 wave 3, lane irlower (IRFEED17-2): the acceptance gate counts
    // a FULL unroll (`ir_evidence::note_full_unroll`, a loop retired) as
    // evidence; a partial unroll still is not. Default ON since JIT r27 LOOP27-4
    // (`=0` kills).
    // nio20 wave 6, lane L3: opt-in (default OFF). A fired inline call prefix counts as acceptance
    // evidence (jit `ir_evidence::call_prefix_is_evidence`); recorded always.
    // JIT round 21, lane accept (79fbc8a88c, host-verified r21 wave 1; dropped by merge 624fae0b49,
    // re-landed t1 wave 8 lane ACCEPT): default ON (`=0` kills). A call site the IR builder expanded
    // inline with no single-pass counterpart (ThreadLocal.get hit path, the three indy expansions)
    // counts as acceptance evidence (jit `ir_evidence::builder_expansion_is_evidence`).
    // t1 wave 8 lane ACCEPT (ACCEPT21-2, docs/feature-designs/wire-tiered-manager.md):
    // opt-in (default OFF). An ALIGNED ScopedMemoryAccess fold (zt t31; no single-pass twin) notes a
    // builder expansion (jit `ir_evidence::scoped_aligned_fold_is_evidence`). Pending a host A/B.
    // t1 wave 8 lane ACCEPT (refusal-memo review): memo correctness, default ON (`=0` kills). The
    // acceptance gate's input-stamped refusal carries the inputs generation read when the compile
    // was armed, not the one current at the write (jit `ir_evidence::refusal_stamp_at_arm_enabled`).
    // JIT round 23 wave 8, lane W8-L2 (CF-W8-3): default ON (perf; kill switch "0"). The acceptance
    // gate prices an in-splice blind dispatch that only a rarely taken arm reaches (callee profile,
    // taken share <= 1/8) at that share, floored at 31 permille (jit `ir_evidence`,
    // `splice_cold_arm_ranges`).

    // JIT round 21, lane accept (jr21-acceptance-gate-discards-builder-expansions-FIXED-20261015.md):
    // default ON (`=0` kills). A call site the IR builder expanded inline with no single-pass
    // counterpart (ThreadLocal.get hit path, the three indy expansions) counts as acceptance
    // evidence (jit `ir_evidence::builder_expansion_is_evidence`); recorded always.
    // JIT round 17 wave 2, lane irfeed (MON3R17-2): a loop block coarsening
    // switch admits lock-holding OSR guard exits only in a body whose compile
    // coarsened a block (`osr_exit::osr_guard_exits_holding_a_lock_for_this_compile`
    // / `_admitted_for`), not in every OSR body. Default ON (`=0` restores the
    // switch-wide rule); inert unless a block switch is on.
    // 2026-09-24 (round 11 wave 14). OPT-IN IR nested-lock elision
    // (`ir_optimize::elide_nested_monitors`): the inner pair of
    // `synchronized (o) { synchronized (o) { .. } }` is deleted. DEFAULT-ON
    // kill switch since round 11 wave 17.
    E { group: Group::JIT, token: "ir-sink-equal-depth", on_key: Some("CRATONVM_JIT_IR_SINK_EQUAL_DEPTH"), off_key: None, off_word: Some("0"), since: "2026-09-12" },
    // JIT round 16 wave 5, lane codegen (CG16-9): equal-depth sinking for
    // `int`/`long` values and compares only; default OFF, an A/B lever.
    // Round 11 wave 12 (irback W11-2, lane ircore): the sink pass's φ-edge read
    // attribution without the equal-depth tie rule. OPT-IN.
    // Round 16 (lane indy, SY4-1): the optimizing tier CALLS a bridged
    // `invokedynamic` (concat, lambda, pattern switch, record methods) through
    // the single-pass tier's `indy_bridge` instead of trapping at it.
    // Opt-in (default OFF) until measured.
    // JIT round 20 wave 11, lane pIndy (W10L3-3 / W11L3-3 of
    // `docs/feature-designs/jit-ir-optimizations.md` /
    // `-w11-L3.md`): the optimizing tier builds a method's own non-capturing
    // lambda site with a published singleton as `word != null ? word :
    // bridge` (no memory effect on the hot path), independently of
    // `ir-indy-bridge`. Default ON; `=0` keeps the trap or the bridge.
    // JIT round 21, lane lambdatrap (TLOCAL21-2 of
    // `docs/feature-designs/jit-ir-optimizations.md`; page
    // `jr21-tlocal-lambda-word-optimizing-body-traps-20261014.md`): with
    // `ir-indy-lambda-word` off, the optimizing tier builds a method's own
    // non-capturing lambda site whose singleton is published as the
    // single-pass tier's bridged call (its constant-word probe answers the
    // singleton inline) instead of an uncommon trap. Default ON; `=0` keeps
    // the trap.
    // JIT round 21, lane lambda2 (TLOCAL21-1 of
    // `docs/feature-designs/jit-ir-optimizations.md`, LAMBDATRAP21-3 of
    // `docs/feature-designs/jit-ir-optimizations.md`; page
    // `jr21-tlocal-capture-new-kernel-20261014.md`): the optimizing tier
    // builds an `invokeinterface` SAM call whose receiver is provably the
    // value of the method's own lambda `invokedynamic` (a capturing site's
    // inline proxy, a non-capturing site's published singleton) as an
    // `invokestatic` of the lambda's static impl, captures first. Default ON;
    // `=0` keeps the interface call.
    // JIT round 21, lane lambda3 (page
    // `docs/feature-designs/jit-ir-optimizations.md`,
    // "Round 21 lane lambda3"): under `ir-lambda-impl-call`, a CAPTURING
    // lambda's SAM call becomes its impl call only where the impl splices;
    // a build refused with a lambda plan in force is retried without it.
    // Default ON; `=0` restores lane lambda2's plan.
    // JIT round 21, lane lambda3, second host round (page
    // `jr21-tlocal-capture-new-kernel-20261014.md`): under
    // `ir-lambda-impl-call`, plan CAPTURING lambda sites too. Opt-in: two
    // capturing rows still lost 3.3x with the spliced shape engaged.
    // JIT round 21, lane lambdacap (page
    // `jr21-tlocal-capture-new-kernel-20261014.md`, "Round 21 lane
    // lambdacap"): an optimizing body that built a capturing lambda's proxy
    // keeps the VM-context entry ABI after escape analysis removed the proxy,
    // so the supersede forward and late-static cells can reach it
    // (`jit::ir_lower::ir_lambda_proxy_keeps_context_enabled`). Default ON;
    // `=0` restores the context-free body.
    // JIT round 21 wave 15, lane lambda3 (LAMBDACAP21-2 of
    // `docs/feature-designs/jit-ir-optimizations.md`; page
    // `docs/feature-designs/jit-ir-optimizations.md`):
    // under `ir-lambda-impl-call`, a SAM call inside a SPLICED body on a lambda
    // that body built gets the lambda's static impl spliced in its place
    // (`jit::ir_graft_spliced_lambda_impls`). Default ON; `=0` keeps the
    // inline-cache call there.
    // t1 wave 8, lane LAMBDATRAP: routing, default ON (the jr21 A/B above is this trade), kill "0". A published-word site
    // keeps its bridge under the refusal-only plan (ir-indy-bridge-on-trap-refusal-loop-methods) instead of its trap.
    // t1 wave 11 (lane JLAM, LAMBDATRAP21-4): perf, default ON since the j27 flip of 2026-10-28, kill "0". A method's own non-capturing lambda site that
    // nobody evaluated yet (no published singleton) is built as the single-pass tier's Op::IndyBridge instead of the trap
    // whose first firing bans the optimizing body; lambda sites only (vm `invokedynamic::jit_indy_site_is_constant_lambda`,
    // jit `ir_indy_late_lambda_bridge_enabled`, census `[ir] indy-late-lambda-bridge` under CRATONVM_DBG_JITC).
    // JIT round 20 wave 12, lane qLamNew (W10L3-2 / W11L3-2 second half /
    // W9L2-1 part 2 of `docs/feature-designs/jit-ir-optimizations.md`,
    // SPLICE20-1's allocation half): the optimizing tier builds a method's own
    // linked CAPTURING lambda site as `Op::New` of its VM-spun proxy class plus
    // one store per capture (escape analysis may then scalar-replace it), from
    // the site's `(proxy class, capture count)` fact. Default ON; `=0` keeps
    // the trap or the bridge.
    // JIT round 21 wave 19, lane lambda3fix (LAMBDA321-4 of
    // `docs/feature-designs/jit-ir-optimizations.md`; page
    // `docs/feature-designs/jit-ir-optimizations.md`):
    // under `ir-lambda-impl-call`, a SAM call on a lambda made before the loop
    // that applies it (a local the method writes once, outside every loop) is
    // planned as its impl call too (`jit::lambda_single_store_outside_loops`).
    // Default ON; `=0` keeps the inline-cache call there.
    // JIT round 20 wave 19, lane ySplice (SPLICE20-1 of
    // `docs/feature-designs/deopt-osr.md`): a spliced
    // callee's linked CAPTURING lambda site is built as its proxy's `Op::New`
    // plus one store per capture, as `ir-indy-capture-new` builds a method's
    // own (the callee is spliced instead of staying a call). Default ON; `=0`
    // keeps the `capturing ... no constant word` refusal.
    // JIT round 20 wave 19, lane ySplice (SPLICE20-2): a spliced callee whose
    // `new` names a class not loaded (or not initialized) yet, on an arm the
    // callee's own branch profile never ran, is spliced with a precise exit at
    // that `new` (the interpreter resolves, loads and initializes there; the
    // first exit asks a recompile). Default ON; `=0` keeps the
    // `ir-splice-new-site-unresolved` refusal.
    // r23 wave 9, lane L6: perf, opt-in "1" (no win on the n23-w9a A/B). A spliced callee whose `ldc` of a CONSTANT_String is not
    // resolved yet, on an arm the callee's branch profile never ran (StringLatin1.newString's `len == 0 -> ""`), is
    // spliced with a precise exit at that `ldc` instead of being refused (`ldc-non-numeric-constant`); the
    // interpreter resolves the constant there and the exit asks a recompile (jit `ir_splice_unresolved_ldc_exit_enabled`).
    // JIT r25, lane regex rows: perf; default ON since R25-FLIP, `=0` kills. The inline resolver's `static-intrinsic-keeps-its-own-fast-path` refusal
    // no longer applies to a `System.arraycopy` inside an optimizing-tier splice, which binds to the same direct helper
    // and inline prefixes as the caller's own site; so Arrays.copyOfRange([BII) splices into StringLatin1.newString
    // (with ir-splice-unresolved-ldc-exit, into every substring) (jit `ir_splice_keeps_arraycopy_bind`).
    // JIT round 20 wave 19, lane ySplice (SPLICE20-7): the single-pass tier
    // splices a callee whose `invokedynamic` is a constant-word lambda site
    // (the published singleton, read through the site's constant word, the
    // bridge cold), as the optimizing tier does. Default ON; `=0` keeps the
    // `invokedynamic` refusal of the single-pass resolver.
    // L1W1-1 of docs/feature-designs/jit-ir-optimizations.md
    // (churn page J2-residual): the single-pass tier splices a constant-length
    // primitive `newarray` (`0xbc`) of a callee with no exception table and a
    // unique safepoint-bci inline-frame chain. Default ON; `=0` keeps the
    // single-pass refusal.
    // Round 16 (lane matrix): hoist a loop-invariant array element read (the
    // row `c[i]` of `c[i][j]`) out of its loop with a header-frame deopt, so
    // the range predicate can guard the row. Default ON since round 16.
    // Round 16 (lane matrix): admit `multianewarray` to the optimizing tier
    // (`Op::MultiNewArray`, a call to the single-pass `multianewarray_n`
    // helper). Default ON since 2026-10-06.
    // Round 16 (lane matrix): plant no range predicate in the optimizing OSR
    // route's compile; its guard makes the OSR door refuse the whole body.
    // Opt-in (default OFF) until measured.
    // Round 16 (lane matrix, MX16-1): OSR entry stubs re-execute the hoisted
    // array reads and range predicates they jump past (and leave for the
    // interpreter when one fails), so the OSR route may hoist and keep its
    // predicates. Default ON since 2026-10-06.
    // Round 16 matrix residual (MX16-5): the element-wise vector pre-loop on a
    // counted loop's entry edge (`jit/src/ir_vector_loop.rs`). Default ON;
    // `0` turns it off.
    // Alias-safe field preparation for wide numerical primitive-array loops.
    // AVX-512 testing defaults; every path has an individual and master veto.
    // Integer sum widening; default ON for AVX-512 testing.
    // r21 vector (JIT round 21 wave 4, lane vector; book `docs/feature-designs/jit-ir-optimizations.md`,
    // probe `R21VectorRows.java`). Perf, opt-in. VEC21-1: the IR vector pre-loop admits
    // `& | ^` and constant-count shifts (count masked as Java masks it) over int/long lanes (jit `ir_vector_bitops_enabled`).
    // r21 vector: perf, opt-in. VEC21-3: byte[] and short[]/char[] lanes for `+ - & | ^` (and 16-bit `*`), whose
    // low result bits depend on the low operand bits only (jit `ir_vector_subword_enabled`).
    // r21 vector: perf, opt-in. VEC21-4: int/long `s += f(a[j], ..)` / `s -= ..` reductions (wrapping, so exact);
    // FP accumulators stay refused (jit `ir_vector_reduce_enabled`).
    // r23 wave 1, lane L8 (VEC21-5): perf, default ON, kill "0". The IR vector pre-loop admits `m = Math.min/max(m,
    // f(a[j], ..))` reductions over int/long/float/double lanes, Java-exact (FP: -0.0 below +0.0 by an AND/OR of the
    // two operand orders; any NaN, element or seed, discards the pre-loop and the scalar loop runs from the entry)
    // (jit `ir_vector_minmax_enabled`).
    // r23 wave 1, lane L8 (VEC21-8): perf, default ON, kill "0". The IR vector pre-loop serves an early-exit scan
    // (`for (j..n) if (a[j] OP x) break/return`) over byte/short/char/int/long lanes: it only advances `j` to the
    // first hit, and the untouched scalar loop takes that iteration (jit `ir_vector_scan_enabled`).
    // r23 wave 1, lane L8 (BCE21-5, vector side): perf, default ON, kill "0". The IR vector pre-loop admits a `j != n`
    // loop test like `j < n` (it steps only while `j + W <= n`; a `j > n` entry takes no step) (jit
    // `ir_vector_ne_exit_enabled`).
    // r21 vector residuals (VEC21-6): perf, default ON, kill "0". The IR vector pre-loop admits `int`/`long` shifts
    // by a loop-invariant count (`a[j] << k`): the count is masked the Java way (`& 31` / `& 63`) into its own
    // register once, before the loop (jit `ir_vector_varshift_enabled`).
    // r21 vector residuals (VEC21-2): perf, default ON, kill "0". The IR vector pre-loop admits loads narrower than
    // its lanes, widened at the load exactly as the scalar `xaload` / `i2l` / `f2d` (`d[j] = c[j]` char -> int,
    // `l[j] = a[j] * k` int -> long, `d[j] = f[j]` float -> double, `s += b[j]`) (jit `ir_vector_widen_enabled`).
    // r21 vector residuals (VEC21-2): perf, default ON, kill "0". `long` lane multiply in the IR vector pre-loop
    // (three `VPMULUDQ`, the wrapped 64-bit product) (jit `ir_vector_long_mul_enabled`).
    // r21 vector residuals (VEC21-2, narrowing half): perf, default ON, kill "0". `int` lanes truncated into a
    // `byte[]`/`short[]`/`char[]` store (`b[j] = (byte) a[j]`), exactly as `bastore`/`sastore`/`castore`; a
    // `boolean[]` destination stays the scalar loop's (jit `ir_vector_narrow_enabled`).
    // r23 L8 close (L8-3): perf, default ON, kill "0". `Math.min/max(x, y)` as an element-tree node of a vector store
    // loop (`c[j] = Math.max(a[j], b[j])`) over int/long/float/double lanes; FP is the JDK body's bits in every lane
    // (both operand orders combined, then each argument's NaN blended in) (jit `ir_vector_elem_minmax_enabled`).
    // r23 L8 close (L8-4): perf, default ON, kill "0". A vector scan's test may be a chain of compares joined by
    // `||`/`&&`, over element trees (sub-word elements then widened to int lanes), FP lanes (VCMPPD, the predicate
    // from the fcmpl/fcmpg NaN rule) and unsigned char order compares (jit `ir_vector_scan_wide_enabled`).
    // r23 L8 close (L8-2, VEC21-7): perf, default ON, kill "0". On Windows a body with a vector pre-loop saves
    // XMM8-XMM11 in its prologue so the pre-loop may use them (no effect on System V, where they are volatile)
    // (jit `ir_lower::vector_xmm_saves_wanted`).
    // r23 L8 close (L8-1 residual): perf, default ON, kill "0". The partial unroller leaves a loop the IR vector
    // pre-loop admits to the pre-loop (the main/post form had been unrolling reductions first, leaving the
    // planner only the post loop) (jit `ir_optimize::unroll_yields_to_vector_enabled`).
    // r23 L8 close (L8-7): perf, default ON, kill "0". An If arm that opens with an always-failing guard (a spliced
    // class-loading / unloaded-call / athrow exit) has the guard moved to the branch and the arm cut, so the code the
    // builder walked past the trap (a call, field reloads) no longer joins the live path as phis (jit
    // `ir_optimize::hoist_arm_entry_traps`).
    // r23 L8 heap-buffer close: perf, default ON, kill "0". A deopting compactness guard (JIT r25 proven receiver
    // fields) is withdrawn after one charged trap, and at every field site on the same receiver node, instead of
    // four traps per site: a legacy receiver walked the guard from field to field until the method was made not
    // compilable (jit `deopt::proven_compact_one_trap_enabled`, vm `deopt_resume::per_bci_despec_limit`).
    // r23 L8 heap-buffer close: perf, default ON, kill "0". The affine guard predicate also takes an `int`
    // expression of the induction (`k << 2`), with the pre-header checking that it fits at both extremes
    // (jit `affine_guard_predicate::affine_int_enabled`).
    // r23 L8 heap-buffer close: perf, default ON, kill "0". A guard's LICM condition cone holds a field read but
    // not the object it was read from, so a guard on `b.f`'s value no longer keeps `b.g` in the loop (jit
    // `ir_optimize::licm_guard_cone_field_value_enabled`).
    // r23 L8 heap-buffer close: perf, default ON, kill "0". A spliced body's call operands may be pure expressions,
    // so the receiver of `b.putInt(x + y)` stays known and its exact-receiver row is planned (jit
    // `ir::splice_operand_expressions_enabled`).
    // r23 L8 heap-buffer close: perf, default ON, kill "0". A counted loop's guard on a value its own shape bounds
    // (`(i & 7) << 4`) is planted once at the bound, and a scoped access at such an offset is proven over it (jit
    // `affine_guard_predicate::range_guard_predicate_enabled`).
    // r23 L8 heap-buffer close: perf, default ON, kill "0". The affine and scoped-access predicates also key on a
    // derived induction `v = phi(v0, v + s)` of a counted loop (a relative buffer accessor's `position`), with the
    // pre-header checking that it stays an `int` (jit `affine_guard_predicate::affine_derived_iv_enabled`).
    // r23 L8 heap-buffer close, item 3 residual: an unloaded checkcast / instanceof with no proving profile is
    // still a ClassLoading splice exit when a null test proved its operand non-null (jit
    // `ir::typecheck_operand_nonnull_by_test`); default ON, kill "0".
    // Round 16 matrix residual (MX16-2): one more LICM pass after the
    // array-load hoist moved a row, so `row.length` in a header leaves too.
    // Default ON; `0` turns it off.
    // Round 16, lane indy (IB-3): a stash at an optimizing body's own IR site trap
    // takes the site-trap policy instead of a `TransferToInterpreter` charge.
    // Round 16 wave 7, lane indy: an `invokedynamic` linked through a generic
    // bootstrap (a JDK-linked shape, a user or Groovy bootstrap) is bridged by
    // the compiled tiers once the interpreter has linked it, and its bound
    // `CallSite` target is invoked without a synthetic frame. Opt-in until measured.
    // Round 16 wave 9, lane indy (IN16-7): a linked call site whose `CallSite`
    // target is a direct handle of exactly the site's type calls the target
    // method through the compiled dispatch instead of `MethodHandle.invoke`.
    // Opt-in until measured.
    // Round 16 wave 10, lane indyir (IN16-8): the optimizing tier compiles a
    // constant call site whose target is a direct handle of exactly the site's
    // type as a plain invoke of that target (direct binds, inline caches, IR
    // inlining). Opt-in until measured.
    // The constant twin (REFLECT21-5): a `ConstantCallSite` bridge that keeps serving the direct leaf the
    // optimizing tier would compile as a plain call asks the same bounded recompile. Default ON; `=0` keeps
    // the one request the first bind makes.
    // A trap at an `invokedynamic` that was only unlinked when the body was built, and that the linked bridge
    // serves once linked, is charged as a recompile instead of retiring the method (single-pass
    // `UnreachedCode` = not compilable; IR site trap = sticky IR ban). Default ON; `=0` keeps the old charge.
    // A guarded `MutableCallSite` / `VolatileCallSite` site's planned STATIC target is spliced on the guard's
    // hit edge (`IrBuilder::try_indy_mutable_guarded_splice`) instead of called. Default ON; `=0` keeps the
    // guarded call.
    // mcs21 (MCS21-2): a guarded `MutableCallSite` / `VolatileCallSite` site's `CallSite.target` read carries the
    // compact row the VM names (`IndyMutableGuard::field_row`), so a compact call site is read inline instead of
    // through the checked `jit_getfield` helper on every execution. Opt-in until a host A/B has priced it.

    // JIT round 21 lane reflect (page `jr21-reflective-invoke-and-mcs-flip-handoff-rows-FIXED-20261014.md`, row
    // `R20PIndyMutableSite` mcs-stable): a `MutableCallSite` / `VolatileCallSite` bridge that keeps serving
    // the target its newest guard cell plans asks, at powers of two of those calls and at most four
    // queued times per site, for the optimizing recompile that guards it. Default ON; `=0` keeps the
    // one request the first cell makes.
    // JIT round 21 lane mcs (page `jr21-mcs-guarded-indy-call-lost-its-direct-row-20261014.md`, rows
    // `R21IndylateRows` mcs-* / vcs-*, `R21ReflectMcsStable`, `R20PIndyMutableSite` mcs/vcs-stable): a guarded
    // `MutableCallSite` / `VolatileCallSite` site's planned STATIC target is spliced on the guard's hit edge
    // (`IrBuilder::try_indy_mutable_guarded_splice`) instead of called. Default ON; `=0` keeps the guarded call.
    // JIT round 21 lane indyconst (page `jr21-indy-constant-recompile-request-spent-on-already-queued-FIXED-20261015.md`,
    // REFLECT21-5): the constant twin -- a `ConstantCallSite` bridge that keeps serving the direct leaf the
    // optimizing tier would compile as a plain call asks, at powers of two of those calls and at most four
    // queued times per site, for the optimizing recompile (the first bind's one request was spent on
    // `AlreadyQueued` or came after the optimizing compile). Default ON; `=0` keeps the one request.
    // JIT round 21 lane indyconst (page `jr21-one-shot-recompile-latches-spent-on-already-queued-FIXED-20261015.md`):
    // a one-shot optimizing-retry request whose latch is already spent (the deferred-`new` memo at the eager
    // door and the class-load re-sweep, the IR inline-cache profile seed's per-method claim) that the tier
    // answers `AlreadyQueued` is owed to the background task holding the slot (`late_bind_owed`), which asks
    // the C2 retry after `finish`. Default ON; `=0` drops the request as before.
    // JIT round 21 lane tiermgr (INDYCONST21-1, page `jr21-tiermgr-lost-work-in-the-tier-manager-FIXED-20261027.md`):
    // a C2 retry from ANY door that finds the method-entry slot held is remembered in the tier manager
    // (`MethodState::retry_owed`); an optimizing compile dispatched after the request answers it, any other exit
    // of the slot holder asks it again. Default ON; `=0` drops the `AlreadyQueued` request as before.
    // JIT round 21 lane tiermgr (INDYLATE21-1, same page): the acceptance memo's input-stamped refusal also
    // carries a per-method input stamp every C2 retry door moves, and is stamped with what the refused build
    // READ (taken at the compile door), so a retry is not SKIPPED by a refusal judged on older inputs.
    // Default ON; `=0` keeps the process-wide generation read at the refusal.
    // The linear-scan consumer's SPLIT half: follow a value the scan split from
    // register to register and back to memory, instead of declining it whole.
    // Off because it is unmeasured, not because it is unsound -- see R4 in
    // `NOTES-regalloc.md` and `docs/jit/linear-scan-regalloc.md`.
    //
    // `on_key` only, like `ir-ref-residency`: the flag is opt-in and has no
    // kill switch, because it IS the kill switch.
    E { group: Group::JIT, token: "ir-param-copy", on_key: Some("CRATONVM_JIT_IR_PARAM_COPY"), off_key: None, off_word: Some("0"), since: "2026-09-04" },
    // 2026-09-24 (round 11 wave 14). DEFAULT-ON kill switch: an int/long
    // parameter's home is its prologue word, not a coloured copy
    // (`ir_lower::Lowerer::alias_param_home`).
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
    // The statically-bound door's sibling of `mic-exc-table-publish`: may a
    // baked direct CALL target a callee that declares its own exception table?
    // Default-OFF pending its own measurement (16 refused binds on netty's
    // BigEndianHeapByteBufTest against 736 for the native shadow), and
    // interlocked with `sp-ic-deopt-check` the same way.
    // Serve `new`'s JVMS 5.5 initialization check from `class_init_memo`
    // instead of `ensure_class_initialized_shared`, as getstatic/putstatic
    // already do. Default-ON; the opt-out is the same-binary A/B, and its OFF
    // position is the older, correct, slower path.
    // Optimizing tier: an inline G1 reference store (compact receiver, null
    // old value, null or same-region new value), else the full helper.
    // DEFAULT-ON (`runtime_flag_default_on`), so a KILL SWITCH and `off_word`
    // is `"0"`.
    // Optimizing tier: redundant-load elimination across a dominating block.
    // Only acts under `CRATONVM_JIT_IR_LOAD_CSE`. DEFAULT-ON
    // (`runtime_flag_default_on`), so a KILL SWITCH and `off_word` is `"0"`.
    // Single-pass tier (round 16 lane alloc, OT-3): a spliced `this.f = x`
    // whose local 0 is a trusted oop takes the null-test receiver guard
    // instead of the heap-range containment check. Default ON since round 16
    // (measured: single-pass alloc-store 18.2->16.9 ns, alloc-ref-dead
    // 14.7->13.5 ns; `runtime_flag_default_on`), so a KILL SWITCH, `"0"`.
    // Optimizing tier (round 16 lane alloc): the single-pass tier's inline ZGC
    // start-bit store in front of the IR inline `new`'s announce call.
    // Default ON since round 16 (measured: alloc-ref-dead 20.5->10.5 ns,
    // alloc-store 20.6->11.6 ns, R16Bench bintrees 124->87 ms, alloc
    // 13.9->10.4 s; `runtime_flag_default_on`, so a KILL SWITCH, `"0"`); needs
    // `CRATONVM_ZGC_JIT_INLINE_ANNOUNCE` (default on).
    // Both tiers (round 16 wave 7, lane strings): under ZGC a compiled
    // primitive `newarray` bumps the TLAB inline and announces the array
    // through `zgc_note_tlab_object` (inline start-bit store first) instead of
    // calling `jit_newarray` per allocation. Default ON (zs s1/ca: this note said opt-in; the switch is `runtime_flag_default_on`), kill "0".
    // r21 alloc (JIT round 21 wave 2, lane alloc; probe `R21AllocLean.java`, page
    // `docs/feature-designs/profile-guided-inlining.md`).
    // Correctness, default ON, kill "0": on ZGC the IR `new`'s jit_post_tlab_init arm calls before it commits the
    // TLAB cursor (finalizer-free classes only), closing the frozen-peer window the GC-side cover held.
    // r21 alloc: perf, opt-in. The IR inline anewarray bump on Generational and G1 (no announce); it was ZGC-only.
    // JIT round 23 wave 2b (lane L4): default ON, kill "0" (G1 objarr16 46.7 -> 22.2 ns/op, Gen 40.6 -> 28.6; the r22 A/B ran on ZGC only).
    // r21 alloc: perf, opt-in. `ArrayLength(NewArray(Const n))` folds to `Const(n)`.
    // r21 alloc: perf, opt-in. Escape analysis deletes a constant-length primitive array (<= 64 elements) nothing live uses.
    // JIT round 23 wave 2b (lane L1, churn page J3): perf, default ON, kill "0". Escape analysis deletes a constant-length
    // primitive array (<= 1024 elements) nothing uses, and each EA round first folds `new T[n].length` to `n`.
    // JIT round 23 wave 2d (lane L1, TCL23-1): perf, default ON, kill "0". A field read of a loop-invariant, possibly-null
    // base leaves its loop behind a pre-header null guard that deopts to the loop entry (loop predication).
    // JIT round 23 wave 2d (lane L1, TCL23-2): perf, default ON, kill "0". LICM does not count an in-loop checkcast /
    // instanceof as a memory write, so the read hoist serves loops that type-test.
    // jit-jdk-perf: a guarded loop that stores only other fields keeps its invariant reads out (read hoist).
    // JIT round 23 wave 3b (lane L1, L1W3-1): perf, default ON, kill "0". Production compiles keep a loop's carried
    // integers in registers across the back edge (integer equal-depth sink + back-edge phi coalescing); an explicit
    // CRATONVM_JIT_IR_SINK_EQUAL_DEPTH / CRATONVM_JIT_IR_PHI_COALESCE_BACK_EDGE value wins.
    // The FP half of L1W3-1 (2026-10-25): perf, default ON, kill "0". A loop's single-use float / double recurrence
    // steps get XMM registers, and an FP phi is published at its edge from its source's register, not its home word.
    // JIT round 23 wave 3c (lane L1): perf, default ON, kill "0". The null-base load predicates also on the optimizing OSR
    // route / OSR-door graphs, for a loop entered only through its header's opaque OSR entry merge.
    // JIT round 23 wave 4 (lane L1): perf, default ON, kill "0". The null-base load predicate for a loop spliced from a
    // callee, named by the loop-entry inlined chain; charged at the call site under its chain id.
    // JIT round 23 wave 8 (lane L7): perf, default ON, kill "0". A spliced loop's entry predicates (null-base, checkcast)
    // step over LICM's pre-header copy of that loop's own length / element read (lambda-driven loops kept h.k in the loop).
    // JIT round 23 wave 4 (lane L1): perf, default ON, kill "0". The optimizing acceptance gate keeps a body with a direct
    // self-call whose frame is at least an eighth smaller than the single-pass body's (deeper compiled recursion).
    // JIT round 23 wave 4 (lane L1): perf, default ON, kill "0". A single-pass body whose stubs never spilled into the 256-byte
    // frame-deopt register region gives it back (patched SUB RSP, trimmed layout and osr_frame_size): deeper compiled recursion.
    // JIT round 23 wave 6 (lane L1): perf, default ON, kill "0". One more invariant array-element hoist round after the null-base
    // predicate moved a field read out of the loop (h.tab[5] once h.tab is hoisted).
    // JIT round 23 wave 8 (lane L7): perf, default ON since the w2c close (2026-10-24), kill "0" (opt-in before: neutral alone on n23-w8g: W2c nocast 2.11/1.21/3.88 vs 1.99/1.34/2.04 ns).
    // That round also serves a loop spliced from a callee, naming the loop-entry chain frame its null-base predicate names.
    // JIT round 23 wave 7 (lane L1): perf, OPT-IN "1" (neutral on the n23-w7 A/B). A virtual / interface site every multi-class guarded reader
    // declines splices its top receiver class (50 %+) behind one exact test, call miss edge (regex CharPredicate.is, 76 % of 5).
    // JIT round 23 wave 8 (lane L5): perf, opt-in "1" (n23-w8b RxLoop A/B: no win). A guarded site outside every loop is hot (hot body cap)
    // from 100 recorded calls, HotSpot's InlineFrequencyCount, not 256 (regex Pattern$Node.match chain: 152-214 calls).
    // JIT round 23 wave 8b (lane L5): perf, opt-in "1" (n23-w8g RxLoop A/B: no win). Each code buffer's code starts a hashed cache-line colour
    // (0-4032 bytes) into its own mapping, so compiled bodies stop sharing L1i sets (all began 64 KiB-aligned on Windows).
    // JIT round 23 wave 9 (lane L5): perf, default ON, kill "0". A depth-0 splice whose SAM call names one lambda proxy splices the
    // forwarder one level down behind a class test, instead of the whole body staying a call (regex Start.match -> Greedy.match).
    // JIT round 25 wave 7 (lane W7-B): perf, default ON since the 2026-10-25 flip ("0" kills it). A dispatch-chain link (a spliced body's virtual call of its own
    // selector, regex `next.match`) splices one level down behind a class test from the callee's receiver profile when the
    // site is hot and every other call of the link body is spliced in turn; the chain body's interface sites take the
    // profiled nested row too.
    E { group: Group::JIT, token: "ir-nested-chain-splice", on_key: Some("CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE"), off_key: None, off_word: Some("0"), since: "2026-10-24" },
    // JIT round 25 wave 10 (lane W10-3): perf, default ON under ir-nested-chain-splice (inert without it), kill "0". The unset
    // nest depth becomes the builder's frame cap and the leaf tier takes call-free static bodies up to 24 bytes, so a spliced
    // chain does not push its links' own callees (ASCII.getType under the \s predicate) back to calls.
    // JIT round 25 wave 10 (lane W10-4): perf, default ON since the 2026-10-25 flip ("0" kills it). An interface call inside a spliced lambda implementation body
    // (`CharPredicate.lambda$union$N`'s `is(ch) || p.is(ch)`) takes the profile-guarded nested row (dominant class behind an
    // exact test, call miss edge); also opens ir-nested-chain-depth.
    E { group: Group::JIT, token: "ir-nested-lambda-iface", on_key: Some("CRATONVM_JIT_IR_NESTED_LAMBDA_IFACE"), off_key: None, off_word: Some("0"), since: "2026-10-25" },
    // JIT round 25 wave 10 (lane W10-3b): perf, default ON under ir-nested-chain-splice / ir-nested-lambda-iface (inert without
    // them), kill "0". A chain / lambda-body interface site the inline String prefix answers (CharSequence.charAt) keeps its
    // call instead of the bytecode splice of String.charAt, and the chain link rule counts it as answered.
    // JIT round 25 wave 10 (lane W10-3b): perf, default ON under ir-nested-chain-splice (inert without it), kill "0". A
    // splice tree with a chain link in a loop of its own body keeps its nested hot-cap rows under a top-level site the plan
    // does not call hot (Matcher.search's root.match: the matchonly bimodality).
    // JIT round 25 wave 11 (lane W11-4): perf, opt-in "1". Turns on the loop proofs of the spliced regex scan loop
    // together (ir-seq-charat-predicate, -spliced, -field-class-predicate, ir-chain-inner-loop-proofs; since W13-4 not
    // -hoist / -entry-bound, which lost on c29/c30); an explicit "0" of one of them still kills it.
    // JIT round 25 wave 12 (lane W12-4): perf, opt-in "1" (also under ir-chain-loop-proofs). The field-class predicate and
    // the charAt entry bound are also offered every loop nested in a String-guarded loop (the spliced regex scan loop
    // inside Start.match's position loop); a miss-arm call behind guards folds with its test, the bound walk passes guards.
    E { group: Group::JIT, token: "ir-chain-inner-loop-proofs", on_key: Some("CRATONVM_JIT_IR_CHAIN_INNER_LOOP_PROOFS"), off_key: None, off_word: None, since: "2026-10-25" },
    // Round rx27 wave 1 (lane chain, W7B-2): a knob, the chain tree byte cap (default 325, FreqInlineSize; clamped to
    // the compile's 1300-byte splice budget), for the regex-groups A/B.
    // Round rx27 wave 3 (lane chain, W7B-3): opt-in, within noise on direct rows and below the bundle's 15% bar in the 2026-11-01 A/B. Inert without ir-nested-chain-splice. A nested two-class
    // row (SPLICE19-4) at a dispatch-chain site only (GroupTail.match pc 70), without the global
    // ir-nested-bimorphic-splice.
    // Round rx27 wave 4 (lane chain): perf, default ON since 2026-10-29 (kill "0"; "dse0": cached -21 %, split-ws -13 %,
    // regex-split -5 % on bimodal-10, every other row within 1.5 %), inert without ir-nested-chain-splice. Deep dispatch-chain
    // spines: the builder's splice-frame cap 8 -> 12 (the unset nest depth follows in chain trees only), the inline
    // resume budget 9 -> 13 on both the jit and VM (deopt_resume) sides, and a transitive chain link rule (a link whose
    // in-loop row keeps a call in its own tree is refused).
    E { group: Group::JIT, token: "ir-chain-deep-spine", on_key: Some("CRATONVM_JIT_IR_CHAIN_DEEP_SPINE"), off_key: None, off_word: Some("0"), since: "2026-10-28" },
    // Round rx27 wave 4 (lane chain): perf, opt-in "1" (default ON in the 2026-10-29 bundle flip, back to opt-in the same
    // day: cached +19 %, split-ws +14 % in the bundle), inert without ir-nested-chain-splice. A chain body entered
    // from a non-chain site (Matcher.match's matchRoot.match) is admitted and priced by the chain link rule.
    E { group: Group::JIT, token: "ir-chain-entry-splice", on_key: Some("CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE"), off_key: None, off_word: None, since: "2026-10-28" },
    // Round rx27 wave 4 (lane chain): perf, default ON since the 2026-10-29 bundle flip (kill "0"), inert without ir-nested-chain-splice. An interface site of a
    // chain body (a greedy node's predicate.is) may hold the nested two-class row.
    // Round rx27 wave 4 (lane chain): perf, default ON since the 2026-10-29 bundle flip (kill "0"). A nested call in a loop of its enclosing body is hot: the
    // nested hot cap, a nest-exact tree with a loop, and the planner's cold prune (Matcher.group in a spliced lambda).
    E { group: Group::JIT, token: "ir-nested-in-loop-hot", on_key: Some("CRATONVM_JIT_IR_NESTED_IN_LOOP_HOT"), off_key: None, off_word: Some("0"), since: "2026-10-28" },
    // Round rx27 wave 6 (lane chain): perf, opt-in "1". The splice's cold-arm proof (an unloaded new / type-check target
    // behind never-run branch edges) also cuts every proven edge together, so a target behind two such edges is cold
    // (Pattern.curly's new Curly).
    // Round rx27 wave 1 (lane chain): perf, opt-in "1". A top-level splice tree with rows under a loop whose append could
    // not hold them (the caller's budget ran out inside the loop) is rolled back: the loop-rooted callee stays a call,
    // compiled as its own root (W13-6's premise; the anewarray rule is one trigger of it).
    E { group: Group::JIT, token: "ir-loop-root-own-compile", on_key: Some("CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE"), off_key: None, off_word: Some("0"), since: "2026-10-28" },
    // Round rx27 wave 1 (lane chain): opt-in "1", inert without ir-loop-root-own-compile. Also counts an in-loop row a
    // nested strip of the caller's compile dropped as a loss (the matchonly reading of W13-6).
    E { group: Group::JIT, token: "ir-loop-root-own-compile-strips", on_key: Some("CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE_STRIPS"), off_key: None, off_word: Some("0"), since: "2026-10-28" },
    // JIT round 25 wave 10 (lane W10-5): fix, default ON, kill "0". A superseded lambda implementation body takes its
    // lambda-adapter thunks with it (caches holding them cleared, thunks retired), so callers stop entering the displaced
    // C1 body and its not-entrant stub; no thunk is built for a superseded body.
    // JIT round 23 wave 5b (lane L1): perf, default ON, kill "0". Back-edge phi coalescing admits a register shared with a phi
    // of another merge (an OSR body's entry-merge phi) when none of that phi's edge copies lands in the source's block.
    // JIT round 23 wave 5 (lane L1, TCL23-3): perf, default ON, kill "0". A loop-invariant operand's in-body checkcast to a
    // final class leaves behind a pre-header exact-class guard (loop-entry frame), so the read through it can be hoisted.
    // JIT r23 w2c close: perf, default ON, kill "0". The checkcast predicate at a loop spliced from a callee (the
    // loop-entry chain frame, the call site's chain withdrawal id), as the null-base predicate's chain rule.
    // JIT r23 w2c close: perf, default ON, kill "0". A second unroll round after the loop-entry checkcast / null-base
    // predicates, over the loops they emptied of their reads (main/post once the body is pure).
    // JIT r23 w2c close (item (B)): perf, default ON, kill "0". The main/post unroll of a counted loop whose body reads
    // fields / elements (nodes that can deopt), each copy with its own frames, its reads ordered on the memory chain.
    // JIT r23 w2c close: perf, default ON, kill "0". A checkcast on the hit edge of an instanceof of the same target
    // (javac pattern matching) is removed: the instanceof already proved it passes.
    // JIT r23 w2c close: perf, default ON, kill "0". The bounds-check offset rule proves an induction whose step is up
    // to 3 + k under its own `i < a.length - k` guard (a main/post main loop), not only up to 3.
    // JIT round 23 wave 8 (lane L7, TYPECHECK21-1 part C): perf, default ON, kill "0". A checkcast's readers read its
    // operand; the cast stays as a check (same helper, same ClassCastException) and stores no result on its hit path.
    // JIT round 23 wave 9 (lane L7): perf, opt-in "1" (neutral on n23-w9a/w9b: W2c cast 1.851/1.985/1.838 vs 1.801/
    // 2.783/1.733 ns). A forwarded checkcast's exact hit and null branch jump straight to the arm's join.
    // JIT round 23 wave 9 (lane L7): perf, opt-in "1" (neutral on n23-w9a/w9b: W2c nocast 1.498/1.769/1.312 vs 1.404/
    // 1.769/1.300 ns). An inline field read's compact arm falls through; legacy read and helper go to a cold tail.
    // JIT round 23 wave 9 (lane L7): perf, opt-in "1" (no win on n23-w9a/w9b). An inline field read whose receiver a
    // Guard(r != null) earlier in its block proved emits no null test (the null-base predicate's pre-header copies).
    // JIT round 23 wave 9 (lane L7): perf, opt-in "1" (neutral on n23-w9c: SegMono heapget 184/120/161 ns at =0, as
    // on). An unsound equal-depth sink placement takes back only its offending moves, not the whole method.
    // JIT round 23 wave 9 (lane L7): perf, opt-in "1" (neutral on n23-w9c; battery SAME both ways). A node that cannot
    // deopt (pure, trap-free, a direct getstatic) anchors no by-bci frame state, in the sink model and in bci_native.
    // JIT round 23 wave 10 (lane L7): perf, opt-in "1" (neutral to slightly worse on n23-w10a; battery SAME). A frame-named
    // single-use int/long defined in a loop, whose read the carry window cannot reach (31*h), gets a register.
    // JIT round 23 wave 10 (lane L7): perf, default ON, kill "0". A deopt frame names a loop value by register when every
    // other holder of that register lives and dies before the loop (phi homes drop at the back edge).
    // JIT round 23 wave 11 (lane L7): perf, opt-in "1" (neutral on n23-w11a). `k * x` with the constant first (javac's
    // `31 * h`) multiplies by the immediate (`imul eax, r, k`) instead of `mov eax,k; mov rcx,x; imul eax,ecx`.
    // JIT round 23 wave 8 (lane L7): perf, opt-in "1" (loss on n23-w8g: SegMono heapget 216/185/294 vs 199/111/196 ns/op;
    // the retired guards' compares stay in the loop). A counted loop's body Guard(c*k + d op X) -> one pre-header guard.
    // JIT round 23 wave 5 (lane L1, CF23-3): perf, DEFAULT ON since R23-W11-FLIP (n23-w11b CF family A/B). An unmasked spliced athrow becomes an always-failing guard through
    // the chain at its own pc (the interpreter throws in the callee's frame) instead of refusing the whole build.
    // JIT round 23 wave 4 (lane L1, CF23-1): perf, DEFAULT ON since JIT r24 (kill "0"; CfCpu pending 100 vs 113 ns/op). An
    // unbound statically bound instance call left in a spliced body takes the bound-site inline cache instead of refusing the
    // splice (or a covered try's whole tree).
    // JIT round 23 wave 4 (lane L1, CF23-2): perf. A spliced branch the callee's own profile never saw taken becomes a
    // chain-framed guard charged at the call site (unstable_if inside an inlined callee). JIT round 25 review: default ON
    // (the spliced feed's own switch; it was on by default through CRATONVM_JIT_IR_PRUNE_RARE_ARM already), kill switch "0".
    // JIT round 23 wave 9, lane W9-L2 (CF-W9-2): default ON (perf; kill switch "0"). The never-taken
    // prunes (own and spliced) also take an arm taken at most 1 in 100 000 over >= 2000
    // observations; also feeds CF23-2's spliced rows (jit `branch_edge_prunable`).
    // JIT round 23 wave 9, lane W9-L2 (CF-W9-3): perf, DEFAULT ON since JIT r24 with the CF handler family (kill "0";
    // neutral alone on n23-w9c, R21CfChain thenapply-done 81/57/70 on vs 78/65/217 off). A constant-VarHandle get/set on a fresh,
    // unpublished receiver of the plan's exact class is built as the field Load/Store with no call
    // (jit `IrBuilder::try_vh_const_fresh_field_access`).
    // JIT round 23 wave 4 (lane L1): perf, DEFAULT ON since JIT r24 (kill "0"; the late-armed client's catch is posted by
    // spliced-catch-report). The spliced-callee-handler gate refuses only for a JVMTI exception client armed at compile time,
    // not for the VM merely serving exception events from compiled code (which refused every compile).
    // JIT round 23 wave 3c (lane L1, L2W3-1): perf, default ON, kill "0". Spliced trees with callee handler rows are admitted on
    // the optimizing OSR route too (needs the opt-in CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS family).
    // JIT round 23 wave 3d (lane L1, proxy proposal 4e): perf, opt-in until an A/B. A statically bound instance self-call
    // (private/final/invokespecial of the method itself) stays in the optimizing tier as a receiver-carrying direct
    // self-call (receiver null-checked unless `this`) instead of bailing the whole method to single-pass.
    // r21 ea (wave 3). Correctness of a switch, default ON, kill "0" (the old split): EA21-3, the IR tier honours
    // `CRATONVM_DISABLE_SCALAR_REPLACEMENT` (its escape analysis offers no candidate, the box φ-web fold stays off).
    // r21 ea (wave 3): perf, opt-in. EA21-2: Integer/Long boxes carried through φs and read back only by
    // intValue()/longValue() become primitive φs; a deopt rebuilds a named box through the wrapper's valueOf (the
    // cached instance for a cached value).
    // r21 ea (wave 3): perf, opt-in. EA21-4: instanceof / a passing checkcast of a reference whose every definition
    // is an allocation of one class folds when it PASSES against that exact class (a miss never: stamp admission).
    // t1 wave 8 (lane EA, r9 EA review proposal 5): perf, opt-in. At the top of every escape-analysis round, `x == null`
    // / `x != null` of a reference whose every definition is a fresh new/newarray (a φ of them included) folds to its
    // constant, so a null check a previous round forwarded onto an allocation (`Optional.get()`'s `value == null`)
    // no longer escapes it globally.
    // t1 wave 5 (lane M, t1-V3-in-flight-compile-keeps-scalar-replacement-after-live-can-tag-objects): correctness, default
    // ON, kill "0". A live-phase can_tag_objects logs a JitCache invalidation record that refuses at publication every
    // body whose compile began before it (HotSpot's ciEnv::jvmti_state_changed); installed bodies stay.
    // r21 intrinsics (JIT round 21 wave 3, lane intrinsics; probe `R21IntrinsicCorners.java`,
    // book `docs/feature-designs/jit-ir-optimizations.md`).
    // Correctness, default ON, kill "0": neither tier answers StrictMath.floor / ceil / rint with ROUNDSD (its NaN
    // bits are not the JDK body's; HotSpot never intrinsifies the StrictMath three). jit `strictmath_rounding_exact_enabled`.
    // r21 intrinsics: correctness, default ON, kill "0". The Math/StrictMath floor / ceil natives return a NaN argument
    // unchanged and rint quiets it with the sign cleared, the JDK bodies' bits (native-builtins `math_rounding_nan_exact`).
    // r23 wave 11, lane L6: perf, default ON since n23-w11a, kill "0" (SplitParts list/cached/full win). The inline
    // reference arraycopy prefix also copies a range whose
    // destination component differs from the source's (SubList.toArray's Object[] -> String[]), proving each element
    // by an exact class-id compare; the first unproven element declines to the bound helper (jit
    // `ir_lower::ir_inline_ref_arraycopy_checked_enabled`).
    // r23 wave 12, lane L6: perf, OPT-IN "1" (neutral on n23-w12a). The optimizing tier's System.arraycopy prefix copies a byte[] /
    // boolean[] range between two different arrays inline (String.substring's Arrays.copyOfRange) instead of calling the
    // bound helper (jit `ir_lower::ir_inline_byte_arraycopy_enabled`).
    // r22 intrinsics (JIT round 22 wave 1, lane intrinsics; probe `R22IntrinsicsRows.java`,
    // books `docs/feature-designs/jit-ir-optimizations.md` and the r19-r21 strings / concat / vector books).
    // Perf, opt-in (INTRINSICS21-5): the single-pass Arrays.equals intrinsic compares 16 bytes per step (MOVDQU +
    // PCMPEQB + PMOVMSKB, an 8-byte step and a byte tail) instead of REP CMPSB (jit `arrays_equals_sse2_enabled`).
    // r22 intrinsics: perf, opt-in (INTRINSICS21-7). The optimizing tier lowers Math.round(D)J / round(F)I as a scalar
    // op (the JDK body's bit arithmetic, the d2l / f2i fallback) instead of a registered-native call (jit
    // `ir::ir_math_round_scalar_enabled`).
    // r22 intrinsics (wave 2): perf, opt-in (INTRINSICS22-4). The single-pass backend answers Math / StrictMath.round
    // inline, registered only where the optimizing tier's row (`ir-math-round-scalar`) claims it too (jit
    // `sp_math_round_enabled`).
    // r22 intrinsics (wave 2): perf, opt-in (L3W6-4). The single-pass backend answers Character.isHigh/LowSurrogate,
    // charCount, isBmpCodePoint, isValidCodePoint inline where the optimizing tier's rows claim them (jit
    // `sp_code_point_scalar_enabled`).
    // r22 intrinsics (wave 2): perf, opt-in (INTRINSICS22-5, first step). An integral Arrays.equals site no longer
    // refuses its method the optimizing tier; there it is an ordinary static call (jit `ir_arrays_equals_call_admits`).
    // r23 wave 2d, lane L8: perf, opt-in since r23 wave 3 (it lost 3-13x on n23-w2). A MemorySegment element accessor
    // site no longer refuses its whole method the optimizing tier; it stays a call (jit `ir_ffm_element_call_admits`).
    // r23 wave 3, lane L8: perf, opt-in since r23 wave 4 (A/B on n23-w3: segget 35.9 us/op on vs 1.55 us/op off).
    // A MemorySegment element accessor site is admitted to the
    // optimizing tier and bound to a judged thin helper serving the FFM element door's cached road (native and heap),
    // declining to the site's own dispatch (jit `ir_ffm_element_direct_entry`, vm `jit_ffm_element_{get,set}_direct`).
    // r23 wave 4 stage 1, lane L8: perf, opt-in (no consumer yet). The optimizing tier's guarded field reads gain a
    // 1-byte boolean and a reference read (UnboxOp::FieldBool / FieldRef; the reference read keeps the collector's
    // read barrier: live JIT_READ_BOUNDS at run time, refused while ZGC's is armed) (jit `ir_ffm_element_inline_enabled`).
    // It also turns on the VM's segment-field-slot answers and the filing of static final ValueLayout facts at
    // <clinit> (jit `ffm_element_inline`, vm `note_class_initialized_ffm_layout_facts`).
    // R23-W7-FLIP (JIT round 23 wave 7, orchestrator triage): default ON, kill "0" (A/B n23-w6 R23W4SegMono: 1900 -> 270-400 ns/op every row; R23W6SegModes/R23W4SegMono facts == HotSpot both modes).
    // r23 L8 close (L8-8): perf, default ON, kill "0". The FFM element inline expansion serves float/double accessors
    // too, moving the value through its raw bits around the scoped access (jit `ffm_element_inline_fp_enabled`).
    // JIT r23 wave 7 (lane L8, applied by L1): perf, default ON, kill "0". A second Op::Unbox on the same receiver node and
    // class in one block skips the null test and the exact-class guard (FFM element inline reads 3-4 segment fields per access).
    // JIT round 23 wave 8 (lane W8-L4): perf, default ON, kill "0". The FFM element inline expansion's segment-field
    // reads (final length / readOnly / scope / base / offset / min of a loop-invariant segment) are read once in the
    // loop's pre-header, deopting to the loop entry; withdrawn per header by the de-spec registry (jit
    // `ir_optimize::ffm_field_hoist`).
    // JIT round 23 wave 8b (lane W8-L4): perf, default ON, kill "0". The FFM element inline expansion's HEAP scoped access
    // skips the receiver / base null tests, the primitive-array screen and the payload bounds in ir_lower: the expansion's
    // exact-class-guarded field reads and segment bounds guards prove them (jit `Graph::ffm_proven_heap_accesses`).
    // JIT round 23 wave 9 (lane L4): perf (mixed on n23-w9a/w9b R23W4SegMono: heapget 143/159/144 on vs
    // 167/147/150 off, then 112/141/126 vs 130/140/112; nativeget 264/292/280 vs 242/282/218; won the n23-w13m triage A/B, see the R23-TRI-FLIP line). A heap segment's scope read, hoisted to a loop's
    // pre-header by ir-ffm-field-hoist, is proven a GlobalSession$HeapSession (never closes) by one loop-entry guard,
    // and the loop's scoped accesses on it emit no session check (jit `ir_optimize::ffm_field_hoist`,
    // `Graph::ffm_never_closing_scopes`).
    // R23-TRI-FLIP: default ON after the round-23 in-session triage A/B (n23-w13m), kill "0".
    // JIT round 23 close (the native rows of the r23-w2 MemorySegment page): perf, default ON, kill "0". In a loop that
    // runs no Java, a native segment's session is checked once in the pre-header (a never-closing class, or a confined
    // session this thread owns and that reads open) and the loop's scoped accesses on a proven one skip their check
    // (jit `ir_optimize::ffm_field_hoist::plant_session_prechecks`, `UnboxOp::SessionPrecheck`).
    // JIT round 23 wave 11 (lane L4): perf, default ON since R23-W13-FLIP, kill "0". A MemorySegment element site with no receiver observation at
    // all (its loop body never ran before the optimizing compile) is built as an always-failing UnstableIf guard
    // instead of refusing the method; the first arrival deopts once and the recompile plans the inline expansion
    // (HgZero: the method otherwise stays single-pass at ~45 ns per access) (jit `ir_ffm_element_unreached_site`).
    // JIT round 23 wave 13 (lane L4): perf, default ON (R23-W13-FLIP), kill "0" (needs ir-ffm-unreached-site-trap). After the unreached trap fired,
    // a recompile that still finds no receiver keeps the element site an inline-cache call with its profile seed armed,
    // instead of refusing the method to single-pass forever; the seed's recompile plans the inline expansion (jit
    // `ir_ffm_element_seed_call_site`).
    // t1 wave 6 (lane J6, FFM27-1): perf, opt-in "1" (needs ir-ffm-unreached-site-trap, ir-ffm-unreached-seed-call and
    // ir-ic-profile-seed). After the unreached trap fired, a recompile whose receiver row HAS receivers but gives no inline
    // plan keeps the element site an inline-cache call (seed armed when the row is thin) instead of refusing the method to
    // single-pass forever (--compatible R26SegVecPut native-put-add 2657 vs 258 ns) (jit
    // `ffm_element_inline::ffm_element_received_seed_call_site`).
    // t1 wave 7 (lane J6): pathology fix, opt-in until the host A/B (T1J6FfmSeedRows). An FFM seed-call site (unreached trap fired, no plan yet) arms
    // the inline-cache profile seed when it sits in a textual loop, even if the stale branch profile calls it cold; before,
    // a kernel first run with n = 0 kept a generic IC call into the JDK accessor for good (T1J6FfmSeedRows mono-native-put
    // ~33-70 us/op vs HotSpot 19-42 ns) (jit `ffm_element_inline::ffm_seed_call_site_arms_in_loop`).
    // JIT round 23 wave 12 (lane L7): perf, opt-in "1". Any other call-site intrinsic no execution has reached becomes
    // the same UnstableIf trap (deopt, reprofile, recompile; withdrawn after one charge) instead of refusing the method.
    // JIT round 23 wave 12 (lane L7): perf, default ON since the w2c close (2026-10-24), kill "0". The array-load hoist skips an in-body checkcast / instanceof
    // (writes nothing Java-visible, as ir-licm-typecheck-not-write argues), so W2c cast's h.tab[5] leaves the loop.
    // JIT round 23 wave 13 (lane L7): perf, default ON since the w2c close (2026-10-24), kill "0". Graph::direct_static_fields also lists the spliced bodies'
    // getstatics the lowering reads directly, so a spliced loop's pre-header statics no longer count as traps.
    // JIT round 23 wave 8b (lane W8-L4): perf, default ON, kill "0". getAtIndex / setAtIndex expansions test alignment on
    // the segment's start alone (idx * width adds no bit under the mask), so the guard is loop-invariant and leaves the loop
    // (jit `ir_ffm_index_align_invariant_enabled`).
    // JIT round 23 wave 8c (lane W8-L4): perf, OPT-IN "1" (lost the n23-w8g A/B: R23W4SegMono heapget 216/185/294 on vs
    // 178/171/272 off, heapput 166/163/258 vs 132/152/233). The scheduler's compare-to-branch sink also moves a
    // compare read only by a Guard into the guard's block, right before it, so ir_lower fuses them (CMP; Jcc) instead of
    // SETcc + spill in the loop header and reload + TEST at the guard (jit `ir_schedule::sink_compare_to_guard_enabled`).
    // r23 wave 2b, lane L5 (INTRINSICS22-5, inline-prefix half): perf, default ON, kill "0". The optimizing tier
    // answers an admitted integral Arrays.equals site inline (identity, null, length, the 16-byte compare loop) and
    // keeps it out of the splice (jit `ir_arrays_equals_prefix_enabled`).
    // r22 intrinsics (wave 2): perf, opt-in (INTRINSICS21-2). The single-pass String.indexOf(int) intrinsic admits a
    // variable needle (outside 0..=0xFFFF it deopts) and scans 16 bytes per step first (jit
    // `sp_string_index_of_char_enabled`).
    // r23 wave 1, lane L5 (INTRINSICS22-1): perf, default ON, kill "0". The single-pass backend answers
    // String.codePointAt(int) inline (LATIN1 byte, UTF16 unit or surrogate pair), declining to the call on any miss
    // (jit `sp_string_code_point_at_enabled`).
    // r23 wave 1, lane L5 (INTRINSICS22-1): perf, default ON, kill "0". The single-pass backend answers
    // StringBuilder.appendCodePoint(int) inline where nothing grows or inflates, declining to the call otherwise (jit
    // `sp_sb_append_code_point_enabled`).
    // r23 wave 2, lane L5 (R23W1L5-1): perf, default ON, kill "0". The single-pass appendCodePoint row grows a full
    // LATIN1 payload through the sb_grow stub and runs again (also needs CRATONVM_JIT_SB_GROW) (jit
    // `sp_sb_append_code_point_grow_enabled`).
    // r23 wave 2, lane L5 (R23W1L5-3): perf, default ON, kill "0". StringBuilder.codePointAt(int) as a declining
    // single-pass call-site intrinsic (jit `sp_sb_code_point_at_enabled`) and as an optimizing-tier prefix kind outside
    // the opt-in CRATONVM_JIT_IR_SB_PREFIX (jit `ir_sb_code_point_at_prefix_enabled`), which releases the row's pin.
    // r23 wave 2, lane L5 (R23W1L5-2): perf, default ON, kill "0". String.codePointBefore(int) / codePointCount(int,
    // int) as declining single-pass call-site intrinsics (the IR prefix bodies; jit
    // `sp_string_code_point_before_count_enabled`).
    // r22 intrinsics (wave 4): perf, opt-in. The single-pass String.equals intrinsic answers false for unequal coders
    // (the JDK body) instead of deopting per call (jit `sp_string_equals_coder_false_enabled`).
    // r22 runtime (wave 2): perf, opt-in (RUNTIME22-2). Both compile doors bind Class.getName() to the thin
    // jit_class_get_name_direct helper, which answers from the getName() memos with no dispatch funnel and declines
    // every miss to the generic dispatcher (jit `class_get_name_direct_enabled`).
    // r22 loops (JIT round 22 wave 1, lane loops; probe `R22LoopsRetire.java`, book
    // `docs/feature-designs/jit-ir-optimizations.md`). Perf, opt-in (LOOPS22-1, HOIST13-2's residual): a
    // counted `i < n` / `i > n` loop whose every carried value is linear (`c += k`, the promoted static / field cells
    // included) is replaced by its exit values and deleted (jit `ir_optimize::loop_retire::loop_retire_enabled`).
    // r22 loops: perf, opt-in (`jit-r10-loop-proposals.md` item 3). The IR range lattice gives `|`, `^` and `>>>` of an
    // operand that may be negative a sign-aware interval instead of top (jit `range_analysis::signed_bitwise_ranges_enabled`).
    // r22 loops (wave 2): perf, opt-in, inert without ir-loop-retire-accumulators. LOOPS22-3: also retire an inclusive
    // `i <= n` / `i >= n` loop behind a pre-header Guard(n != MAX/MIN) and an `i != n` loop
    // (jit `ir_optimize::loop_retire::guarded_bounds_enabled`).
    // r22 loops (wave 2): perf, opt-in, inert without ir-loop-retire-accumulators. LOOPS22-2: also retire a spliced
    // callee's loop, whose frames are chain snapshots (jit `ir_optimize::loop_retire::spliced_retire_enabled`).
    // r22 loops (wave 2): perf, opt-in. LOOPS22-4 / BCE21-4: the IR bounds-check elimination proves `a[(int) i]` for a
    // `long` induction under `i < (long) a.length` (jit `ir_check_elim::long_induction_bce_enabled`).
    // r22 loops (wave 2): perf, opt-in. nio21 L5W3-1: a passed Guard(idx < a.length) bounds `idx` for the accesses
    // after it, as an `If` edge does (jit `ir_check_elim::guard_upper_bounds_enabled`).
    // r24 loops (item C of `jit-r24-alloc-and-ffm-loop-residuals-proposals-RETIRED-20261026.md`): perf, opt-in. Fully unroll a
    // constant-trip counted loop whose body branches (an inner diamond the full unroller's single-block shape
    // refuses); each copy's branch folds on its constant induction value (jit
    // `ir_optimize::cfg_unroll::cfg_unroll_enabled`).
    // r24 loops (item B3 of `jit-r24-alloc-and-ffm-loop-residuals-proposals-RETIRED-20261026.md`): perf, opt-in. An optimizing splice
    // admits a callee with a MemorySegment element access and expands it inline from the callee's own receiver row
    // (jit `ir_splice_ffm_element_enabled`; read by the VM's inline resolver too).
    // r24 loops (item B2 of `jit-r24-alloc-and-ffm-loop-residuals-proposals-RETIRED-20261026.md`): perf, opt-in. A native MemorySegment
    // loop behind a session pre-check resolves each access's arena handle once at the loop entry; each access then
    // compares the slot's key and reads (jit `ir_optimize::ffm_arena_cache::ffm_arena_cache_enabled`).
    // t1 wave 8 (lane FFM27, FFM27-2): perf, opt-in "1" (needs ir-ffm-element-bimorphic). After an element site's OWN
    // unreached trap fired (the only speculation withdrawn at its bci), a row naming exactly two segment classes plans the
    // bimorphic expansion before the one-class rules; before, any de-spec at the bci skipped it, so a heap / native kernel
    // first compiled before its loop ran deopted on every call of one class (T1J6FfmSeedRows bi-mixed-put ~34 us/op)
    // (jit `ir_ffm_element_bimorphic_plan_after_trap`).
    // t1 wave 8 (lane FFM27, FFM27-2): perf, default ON, "0" kill. Loop versioning
    // on an invariant receiver class test splits a bimorphic MemorySegment element
    // loop into two mono loops that the vector planner can vectorize (jit
    // `ir_optimize::class_unswitch::ffm_class_unswitch_enabled`).
    // t1 wave 9 (lane JITA, t1-FFM27 withdrawn-guards page): policy, default ON, "0" kill. The one-class element plan stands
    // down at a bci where the VM withdrew one of its guards (anything but the site's own unreached trap), so a site that
    // keeps failing a bounds / read-only / null / class guard stops trading a deopt and a recompile per trap until the
    // method is made not compilable (HotSpot: too_many_traps recompiles without the speculation)
    // (jit `ffm_element_inline::ffm_element_withdrawn_guards_enabled`).
    // r23 wave 2, lane L8 (BCE21-5): perf, default ON, kill "0". Range BCE reads `i != a.length` as `i < a.length`
    // on its differ edge for an induction entering at 0 and stepping by exactly 1 through that edge (jit
    // `ir_check_elim::bce_ne_exit_enabled`).
    // r23 L8 close (L8-5, BCE21-5 proper): perf, default ON, kill "0". `for (j = c; j != a.length; j++)` with a
    // constant c >= 1: the range-predicate pass plants `c <= a.length` in the pre-header and range BCE reads the
    // `!=` exit as `j < a.length` under it (jit `ir_check_elim::bce_ne_exit_start_enabled`).
    // t1 wave 6, lane B6 (BCE21-5 residual): perf, opt-in. `for (j = s; j != a.length; j++)` with a non-constant
    // s: the range-predicate pass plants `s <= a.length` and `0 <= s` in the pre-header and range BCE reads the `!=`
    // exit as `0 <= j < a.length` under them (jit `ir_check_elim::bce_ne_exit_var_start_enabled`).
    // r22 loops (wave 6b, default flipped OFF in wave 7): opt-in. Both tiers put value2 in the SSE destination of
    // fadd / dadd / fmul / dmul, so two NaN operands answer value2's NaN -- HotSpot 25's INTERPRETER rule. HotSpot C2
    // answers value1's NaN (the natural order, this switch off), so compiled code stays on the C2 rule by default
    // (jit `fp_nan_operand_order_enabled`; page `docs/feature-designs/real-jdk-mode.md`).
    // r21 concat (CONCAT21-1): perf, default ON since the r23 A/B (R23-AB-FLIP), kill "0". Both tiers' inline concat of a LATIN1 site builds a run-time UTF16
    // operand (UTF16 String, char above 0xFF) with the plan's wide twin inline instead of declining to the bridge
    // (jit `concat_inline_plan::concat_coder_promote_enabled`).
    // r21 concat (wave 5, CONCAT21-2): perf, default ON since the r22 A/B, kill "0". The compiled concat bridge renders a UTF16 result whose operands
    // need no toString() as units into one TLAB String instead of the general renderer (vm `concat_latin1::utf16_fast_enabled`).
    // r21 concat (wave 5, CONCAT21-3): perf, default ON since the r22 A/B, kill "0". Both tiers' inline concat admits float / double operands, rendered
    // by a leaf call to the VM's shared Float/Double.toString formatter (jit `concat_inline_plan::concat_fp_inline_enabled`).
    // r21 concat (wave 5, CONCAT21-4): perf, opt-in. The single-pass String.valueOf(Object) prefix passes an exact Integer
    // through to the inline concat it provably feeds, which renders the int (jit `op_concat::concat_value_of_fusion_enabled`).
    // r21 compat (JIT round 21 wave 4, lane compat; book `docs/feature-designs/jit-ir-optimizations.md`,
    // probe `R21CompatDispatchRows.java`). Correctness, default ON, kill "0" (the 2026-09-22
    // mirror alone): COMPAT21-1, the thin direct-helper gate (bind and call halves) asks the resolver the generic tail
    // asks, so a Bridge in front of loaded concrete bytecode is refused where the tail yields it (vm
    // `thin_helper_asks_resolver_enabled`).
    // r21 compat: perf, default ON since the r23 A/B (R23-AB-FLIP), kill "0". COMPAT21-3, `registered_native_will_run` (the JIT compile gates) answers "does not
    // run" for a non-Intrinsic registration resolve_dispatch yields to loaded bytecode (vm
    // `will_run_asks_resolver_enabled`).
    // r21 compat: perf, opt-in. COMPAT21-4, a force_native_over_real_jdk_bytecode triple seals a caller only while
    // the force interceptor would run its native, i.e. an Intrinsic (vm `forced_seal_asks_resolver_enabled`).
    // r21 codecache (JIT round 21 wave 4, lane codecache; book `docs/feature-designs/jit-ir-optimizations.md`,
    // probe `R21CodeCacheRows.java`).
    // Correctness, default ON, kill "0": a body whose mapping `ExecutableBuffer::drop` rescues into the retirement
    // queue (a cascaded or stray last-owner release) keeps the side tables its code reads by address (MIC/PIC slots,
    // invoke infos, deopt boxes, callee roots, the not-entrant record) queued with it. jit `rescue_keeps_side_tables_enabled`.
    // r21 codecache: correctness, default ON, kill "0". A method whose optimizing body a dependency invalidation
    // or a trap eviction withdrew 400 times (never a flush, eviction or unload sweep) stops being recompiled at C2
    // and stays C1, HotSpot's PerMethodRecompilationCutoff. jit `withdrawal_recompile_cutoff_enabled`.
    // r21 codecache (wave 4 follow-up, CODECACHE21-1): correctness, default ON, kill "0". A dying body withdraws its
    // MIC / PIC / cycle-cell owners (slot word zeroed first) and releases the last ones through the retirement queue,
    // so a callee is never freed as a cascade of the caller's proof. jit `ic_owners_through_queue_enabled`.
    // r22 sync (JIT round 22 wave 1, lane sync; book `docs/feature-designs/jit-ir-optimizations.md`).
    // Correctness, default ON, kill "0" (SYNC22-1): when the retirement queue releases a rescued body's side tables,
    // an inline-cache owner a thread still inside that body published after the body's drop is withdrawn again and
    // released through the queue instead of as a field drop (jit `rescued_tables_rewithdraw_enabled`).
    // r23 wave 1, lane L6 (SYNC22-3 of `jit-r22-sync-proposals.md`): HotSpot parity, default ON, kill "0". Under
    // `--compatible` a thin direct helper is not bound for a triple the native registry holds no row for (a
    // COMPAT_FAMILIES row unregistered it, or it never was): the interpreter and the generic tail run the JDK's
    // bytecode there, so compiled code does too. Asked by the jit's `direct_native_helper` in both modes now, and by
    // the OSR door and the splice keep-bind questions (vm `jit::helpers::compat_bind_asks_registry_enabled`, read
    // once per VM as `SharedVm::jit_compat_bind_asks_registry`).
    // r23 wave 1, lane L6 (REVIEW22-2 of `jit-r22-review-proposals.md`): perf, default ON, kill "0". The `--compatible`
    // getSimpleName / getCanonicalName bridges memoize, per VM and class id, that their ICCE / NCDFE pre-checks passed
    // (per bridge and user-loader state; cleared by a redefinition, pruned on unload), and skip them afterwards
    // (native-builtins `lang_class::name_bridge_verdict_probe`, read once per VM into `ReflectDoorCaches`).
    // r22 sync (wave 2, nio21 w7-L1 P2): correctness, default ON, kill "0". A CAS whose slot read is `Uninitialized`
    // compares as the slot's typed default (zero / null), so a CAS loop over such a slot can land
    // (vm `vm_exec::values_equal_for_cas`).
    // r23 wave 2, lane L6 (page `r23-w2-review2-unsafe-static-rmw-takes-class-monitor-FIXED-20261021.md`): HotSpot parity,
    // default ON, kill "0". The static read-modify-write fallback (`VarHandle` statics, `Unsafe` static RMWs,
    // `synthetic_cas`) holds a per-VM, VM-private lock per class instead of the class mirror's Java monitor, so it never
    // blocks on, owns, or shows as `Foo.class`'s monitor (native-builtins `lang_class::static_rmw_lock`, read once per VM).
    // r21 codecache (CODECACHE21-2): correctness, default ON, kill "0" (the `strong_count > 1` shortcut). Each
    // artifact counts its live `RetainedCode` handles; the handle that takes the count to zero routes through the
    // queue. Read once per artifact at construction. jit `retained_holder_count_enabled`.
    // r21 codecache (CODECACHE21-4): correctness, default ON, kill "0". A withdrawn optimizing OSR body is charged to
    // the same per-method recompile cutoff (no tier change). jit `JitCache::report_osr_withdrawn`.
    // r21 monitors (wave 4 follow-up): correctness, default ON, kill "0". With the balance gate on, an instruction that
    // can raise with a block monitor held and no covering catch-all refuses the method too (HotSpot's full rule).
    // jit `x64::deopt_stubs::monitor_balance_traps_enabled`.
    // r21 monitors: correctness, default ON, kill "0". With the balance gate on, a monitorexit whose `aload k` operand is
    // none of the locals known to hold the innermost open monitor's object refuses the method.
    // jit `x64::deopt_stubs::monitor_balance_operands_enabled`.
    // r21 monitors: correctness, default ON, kill "0". An interpreted monitorexit of an object its frame's record does not
    // name throws IllegalMonitorStateException (no release) when the record already names every block monitor the
    // frame's own code holds at that pc: the hold is another frame's, e.g. a compiled caller's.
    // vm `runtime::interpreter::held_monitors::frame_record_is_complete`.
    // r21 monitors (MONITORS21-5): perf, default ON (gates passed 2026-11-05). The compiled inline monitor spin leaves the one-CPU decision to the
    // monitor's zero run-time budget instead of probing the CPU count per compiled site.
    // jit `runtime_lowering::inline_spin_runtime_cpu_gate_enabled`.
    // r21 ic (JIT round 21 wave 4, lane ic; book `docs/feature-designs/jit-ir-optimizations.md`, probe
    // `R21DispatchRows.java`). Perf, opt-in. IC21-1: a receiver's shared-table cell array that a
    // deeper column (an invokeinterface site's first-receiver slot) does not reach is replaced by a larger copy instead
    // of refusing the cell for the class's life (census row `mic_cell_past_array`). jit `mega_class_row_grow_enabled`.
    // r21 ic: perf, opt-in, only with CRATONVM_JIT_MEGA_VTABLE_COLUMN on. IC21-2: an invokevirtual site the VM binds
    // to its shared-table selector and column at compile time (caller and owner in one runtime package, the method
    // accessible) bakes the directory root, selector and column into its vtable-column probe instead of loading them
    // from the slot. jit `runtime_lowering::mega_vtable_column_baked_enabled`, vm `bake_mega_virtual_site`.
    // r21 ic: perf, opt-in. IC21-3: invokeinterface sites bind per-selector columns from an interface range of their
    // own (64 + a per-table index, wrapped at CRATONVM_JIT_MEGA_IFACE_COLUMN_SPACE_WIDTH since t1 w8) instead of a receiver's vtable slot, so every receiver of one interface
    // selector keeps its shared-table cell at one column; turns CRATONVM_JIT_MEGA_CLASS_ROW_GROW on with it.
    // jit `MegaDispatchTable::interface_space_column`.
    // t1 w8 IC: a number, read only with CRATONVM_JIT_MEGA_IFACE_COLUMN_SPACE on (default 64; 0 = unbounded, the r21
    // wave-4 dense range). IC21-3's interface range wraps at this many columns, so a receiver's array never needs more
    // than 64 + width cells for it and a few hundred interface selectors no longer spend the shared column budget
    // (refusing virtual columns too). Two selectors on one column cost a receiver reached through both one cell
    // (`cell_taken`), never an answer. jit `inline_cache_pic::mega_iface_column_space_width`.
    // r21 ic: correctness, default ON, kill "0" (each VM's init stores its answer into the process gate). The receiver
    // / call-site recording gate is per VM (`ProfileStore::receivers_recording`, asked by the interpreter's invoke
    // doors), and the process gate only ever goes ON, so a VM with no profile consumer no longer stops another VM's
    // recording. jit `profile::receivers_per_vm_enabled`.
    // r22 inline (JIT round 22 wave 1, lane inline; probe `R22InlineRows.java`). Perf, opt-in.
    // `jit-r10-tier2-proposals.md` section 1: the OSR pipeline answers to its own retry budget (`osr_fail_count`)
    // only, so three method-entry compile failures no longer close the OSR door of a method whose OSR compile was
    // never tried. jit `tiered::osr_own_fail_budget_enabled` (via `osr_out_of_retries`).
    // r22 inline: correctness of the planner's analysis, default ON, kill "0" (the byte-only read). The IR splice
    // order's factory-producer hoist reads a site's receiver local only from an `aload` that begins an instruction
    // (`bipush 42; invokevirtual` no longer reads as `aload_0`), and so does the planner's factory-receiver class
    // (`ir_factory_receiver_class`: `getfield #42; invoke..()` no longer plans an exact site the builder cannot
    // prove). jit `ir_splice_order::ir_splice_order_insn_start_enabled`.
    // r22 inline: perf, opt-in (churn-loop page, hand-off J2). An unbound single-pass `invokespecial <init>` site
    // (method entry and the OSR door) takes the bound-site inline cache instead of `jit_invoke_dispatch` for the
    // body's life; elided constructors and `Object.<init>` excluded. Since wave 2 also the optimizing tier's bound-site
    // IC (`ir_bound_site_ic_planned`, w15-L1 P3). jit `sp_bound_site_ic_ctor_enabled`.
    // r22 inline (wave 2): perf, opt-in. INLINE22-1 / SPLICE21-2: a C2 task that published its single-pass
    // fall-through after the acceptance gate refused the optimizing body arms one re-offer; the entry-count drain
    // fires it at 4x the count at the refusal (floor 20000, at most 2 per method) and grants the IR refusal memo one
    // re-judgement. jit `tiered::acceptance_reoffer_enabled`.
    // r23 wave 9, lane L6: perf, opt-in (=1) since the n23-w9c A/B (mixed: full lost 3 of 3). A method-entry C2 body that compiled and was refused at
    // publication (baked-callee-retired, a moved dependency) hands its tier_fail_count charge back, up to 6 times per
    // method, so a large body whose callees republish while it compiles (Pattern.compile()V) still reaches C2
    // (jit `tiered::c2_publication_refusal_refund_enabled`).
    // JIT r25, lane regex rows: perf; default ON since R25-FLIP, `=0` kills. JitCache::prepare_for_publication no longer refuses a body whose baked
    // direct-call target was retired by the supersede redirect alone (superseded, not entrant, withdrawn by no logged
    // invalidation): the body publishes rooting it and reaches the new body through its forwarding stub, as every
    // published caller that baked it does. The cause of Pattern.compile()V's refused C2 publications (jit
    // `publish_over_superseded_callee_enabled`).
    // JIT r25 wave 2, lane regex rows: perf; default ON since R25-FLIP, `=0` kills; meant with publish-over-superseded-callee. The optimizing tier pins
    // every direct-call target it baked during planning from the graph build to its own publication, so a callee
    // superseded (and otherwise unrooted) during a long compile is not freed before `put` can root it (jit
    // `pin_baked_callees_while_compiling_enabled`).
    // r22 inline (wave 8): correctness / parity, default ON, kill "0" (the blanket veto). A first-arrival replay at
    // an own non-invoke bci ignores the artifact's impure-splice flag (no spliced body can have run: splices sit at
    // invokes, which the first-arrival walk counts as commits); both the IR trap planter and the VM sinks ask it.
    // jit `deopt::replay_own_site_ignores_splice_purity`.
    // r21 bce (JIT round 21 wave 4, lane bce; book `docs/feature-designs/jit-ir-optimizations.md`, probe
    // `R21BceRows.java`). Perf, opt-in. BCE21-2: the IR tier's constant-offset predicates --
    // `x <= a.length - c` for `a[i + c]` (offset predicate) and `x <= a.length` for `a[i - c]`, c <= the entry value
    // (range predicate), both deopt-only -- and `ir_check_elim`'s `a[i - c]` arm. jit
    // `ir_check_elim::const_offset_predicate_enabled`.
    // r21 bce: perf, opt-in. BCE21-1: a single-pass counted loop whose exit test reads `a.length` inline takes guarded
    // elisions for other arrays (pre-header loads `a`, null deopts, compares lengths). jit
    // `x64::bce::inline_length_bce_enabled`.
    // r21 bce: perf, opt-in. BCE21-3: a single-pass down count takes guarded elisions against the IV's entry value
    // (`length > iv_entry`, deopt otherwise). jit `x64::bce::down_count_bce_enabled`.
    // t1 wave 8 (lane BCE): perf, opt-in. BCE21-4's single-pass arm: `a[(int) i]` under `for (long i = 0; i <
    // a.length; i++)` is proved statically (no guard). jit `x64::bce::long_induction_bce_enabled`; probe
    // `T1W8BceRows.java`.
    // zr z3/ja: on ZGC a compiled `anewarray` (`new Object[n]`) bumps the TLAB inline in both tiers, the component class as its class word, `jit_anewarray_object` as the slow path. Default ON, kill "0".
    // zr z7/ca: the optimizing tier's inline ZGC allocation (new / newarray / anewarray): the inline start-bit arm skips the post-call frame republish and jumps to the join (value-identical: no call was made). Default ON, kill "0".
    // zr z11/ia: the LONG inline ZGC start-bit store (both tiers) tests its word's ownership against the buffer's precomputed owned-word window (`Tlab::zgc_owned_words`) instead of re-deriving the granule; value-identical. Default ON, kill "0".
    // zr z11/ia: the LONG inline ZGC start-bit store drops its never-taken 8-byte-grid test (8-aligned bumps; the table publishes only for an 8-aligned base). Default ON, kill "0".
    // zr z11/ia: both tiers' inline ZGC allocation keeps the bump's end in RAX across the start-bit store (table in R9) and commits it with one store instead of re-deriving it; both announce forms. Value-identical. Default ON, kill "0".
    // zr s1/ca: the LONG inline ZGC start-bit store (both tiers), with the owned-word window, drops its `base <= obj` and `obj - base < span` tests, which the window implies for every armed buffer. Default ON, kill "0".
    // rx27 wave 3b (lane alloc): perf, opt-in. The inline ZGC start-bit store gates on `short_blocked` (implies `blocked == 0`); the IR prefixes' pre-checked pair (substring byte[]+String, toString, char[] copies) takes the owned-word window and no span / grid / `and 63`; the IR `new` layout guard is one `mov eax, [moffs64]`. Same bits, same conditions.
    // rx27 wave 3b (lane alloc): perf, opt-in. The inline ZGC start bit is set by one `bts qword [words], rcx` (long store with the owned window; the lean pre-checked store). Same word and bit; A/B by instructions and by time (memory BTS is microcoded).
    // zr s1/ca: the optimizing tier's inline ZGC new / newarray / anewarray: the inline start-bit arm (no call made) lands past the frame-identity store and the zero test only the stub owes, at sites whose shadow reload is empty. Default ON, kill "0".
    // zr s1/ca: the optimizing tier's inline ZGC newarray / anewarray with a constant length bakes the size (no length load, screen or size arithmetic). ZGC only. Default ON, kill "0".
    // r23 wave 8, lane L8: perf, OPT-IN "1" (n23-w8b A/B neutral: Gen long16 27.5-29.3 vs 27.0-30.3 ns off, objarr16
    // 27.8-30.9 vs 29.0-32.3). The constant-length newarray / anewarray bake also on Generational and G1 (no announce):
    // no length load, screen or size arithmetic for new long[16] / new Object[16] (jit
    // `ir_newarray_const_length_any_gc_enabled`).
    // JIT round 23 wave 11 (lane L3): perf, default ON, kill "0". Off ZGC (Generational, G1) the optimizing tier's
    // inline newarray / anewarray fast path jumps past the frame-identity republish and the null test only the stub owes
    // (jit `runtime_lowering::ir_newarray_fast_join_any_gc_enabled`, `ir_lower` `gen_newarray_fast_join_admitted`).
    // JIT round 23 wave 11 (lane L3): perf, default ON, kill "0". zgc-tlab-align-test's cursor test (TEST R11B, 7; JNE
    // slow, instead of ADD/AND on the loop-carried cursor) on the Generational and G1 bumps too; the ZGC key stays the
    // master (jit `runtime_lowering::ir_tlab_align_test_any_gc_enabled`).
    // zs s6/ca3: the optimizing tier's inline ZGC bump (new, newarray/anewarray, box prefixes) tests the loaded TLAB cursor for 8-alignment (TEST R11B, 7; JNE slow) instead of aligning it (ADD R11, 7; AND R11, -8), taking two ALU operations off the cursor's loop-carried chain. An unaligned cursor takes the slow path. ZGC only. Default ON, kill "0".
    // zs s9/ca4: the optimizing tier's inline ZGC new / newarray / anewarray is split: the fast half (bump, header, inline start-bit store) stays at the site and its inline arm falls through to the continuation; the announce helper arm, the stub, the identity republish, the null test and the exception exit go after the method body. One taken branch fewer per compiled allocation. ZGC only. Opt-in "1" (default OFF until the host A/B).
    // zs s10/ca5: after the optimizing tier's inline ZGC bump passes its end test, N (1..4) PREFETCHW of the lines the next allocations will write, at the distance below (HotSpot's AllocatePrefetch shape). ZGC only. Default 0 (off).
    // zs s10/ca5: the byte distance of CRATONVM_JIT_ZGC_ALLOC_PREFETCH's first line past the new cursor (0..4096, default 256).
    // zt t2/ca: the optimizing tier's inline ZGC newarray/anewarray with a run-time length computes its footprint from the length alone (LEA RDX, [RCX*2^log2 + 23]; AND) and bumps with a two-component LEA RAX, [R11 + RDX], off the cursor's loop-carried chain (the old three-component LEA is the slow LEA on Intel SNB..SKL). 2-3 instructions fewer. ZGC only. Opt-in "1".
    // zt t2/ca: the line count of CRATONVM_JIT_ZGC_ALLOC_PREFETCH for an inline `new` only (HotSpot's AllocateInstancePrefetchLines beside AllocatePrefetchLines). Unset = the array count; 0..4.
    // zs s3/bt: the optimizing tier's gated reference putfield/aastore on ZGC's age-floor plan opens with one 16-bit test of the adjacent pre/post gate bytes and stores at once when both are zero (the full gate sequence runs unchanged behind it). ZGC only. Default ON, kill "0".
    // zs s5/bt2: the same fused gate at the remaining store sites: the single-pass putfield / aastore and the constant-VarHandle / Unsafe reference-store gate screen of both tiers. ZGC only; needs CRATONVM_JIT_ZGC_FUSED_REF_STORE_GATE. Default ON, kill "0".
    // zt t9/ca (Q4 of the zs5bt2 store-gate proposal): while a mark runs, the single-pass aastore on ZGC's age-floor plan skips the satb_pre_write_barrier call for a NULL old element (TEST RAX, RAX; JZ after the old-element load); the helper returns at once on a null old value, so value-identical. ZGC only. Opt-in (default OFF, "1"); census `sp aastore satb-pre executions: entered= called=` under CRATONVM_DBG_SP_REF_STORE_TRACE=1 + CRATONVM_DBG_JIT_METHOD_STATS=1.
    // zt t10/ca2 (R1 of the zt9ca residuals page): a compiled ZGC `new` of a class that still needs jit_post_tlab_init (a finalizer or java.lang.ref superclass, a legacy-layout field owing a typed default) registers the object BEFORE the TLAB cursor commit (inline start bit, else zgc_note_tlab_object uncommitted), then calls jit_post_tlab_init after the commit with bit 30 of the field count set (no second registration). Both tiers; ZGC only. Opt-in (default OFF, "1"); off whenever CRATONVM_ZGC_JIT_HELPER_BEFORE_COMMIT=0 or its parent CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT=0.
    // zt t11/ca3 (proposal zt4ca2): the single-pass inline `new` of a post-init no-op class on ZGC sinks its blind spill and shadow push onto the slow edge, so the fast path makes no call. zt t12/ca4: the announce arm's refusals call zgc_note_tlab_object on their own out-of-line edge after storing the withheld registers (t11's divert to new_object lost every CPU bar); opt-in until the t12-ca4 rows decide it.
    // zt t7/tg (2026-10-19, GN-8): the single-pass aastore's gated post barrier on ZGC's age-floor plan passes the element's slot (tagged with bit 0) to write_barrier, so a compiled store into a large old reference array cards the element's granule under CRATONVM_ZGC_GEN_ELEMENT_CARDS=1. Opt-in ("1"); deciding row E1 of z2-g2-report.md with the JIT on.
    // Optimizing tier (round 16 lane alloc): a `new` whose only escape is one
    // edge of the branch ending its block is re-created on that edge and its
    // loads forwarded (`ea_ir_bridge::ir_sink_allocations_to_escaping_edge`).
    // Opt-in (default OFF).
    // Single-pass and IR tiers: bind plain `jdk/internal/misc/Unsafe` scalar
    // accessor sites to the thin direct helpers (`jit::unsafe_accessor_direct_enabled`,
    // 1354647ae). DEFAULT-ON (`runtime_flag_default_on`); `0` sends every site
    // back through the generic native funnel. Declared in round 11 wave 5 (it
    // was read but undeclared, so it answered only to a live `getenv`).
    // Emit side of the register oop maps: `OopMapEntry::reg_oop_mask`.
    E { group: Group::JIT, token: "native-matcher-find", on_key: Some("CRATONVM_NATIVE_MATCHER_FIND"), off_key: None, off_word: Some("0"), since: "2026-07-11" },
    // r23 wave 2c, lane L5: perf, default ON, kill "0" (restores the natives). The real-layout Matcher find / start /
    // end / group / hitEnd / requireEnd natives yield to the JDK bytecode, which the JIT compiles and splices; folded
    // into `natives.native_matcher_find` and vm `env_cache::native_matcher_find`.
    E { group: Group::JIT, token: "regex-matcher-bytecode", on_key: Some("CRATONVM_REGEX_MATCHER_BYTECODE"), off_key: None, off_word: Some("0"), since: "2026-10-21" },
    E { group: Group::JIT, token: "native-pbe-keyfactory", on_key: Some("CRATONVM_NATIVE_PBE_KEYFACTORY"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "native-string-regex", on_key: Some("CRATONVM_NATIVE_STRING_REGEX"), off_key: None, off_word: Some("0"), since: "2026-06-22" },
    E { group: Group::JIT, token: "osr", on_key: Some("CRATONVM_JIT_OSR"), off_key: None, off_word: None, since: "2026-06-23" },
    // Default-ON (RBC.6 lift): `env_cache::osr_athrow_allowed` answers `true`
    // for `Err(_)` and reads `0`/`false` as the kill switch.
    // Default-ON: `jit::osr_dead_local_entry_allowed` answers `true` for
    // `Err(_)` and reads `0`/`off`/`false`/`no` as the kill switch.
    // Declared 2026-08-19. An OPT-OUT: publishing a per-bci-`Ambiguous` local
    // as `Undefined` rather than `Unsupported` is the default, and this key
    // restores the re-run encoding. See
    // `jit/src/x64/deopt_stubs.rs::osr_ambiguous_dead_enabled`.
    // Declared 2026-08-19 beside `osr-ambiguous-dead`. Also an OPT-OUT: a
    // per-bci-`Ref` local at a bci where the oop mask has no opinion is
    // published as a reference by default. See
    // `jit/src/x64/deopt_stubs.rs::osr_refined_ref_enabled`.
    // Declared 2026-08-24. An OPT-OUT: the optimizing tier republishes the
    // innermost-frame mirror after an inline-cache hit by default, and this key
    // restores the stale-mirror behaviour so one binary has both arms. See
    // `ir_lower::emit_call_loaded_ic_entry`.
    // Declared 2026-08-26. An OPT-OUT: the staged invoke-argument buffer is
    // published on the SHADOW stack by default, not merely named in the oop
    // map. The band verifier consults only the shadow stack. See
    // `x64::safepoint::collect_live_oop_homes`.
    // JIT r20 wave 11, lane pAlloc (book jit-r21-nio-proposals-w14-L4.md, W14L4-3): the IR instanceof / checkcast answer a subclass or implementor of the target from the supers displays before the helper; a miss or an unpublished display still calls it. Default ON, "0" keeps the exact-class / seen-way arms alone.
    // JIT r20 wave 17, lane pAlloc2 (handoff r20 "Round-21 performance targets", R20PAllocTypecheck): the IR instanceof answers a display MISS inline too, for a receiver whose display carries the hierarchy-only verdict, a target outside the synthetic-implements table and a clear duplicate-name latch. Default ON, "0" keeps the hit-only arm.
    // JIT round 21, lane typecheck (page jr21-synthetic-implements-over-admits-real-classes-20261014; re-landed t1 w8 RL-TC after dev merge 624fae0b49 dropped it): the IR instanceof display MISS arm serves synthetic-implements targets (List, Collection, Iterable, Comparable, ...) too, now that synthetic_implements declines every receiver whose supertype closure is hierarchy-only. Default ON (r21 host A/B neutral / instanceof-collection-mix 760 vs 7278), "0" restores the target screen.
    // JIT r23 w2c close: perf, default ON, kill "0". An IR `instanceof` remembers per site and VM the last two receiver
    // classes it proved a miss for inline (display verdict, latch down) and answers them with two compares.
    // JIT round 21, lane tcperf (page jr21-tcperf-class-target-typecheck-cost-FIXED-20261015; re-landed t1 w8 RL-TC): with typecheck-rows on, the IR instanceof / checkcast against a class at depth 1..7 tests the receiver's type-check row FIRST in the status-first form (status word, then ONE compare of the chain word; hierarchy-only misses answered), before the exact-id compare, the seen ways and the display arm. Default ON (r21 host A/B: instanceof-deep 164 vs 226), "0" restores the rows' later placement; inert while typecheck-rows is off.
    // JIT round 21, lane tcperf (re-landed t1 w8 RL-TC): the same status-first row arm for an INTERFACE target (home-slot hit; instanceof also the bloom-proved miss); checkcast takes the hit half. Default ON, "0" restores the rows' later placement; inert while typecheck-rows is off.
    // JIT r25, lane regex rows: perf; default ON since R25-FLIP, `=0` kills. The IR instanceof answers a lambda-proxy receiver (class id >= 0x8000_0000)
    // from two per-target ways the helper fills (the last proxy answered 1, the last answered 0 behind the
    // duplicate-name latch) instead of calling jit_instanceof every time (jit `ir_lower::ir_instanceof_proxy_ways_enabled`).
    // Round rx27, lane compile: perf, opt-in. The lambda-proxy ways keep two proven-1 proxies per target (a helper 1
    // moves the hit way's proxy into a second hit way), so a site alternating between two passing proxies stops calling
    // jit_instanceof on every change (jit `ir_lower::ir_proxy_two_hit_ways_enabled`).
    // Round rx27, lane compile: perf, opt-in. The IR checkcast passes a lambda-proxy receiver found in its target's proxy
    // hit ways inline (the cell the target's instanceofs use) and fills them after a passing jit_checkcast call
    // (jit `ir_lower::ir_checkcast_proxy_ways_enabled`).
    // Round rx27 wave 3, lane compile: perf, opt-in. The single-pass instanceof answers a lambda-proxy receiver from
    // per-target proxy ways the body owns (hit ways, and the miss way behind the duplicate-name latch), filled after
    // jit_instanceof; the optimizing tier's ir-instanceof-proxy-ways for single-pass bodies (jit x64 `op_object.rs`).
    // r21 typecheck (JIT round 21 wave 1, lane typecheck; book
    // `docs/feature-designs/jit-ir-optimizations.md`).
    // The single-pass instanceof answers a display hit (and, with
    // CRATONVM_JIT_TYPECHECK_DISPLAY_MISS, a hierarchy-only miss) inline, as
    // the IR Op::InstanceOf does (jit `sp_instanceof_display_arm_enabled`).
    // Default ON since 2026-10-27 (zipfs `getFileAttributes`' single-pass
    // `instanceof Entry` misses); `=0` kills.
    // The IR reference aastore proves a subclass-of-component or
    // implementor-of-component value from the supers displays and skips
    // jit_aastore_type_check (jit `ir_aastore_display_arm_enabled`). Default ON
    // since measured 2026-11-07; `=0` kills.
    // Correctness: both tiers' definite instanceof miss for a final target
    // also needs the receiver display's hierarchy-only verdict, so an
    // Unmodifiable* stamp displaying as a final JDK class asks the helper
    // (jit `ir_lower::instanceof_final_miss_verdict_enabled`). Default ON,
    // "0" restores the id-range and latch screens alone.
    // TYPECHECK21-3: the IR instanceof / checkcast against `[LT;` answer a
    // reference array whose component is T or a proven subtype inline (jit
    // `typecheck_array_targets_enabled`). Default ON since R22-AB-FLIP; `=0` kills.
    // TYPECHECK21-4: jit_checkcast publishes a plain receiver's missing supers
    // display, as jit_instanceof does (vm `SupersDisplays::publish_on_checkcast`,
    // read once per VM). Opt-in until measured.
    // r21 typecheck wave 2. TYPECHECK21-5: the single-pass aastore proves a
    // subclass / implementor value's store from the supers displays (jit
    // `x64::op_array::sp_aastore_display_arm_enabled`). Default ON since R22-AB-FLIP; `=0` kills.
    // TYPECHECK21-3b: the single-pass instanceof / checkcast against `[LT;`
    // answer a proven covariant reference array inline (jit
    // `x64::op_object::sp_typecheck_array_targets_enabled`). Default ON since R22-AB-FLIP; `=0` kills.
    // JIT tier2 wave 3, lane tcperf (tag tcp21, TCPERF21-4): perf, opt-in. The single-pass instanceof / checkcast test
    // the receiver's type-check row (the optimizing tier's status-first row arm) before the supers-display arm; needs
    // CRATONVM_JIT_TYPECHECK_ROWS and the matching CRATONVM_JIT_TYPECHECK_ROW_FIRST[_IFACE] (jit
    // `typecheck_rows::sp_typecheck_row_first_enabled`).
    // TYPECHECK21-1 part A: the IR instanceof is a header-only read in the
    // memory model (jit `ir_instanceof_header_read_enabled`), and with the
    // second switch also produces no memory token (jit
    // `ir_instanceof_off_chain_enabled`). Opt-in until measured.

    // JIT round 21, lane typecheck (page jr21-synthetic-implements-over-admits-real-classes-20261014): the IR instanceof display MISS arm serves synthetic-implements targets (List, Collection, Iterable, Comparable, ...) too, now that synthetic_implements declines every receiver whose supertype closure is hierarchy-only. Default ON, "0" restores the target screen.
    // JIT round 21, lane dispatch (page jr21-dispatch-typecheck-rows-20261014, TYPECHECK21-4): the IR instanceof against an interface tests the receiver's per-VM type-check row after the seen ways, before the display arm (one compare for a hit; a hierarchy-only miss answered after the status word). Opt-in since the w1g A/B (default ON in front of the ways it lost on the common hit and won only on 7..9-interface receivers); default ON again after the w3a + w3e A/Bs (placed after the seen ways: instanceof-wide-iface 2306 -> 1164 ns, instanceof-interface 542 -> 436, no row lost), "0" turns it off.
    // JIT round 21, lane dispatch: with typecheck-rows on, the row arm also serves class targets (depth 1..7) and checkcast hits. Opt-in.
    // JIT round 21, lane tcperf (page jr21-tcperf-class-target-typecheck-cost-FIXED-20261015): the IR instanceof / checkcast against a class at depth 1..7 tests the receiver's type-check row FIRST in the status-first form (status word, then ONE compare of the chain word; hierarchy-only misses answered), before the exact-id compare, the seen ways and the display arm. Needs typecheck-rows. Default ON, "0" restores the round's placement.
    // JIT round 21, lane tcperf: the same status-first row arm for an INTERFACE target (home-slot hit; instanceof also the bloom-proved miss), first instead of after the seen ways; checkcast takes the hit half. Needs typecheck-rows. Default ON, "0" restores the round's placement.
    // JIT round 21, lane tcfuse (page jr21-dispatch-typecheck-rows-20261014, TCPERF21-2 = DISPATCH21-3): an IR instanceof whose only use is the fused Cmp(Eq|Ne, x, 0) of the If ending its block jumps from its inline 1 / 0 answers straight to that If's two edges (their phi-copy trampolines included); only the helper path materialises the answer. Default ON, "0" restores the round head.
    // JIT round 21, lane tcfuse (TCPERF21-3): two or more IR instanceof / checkcast of the same value share one ClassWordOf of it (an extra input of each check), and each check's status-first row arm starts from that word instead of its own null / kind screens and header load. Needs typecheck-row-first or -iface to matter. Opt-in ("1") since 2026-10-26: the in-process instruction counts lost 2-11 % with two to four checks of a value and saved nothing with six.
    // Declared 2026-09-02. `final-devirt` above is `java/lang/String`'s
    // problem: String is final, so the rewrite it drives took EVERY String
    // access site away from the inline intrinsic. This is the opt-out for the
    // yield that gives them back -- default OFF, so the yield is on.
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
    // JIT round 16 wave 6, lane codegen (CG16-10): the fresh-frame clear with
    // 16-byte stores and no redundant per-word zeroes; default ON since round 16
    // (`ir_zero_fresh_frames_sse_enabled` reads `runtime_flag_default_on`).
    // JIT round 16 wave 7, lane frames: what the SSE-form fresh-frame clear
    // may leave out -- the deopt XMM image the band scan never roots, and the
    // words the prologue writes before its first stop -- and its 32-byte AVX
    // form. Default ON since wave 8 (measured: R16Bench bintrees 98 -> 90,
    // poly 1487 -> 1360 ms with all three); `=0` is each one's kill switch.
    // JIT round 25 wave 6, lane W6-A (`docs/feature-designs/real-frame-deopt.md`,
    // "Wave 6 (W6-A)"): the fresh-frame clear emitted as a minimum set of
    // straight-line stores covering every word its mask requires (skipped
    // words may be zeroed too), instead of one run at a time with loops.
    // Opt-in.
    // JIT round 25 (item 3 of the round-24 redesign): an optimizing-tier body
    // publishes its `Prim` colours as a per-body claim the band scan honours at
    // every poll (the entry poll included), the fresh-frame clear leaves those
    // words out, and the colouring numbers the rootable colours first. Default
    // ON since R25-FLIP, `=0` kills.
    // JIT round 25 wave 2 (smaller optimizing-tier frames and clears; page
    // `docs/feature-designs/real-frame-deopt.md`): the `Pinned`
    // colours of non-reference values and the deopt GPR image join the
    // per-body claim (both inert without `ir-primitive-colour-claim`), and the
    // frame reserves Java-local words for the parameters only. Opt-in.
    // JIT round 25 wave 3 (same page): snapshot-named non-reference values
    // coloured by range instead of pinned (checked at every frame-state read,
    // refused attempts lowered again unranged), and the System V helper-argument
    // area in the per-body claim (inert without `ir-primitive-colour-claim`).
    // Opt-in.
    // JIT round 25 wave 8 (W8-6): the snapshot-named values local to the compile's splices, references
    // included, coloured by range instead of pinned (the wave-3 reader model and its emission check), so the frame
    // words of disjoint splices are shared and the frame and its fresh-frame clear shrink. Opt-in.
    // JIT round 25 wave 9 (W9-4): under `ir-splice-slot-share`, every snapshot-named reference (the root's
    // own too) coloured by range instead of pinned, not only the splice-local ones. Opt-in, inert alone.
    // JIT round 25 wave 10 (W10-6): the optimizing tier splices a callee body that allocates a reference array
    // (`anewarray`), its component class resolved by the VM resolver and rebased into the builder's row table;
    // off, such a body stays a call (`Matcher.<init>` out of `Pattern.matcher`). Opt-in.
    // JIT round 25 wave 13 (W13-6): under `ir-splice-anewarray`, also splice an `anewarray` callee that has a back
    // edge (wave 10's rule); off, such a loop-rooted callee stays a call with its own compile. Opt-in, inert alone.
    // rx27 (lane matcher): back to opt-in after the j27 mass flip; ON is W13-6's measured loss arm (`cached`).
    // Round rx27 wave 1 (lane matcher): splice a callee whose exception table (with a catch-all row: a `synchronized`
    // block, a `finally`) lies wholly inside a branch arm its profile never entered, the arm masked to a cold throw arm
    // that deopts at its branch (`Pattern.matcher`'s compile guard). Default ON since the rx27 bundle flip (2026-10-29), kill "0".
    // JIT round 16 wave 7, lane frames: the OSR entry stub's fresh-frame clear
    // (a frame-residue correctness fix), default ON under
    // `CRATONVM_JIT_IR_ZERO_FRESH_FRAMES`; `=0` is the kill switch.
    // JIT round 16 wave 11, lane frames2: the optimizing tier's deopt register
    // image sized to the registers a frame state can name (FR16P-1), opt-in
    // (default OFF) until measured; and the published end of the Java locals
    // made exclusive so the band scan reads the last local's word (FR-B1, a
    // correctness fix), default ON, `=0` is the kill switch.
    // JIT round 16 wave 12, lane polyclear: no deopt register image at all in
    // an optimizing body none of whose exits can name a register (FR2P-1),
    // default ON since wave 12 (poly 1238 -> 1174 ms), `=0` off.
    // JIT round 16 wave 17, lane frames2: the optimizing tier's callee-saved
    // save bands sized to the registers a body saves (PC-1), and the prologue's
    // watermark capture taking the thread pointer from RAX (FR16P-3); both
    // opt-in (default OFF) until measured. And the planned staging-word clears
    // moved with every layout cut of the frame (a correctness fix for the sized
    // deopt image), default ON, `=0` is the kill switch.
    // gci i5/ir (opt-in): the loop back-edge safepoint polls arm the dead-home facts (synthetic site per back-edge block; the slow path's map claims and clears the homes dead there).
    // JIT r25 item 2, wave 3 (opt-in): a frame-block word written on every path before every GC point is left out of the prologue's clear; the method-entry poll's slow path zeroes it (jit `ir_lower::plan_block_lazy_clear`).
    // JIT r25 item 2, wave 4 (opt-in, with ir-ref-residency): a field-load reference in a callee-saved register stops storing its home; field loads and primitive array ops in its range spill/reload it around their helper calls (jit `ir_lower::plan_lazy_ref_homes`).
    // JIT r25 wave 6, W6-D (opt-in): no method-entry poll in a loop-free body with no Java call (jit `ir_lower::plan_leaf_entry`).
    // JIT r25 wave 6, W6-D (opt-in): a leaf frame (loop-free, only pure ops, no self-lock) also skips the prologue's GC-only parts
    // (fresh-frame clear, bookkeeping zeroes, frame record, thread fetch) and the exits' shadow-top restore; verified after the body.
    // JIT r25 wave 7, W7-C (opt-in): a call whose continuation returns through pure ops only (no GC point, helper, exit or loop)
    // skips its hot-path frame-record republish; its sentinel sides republish (jit `ir_lower::plan_tail_republish_sink`).
    // JIT r24 (CompletableFuture residual page): perf, default ON, kill "0". A spliced callee with a published compiled body has a
    // frozen branch profile; its taken-0 arms are pruned at 100 observations, not 256 (jit `splice_never_taken_pcs_frozen`).
    // JIT round 21, lane argwin (OOM21-1 step 1; page
    // `jr21-compiled-caller-roots-its-outgoing-arguments-for-the-whole-callee-FIXED-20261026.md`):
    // default ON, `=0` off (WARNING argwin-flip-20261026; opt-in 2026-10-14 to
    // 2026-10-26). A statically bound optimizing-tier direct call whose callee owns
    // its arguments hands the callee-deopt service `SERVICE_ARGS_OWNED_BY_CALLEE`
    // and stages nothing, admitted like the single-pass d9/d hand-off
    // (`jit/src/ir_lower.rs`, `ir_callee_owned_args_enabled`); also off under
    // `CRATONVM_JIT_CALLEE_OWNED_ARGS=0`. Probe `R21OomArgWindow`.
    // JIT round 21, lane argwin (OOM21-1 step 2): default ON, `=0` off (WARNING
    // argwin-flip-20261026), acts only at a site
    // `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS` admitted. The homes of the
    // argument-only reference arguments (and their shadow words) are zeroed
    // between the marshal and the `CALL` (`jit/src/ir_lower.rs`,
    // `ir_callee_arg_window_enabled`). Probe `R21OomArgWindow local`.
    // ARGWIN21-4 (same page): default ON, `=0` off. The owned-arguments
    // hand-off's caller rule (both tiers) also admits a straight-line prefix
    // whose only stores fill arrays it allocated itself (varargs, array
    // initializers): `jit/src/deopt.rs`, `owned_args_caller_replay_is_exact`.
    // ARGWIN21-3 (same page): default ON, `=0` off. Both tiers' prologues
    // zero each incoming stack argument word after copying it to its local
    // slot, so the caller's outgoing block stops rooting a dropped reference
    // argument for the whole call (`jit/src/lib.rs`,
    // `zero_incoming_stack_args_enabled`). Probe `R21OomArgWindow wide`.
    // ARGWIN21-7 (same page): default ON, `=0` off. A keep-set step inside a
    // splice whose pc a chain snapshot describes reads its enclosing invoke's
    // snapshots without their operand stacks (the call's arguments); a flat
    // deopt there refuses the attempt (`jit/src/ir_lower.rs`,
    // `ir_keep_set_spliced_invoke_args_enabled`). Probe `R21OomArgWindow`.
    // t1 ARGFIX item 2 (page t1-ARGFIX-ir-owned-args-never-reach-osr-or-spliced-callers): under the precise keep set, a chain snapshot's scope locals are narrowed by each scope's own bytecode liveness (`ir_lower::splice_snapshot_dead_slots`). Opt-in, "1" enables.
    // ARGWIN21-8 (same page): default ON, `=0` off. An armed site's own `Ref`
    // result colour is cleared before its GC point while it holds an earlier
    // occupant no own exit reads (`jit/src/ir_lower.rs`,
    // `ir_site_result_home_clear_enabled`). Probe `R21OomArgWindow temp`.
    // ARGWIN21-9 (same page): default ON, `=0` off. A single-pass owned direct
    // call zeroes the caller's dead reference locals after the marshal
    // (`jit/src/x64/op_invoke.rs`, `sp_owned_call_dead_local_clears_enabled`).
    // Probe `R21OomArgWindow local`.
    // OOM21-6 / ARGWIN21-6 (same page; JIT round 21's fix, re-landed): default
    // ON, `=0` off; acts only with the opt-in `CRATONVM_JIT_SP_MAP_LOCAL_LIVENESS`.
    // The map-liveness rule is made monotone: it speaks inside splices, looks
    // past the 64-byte next-instruction bound and drops every candidate at an
    // uncovered pc (`jit/src/x64/safepoint.rs`,
    // `sp_map_local_liveness_monotone_enabled`).
    // JIT round 21, lane argwin (OOM21-3): opt-in. An optimizing body zeroes a
    // reference parameter's prologue home at an armed dead-home site where the
    // parameter is dead (`jit/src/ir_lower.rs`, `ir_param_home_liveness_enabled`).
    // Probes `R21OomArgWindow temp-arg`, `R21ArgwinWindow`.
    // JIT round 17 wave 3, lane irlower (MON17W2-5): in the IR dead-slot keep
    // set, a node naming its own per-copy frame reads that frame only, not the
    // union of every snapshot at its pc. Opt-in (shrinks a GC keep set).
    // r21 cf (wave 13): perf, DEFAULT ON since JIT r24 (CF handler family; kill "0"). An optimizing-tier splice keeps a volatile
    // `putfield` (a fenced ordered `Op::Store`) instead of refusing the body with
    // `splice-volatile-field-store`: `CompletableFuture.uniApplyNow`'s `d.result =`
    // (`vm/src/runtime/interpreter/jit_bridge.rs`, `jit/src/ir.rs`
    // `ir_splice_volatile_store_enabled`).
    // gen r5w6/oomjit10: default ON, `=0` restores the unconditional RAX bit.
    // A single-pass Java invoke's register oop mask leaves RAX out: its CALL
    // target never reads RAX and nothing after the call reads its pre-call
    // value (`jit/src/x64/safepoint.rs`, `live_oop_register_mask`).
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
    // Wrong-answer A/B lever, not a tuning knob: OFF restores the params-only
    // `run_jit_callee_handler` resume that zeroed a compiled callee's
    // non-parameter locals. See `params_only_callee_handler_frames`.
    // Guard-dominated bounds-check elimination (`x64::bce::range_bce_enabled`).
    // Default-**OFF**. The gate was presence-parsed (`.is_some()`) until
    // 2026-09-12, when `CRATONVM_JIT_RANGE_BCE=0` *enabled* it; it is now a
    // `runtime_flag_on` read, so `=0` is off. `off_word` still stays `None`:
    // unsetting the key is off under both readings, and a wrong elision here is
    // an out-of-bounds heap write, so the token must not depend on which parser
    // a future edit leaves behind. `-bce` (`CRATONVM_JIT_NO_BCE`) still kills
    // every reason including this one.
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
    // Default-ON kill switch, hence `off_key` only: `CRATONVM_JIT=-retpc-validate`
    // makes the A5 unregistered-JIT-frame stack scan treat EVERY in-range stack
    // word as a return address again, the way it did before 2026-08-04. Kept as
    // a one-flag bisect for the false-positive filter in
    // `conservative_roots::is_plausible_return_pc`.
    // Default-ON kill switch, hence `off_key` only:
    // `CRATONVM_JIT=-native-site-cache` takes the JIT's per-call-site native
    // fast path out of a run. It exists because that path spent 2026-08-05 as
    // the prime suspect for the Spring Boot corruption family — it reads a memo
    // keyed on a `JitInvokeInfo` ADDRESS, and while those were recyclable
    // (`383e7f5cf`) it was the loudest way that hazard surfaced — with no way to
    // remove it from a run short of a rebuild. See
    // `jit::helpers::native_site_cache_enabled` for the measured rates.
    // JIT round 19 wave 2, lane funnel2 (FUNNEL17-5 residual): opt-in perf,
    // default OFF. The SBR5 lean native door (`try_lean_native_door`) sends a
    // non-leaf hit through the lean funnel (`safe_native_call_jit_lean`) instead
    // of `safe_native_call_prevalidated_objects`, flushing SATB before the decode.
    // Interpreter round i3 wave 81, lane L3: perf, default ON since the
    // wave-81 triage (`Method.invoke` 17-24% faster in both modes; `=0`
    // reverts). The virtual-MIC helper calls the forced `Method.invoke` /
    // `Constructor.newInstance` native directly instead of the MIC resolution
    // and the by-name `invoke_or_native` cascade.
    // Interpreter round i3 wave 83 (lane L3): a reflective site the direct
    // arm served is filed in the lean native table, so the next call is
    // served at the helper's first statement (default off).
    // JIT round 19 wave 6, lane mega6 (FUNNEL3-19-1): opt-in perf, default OFF.
    // A virtual/interface site's native-site refusal is cached for the refusing
    // receiver class only, so a later receiver whose method is a native is served.
    // JIT round 19 wave 6, lane mega6 (FUNNEL3-19-2): opt-in perf, default OFF.
    // The ChmGet / IntWrapperEquals / ReferenceRefersTo leaf probes take the
    // reference argument the door already validated (no second heap walk).
    // Default-ON kill switch: `CRATONVM_JIT=-negative-cast-memo` withdraws the
    // `instanceof` negative receiver memo (`CastSite::negative_receivers`).
    // `indy-callsite-cache` — the per-instruction cache of a linked non-JDK
    // `invokedynamic` CallSite (invokedynamic.rs, `bootstrap_generic`). Unset (or
    // `all`) caches every CallSite, mutable ones included (default since
    // interpreter round i1 wave 47); `constant` skips Groovy's mutable sites;
    // `0` restores re-bootstrapping on every execution.
    // Interpreter round i1 wave 47 switches, each default ON with `=0` as the
    // kill switch: the frame install that aliases the retired slot's own cache
    // handle (lane L7), a call inside a splice gets a post-call exit (lane L3),
    // the resolved class-entry table (lane L5), `privateLookupIn`'s one module
    // question by identity (lane L6), and one frame listing for JDWP and the C
    // JVMTI table (lane L1).
    // Interpreter round i6 wave 118 (lane L5): the export / reflective module
    // gates ask the one module question by identity first and admit on its
    // yes (default ON, `=0` off; `LoaderFlags::module_access_export_one_question`).
    // Interpreter round i6 wave 118 (lane L5): a class a user loader defined
    // in a platform module's package is in its loader's unnamed module for
    // `Class.getModule()` (HotSpot parity; default ON, `=0` off;
    // `VmConfig::spoofed_package_unnamed_module`).
    E { group: Group::COMPAT, token: "module-access-export-one-question", on_key: Some("CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION"), off_key: None, off_word: Some("0"), since: "2026-12-20" },
    // Interpreter round i1 wave 48 switches, default ON with `=0` as the kill
    // switch unless marked opt-in: the signature-polymorphic site cache (L7),
    // MethodType constants resolved in the VM and --compatible's initiating
    // records and parallel-capable loader model (L5), Lookup.in's module
    // questions by identity (L6), inline-frame rows for stub calls, the hidden
    // invoke frames of a SHOW_HIDDEN_FRAMES walk and --compatible's hidden-frame
    // screen (L3), and the blocking methods' bytecode under a debugger and the
    // default virtual-thread sleep shape (L1). `throwable-standin-virtual-as-bound`
    // is opt-in: it gives virtual threads -XX:-VMContinuations's frames.
    E { group: Group::COMPAT, token: "compat-initiating-records", on_key: Some("CRATONVM_COMPAT_INITIATING_RECORDS"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::COMPAT, token: "compat-parallel-loaders", on_key: Some("CRATONVM_COMPAT_PARALLEL_LOADERS"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    // Interpreter round i6 wave 120 (lane L5): `Lookup.defineClass` /
    // `defineHiddenClass*` run the JDK's own checks down to `defineClass0`
    // (default OFF; `LoaderFlags::lookup_define_jdk_body`).
    // Interpreter round i2 wave 53 (lane L2): a trap-free, call-free
    // synchronized method's optimizing method-entry body gets back-edge poll
    // exits on the named-body verdict bit, which a body bound by a caller-held
    // sync-direct site never gets (`ir_lower::ir_sync_leaf_named_poll_exits_enabled`).
    // Interpreter round i2 wave 54 (lane L2): an optimizing METHOD-ENTRY
    // compile puts a post-call exit after a call inside a splice whose source
    // it keeps (`ir::ir_entry_post_call_splice_exits_enabled`).
    // Interpreter round i2 wave 55 (lane L2): a single-pass method-entry
    // post-call exit beside a guard of another frame bakes its own point
    // (`jit::x64::op_invoke::post_call_own_point_exits_enabled`).
    // Interpreter round i2 wave 56 (lane L2): the same in an OSR-tier compile
    // that already filed a loop map; the OSR door's by-bci readers know the
    // point (`jit::x64::op_invoke::osr_post_call_own_point_exits_enabled`).
    // Interpreter round i2 wave 56 (lane L2): a single-pass self-locking
    // method-entry body's mode exits hand the method monitor to the resumed
    // frame (`jit::deopt::self_lock_mode_exit_handover_enabled`).
    // Interpreter round i2 wave 57 (lane L2): the back-edge poll of a loop
    // inside a spliced callee leaves through a caller chain, in OSR-door and
    // (with the spliced source kept) method-entry optimizing compiles
    // (`jit::ir::ir_spliced_header_poll_exits_enabled`,
    // `ir_entry_spliced_header_poll_exits_enabled`).
    // Interpreter round i2 wave 58 (lane L2): a synchronized splice's monitor
    // enter publishes a map whose inline chain names the spliced callee at
    // bci 0, so a thread blocked entering it lists the callee on top
    // (`jit::ir_lower::ir_sync_splice_enter_names_callee_enabled`).
    // JIT round 19 proposal wave 10 (lane deopt10, i62-L2): single-pass chain
    // points (spliced callee bcis) leave the root method's by-bci readers --
    // OSR entry contract, resume images, cause-less reason lookups -- and a
    // chain trap at the call-site service is charged its own cause
    // (`jit::osr_exit::chain_points_off_root_bcis_enabled`). Opt-in.
    // Interpreter round i6 wave 115, lane L3 (opt-in): a SHOW_HIDDEN_FRAMES
    // walk across a served `invokeWithArguments` asks the JDK for the spun
    // LambdaForm$MH classes of its spread invoker and adapter and lists them.
    // Interpreter round i6 wave 121, lane L3 (parity, default ON since the i7
    // triage, WARNING: i7-default-flip, `=0` off): a SHOW_HIDDEN_FRAMES
    // walk across a served `invokeExact` of an unpregenerated shape asks the
    // JDK for the spun linker and lists its `invokeExact_MT` frame.
    // Interpreter round i6 wave 118, lane L3 (performance, opt-in; proposal
    // i37-L6): the class store keeps each class's hidden-method bits, so a
    // walk tests a bit instead of decoding the method's annotations.
    // JIT round 17 wave 3, lane compatbridge: default OFF (performance A/B); under `--real-jdk`
    // the interpreter's intrinsic table stops serving its reviewed `String.length` / `charAt` /
    // `isEmpty` rows (native-builtins intrinsics/mod.rs `jdk_only_string_table_bytecode_enabled`).
    // STR16-7: under `--real-jdk` the `Integer.toString(I)` / `Long.toString(J)` /
    // `String.valueOf(I)` natives stay unregistered and the JDK bodies run (opt-in).
    // Default ON (`=0`: every virtual thread a `BoundVirtualThread` on the VM's
    // own carriers): the JDK drives virtual threads on its own `ForkJoinPool`
    // carriers through the continuation natives (`vm::jdk_continuation`).
    // Opt-in; one reader since r17 w2 (`stackwalker::virtual_standin_as_bound`,
    // `r17-standin-virtual-as-bound-readers-disagreed-FIXED-20261005.md`).
    // Interpreter round i1 wave 49 (lane L4): a direct (reflective)
    // `LambdaMetafactory.metafactory` / `altMetafactory` call runs the JDK's
    // own factory instead of the registered reflective bridge.
    // Default on since wave 51 (its blocker, the proxy's `/0x` name tail, is
    // fixed); `=0` keeps the bridge.
    // Interpreter round i1 wave 49 (lane L4): the `--real-jdk` checks of the
    // natively linked bootstraps run under `--compatible` too.
    E { group: Group::COMPAT, token: "indy-checks-every-mode", on_key: Some("CRATONVM_INDY_CHECKS_EVERY_MODE"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    // Interpreter round i1 wave 50 (lane L4): the `--real-jdk` selection
    // checks of the invoke doors (invokespecial selection, invokeinterface
    // receivers, methodref tags) run under `--compatible` too.
    E { group: Group::COMPAT, token: "selection-checks-every-mode", on_key: Some("CRATONVM_SELECTION_CHECKS_EVERY_MODE"), off_key: None, off_word: Some("0"), since: "2026-10-01" },
    // Interpreter round i6 wave 133, lane L4 (i132-L6): an interface sender's `invokespecial`
    // checks its receiver (HotSpot's IllegalAccessError) and is never cached; the compiled
    // doors refuse such a site unless it passes its own `this` (parity, default ON).
    // Interpreter round i7 wave 136, lane L6 (i135-L6): the interpreter caches an interface
    // sender's `invokespecial` as a receiver-class-guarded entry once the receiver check has
    // passed for that class (performance only; default ON since i7 wave 144;
    // `VmConfig::interface_special_receiver_memo`).
    // default ON since interpreter round i7 (A/B w142lto)
    // Interpreter round i7 wave 136, lane L2 (i135-L6): the method-entry compile doors bind an
    // unproven interface-sender `invokespecial` through the dispatch helper, which makes the
    // receiver check, instead of refusing the method (performance only; default ON since i7 wave 144;
    // `VmConfig::jit_iface_special_receiver_guard`).
    // default ON since interpreter round i7 (A/B w142lto)
    // JIT round 17 wave 3 (lane select): the by-name dispatch tail runs a
    // supertype name on the receiver's own linked copy, and the JIT's §5.4.6
    // retarget survives a selected class whose name two loaders define.
    // Interpreter round i1 wave 50 (lane L4): the `--real-jdk` argument checks
    // of the `MethodHandles` combinators, and the access check of an `ldc` of
    // a `CONSTANT_MethodHandle`, run under `--compatible` too.
    E { group: Group::COMPAT, token: "mh-combinator-checks-every-mode", on_key: Some("CRATONVM_MH_COMBINATOR_CHECKS_EVERY_MODE"), off_key: None, off_word: Some("0"), since: "2026-10-01" },
    E { group: Group::COMPAT, token: "ldc-handle-access-every-mode", on_key: Some("CRATONVM_LDC_HANDLE_ACCESS_EVERY_MODE"), off_key: None, off_word: Some("0"), since: "2026-10-01" },
    // Interpreter round i1 wave 49 switches, default ON with `=0` as the kill
    // switch unless marked opt-in: an unmounted virtual thread's continuation
    // rows and the JDWP suspension settle wait (L1; the virtual threads in
    // AllThreads are opt-in), the doPrivileged frame of the privileged stand-ins
    // (L3), --compatible's loader constraints, loader throws and JDK-global-name
    // asks and --real-jdk's record-replaces-memo reader (L5), --compatible's
    // URL application handler, named packages, module messages and the JDK's
    // own immutable collections, and the unqualified-export question by
    // identity (L6), and the shared call-site descriptor channel (L7).
    E { group: Group::COMPAT, token: "compat-loader-constraints", on_key: Some("CRATONVM_COMPAT_LOADER_CONSTRAINTS"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::COMPAT, token: "compat-loader-throws", on_key: Some("CRATONVM_COMPAT_LOADER_THROWS"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::COMPAT, token: "compat-global-name-ask", on_key: Some("CRATONVM_COMPAT_GLOBAL_NAME_ASK"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::COMPAT, token: "compat-url-app-handler", on_key: Some("CRATONVM_COMPAT_URL_APP_HANDLER"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::COMPAT, token: "compat-named-packages", on_key: Some("CRATONVM_COMPAT_NAMED_PACKAGES"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::COMPAT, token: "compat-module-messages", on_key: Some("CRATONVM_COMPAT_MODULE_MESSAGES"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::COMPAT, token: "compat-jdk-immutables", on_key: Some("CRATONVM_COMPAT_JDK_IMMUTABLES"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::COMPAT, token: "module-export-all-by-identity", on_key: Some("CRATONVM_MODULE_EXPORT_ALL_BY_IDENTITY"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    // Interpreter round i2 wave 53 (lane L6): a thread suspended in a native
    // outside a blocking region acknowledges the JDWP suspension (stage 2 of
    // the suspension handshake).
    // Interpreter round i4 wave 94 (lane L1): a `SUSPEND_EVENT_THREAD` event
    // set waits for its event thread, and the server poll's `ClassPrepare`
    // set that suspends every thread waits for them, before either is
    // written, as HotSpot's back end suspends before it sends (HotSpot parity
    // fix, default ON; `=0` sends them at once as before).
    // Interpreter round i5 wave 101 (lane L1): a JDWP invocation's reply
    // waits for the threads its closing re-suspension covered, as HotSpot's
    // `invoker_completeInvokeRequest` suspends before it replies (HotSpot
    // parity fix, default ON; `=0` replies at once as before).
    // Interpreter round i7 wave 136 (lane L1): `-agentlib:jdwp` counts as a
    // JDWP debugger for the start-time facts `--jdwp-port` alone answered
    // (every local a GC root, a blocked thread's inspection window). HotSpot
    // parity fix, default ON; `=0` asks `--jdwp-port` alone.
    // Wave 140, lane L1: `timeout=` bounds each listen of a `server=y`
    // transport; no debugger in time ends the process as the back end does.
    // Parity fix, default ON (`=0` waits for ever, as before).
    // Interpreter round i5 wave 104 (lane L1): `main`'s end posts JVMTI
    // `ThreadEnd` / JDWP `ThreadDeath`, the `DestroyJavaVM` attach JVMTI / JDWP
    // `ThreadStart`, each under its own JDWP thread id, as HotSpot's launcher
    // (HotSpot parity fix, default ON; `=0` posts none and keeps one id).
    // Interpreter round i2 wave 54 (lane L6): a suspended thread that wins a
    // contended monitor gives it back until its resume (HotSpot's
    // `ExitOnSuspend`).
    // Interpreter round i2 wave 54 (lane L6): the JDWP suspension's waits
    // (the debugger's park, the settle and frame waits, a GC-blocked
    // suspended thread's wait) wait on a condvar the publications notify.
    // Interpreter round i4 wave 84 (lane L6): a mounted JDK virtual thread
    // blocking on a contended monitor has the scheduler compensate for the
    // carrier it holds (`Blocker.begin`/`end`), the stand-in for HotSpot 25's
    // unmount (JEP 491). `=0` keeps the carrier uncompensated.
    // Interpreter round i4 wave 87 (lane L6): JEP 491 -- a JDK virtual
    // thread's contended interpreted `monitorenter` unmounts it (entrant
    // queue, release hand-off, `takeVirtualThreadListToUnblock`) instead of
    // blocking its carrier. Default off.
    // Interpreter round i4 wave 92 (lane L6): a mounted JDK virtual thread
    // enters the compiled body of a static callee whose bytecode cannot reach
    // a yield point (no invoke, no monitor op, not synchronized), and offers
    // it for tier-up (performance only; default OFF until measured;
    // `VmConfig::vthread_compiled_leaf_entry`).
    // Interpreter round i4 wave 93 (lane L6): the same for INSTANCE leaves
    // through the general dispatcher's virtual / interface / special arms
    // (performance only; default OFF until measured;
    // `VmConfig::vthread_compiled_instance_leaf`).
    // Interpreter round i4 wave 93 (lane L6): the virtual-thread leaf
    // predicate also admits a body whose only invokes are up to four
    // `invokestatic` sites this thread has cached as static leaves
    // (performance only; default OFF; `VmConfig::vthread_compiled_leaf_callers`).
    // Interpreter round i5 wave 100 (lane L6): back-edge OSR on a mounted JDK
    // virtual thread for a frame whose method passes the leaf predicate
    // (background compilation only; performance only; default OFF;
    // `VmConfig::vthread_compiled_leaf_osr`).
    // Interpreter round i5 wave 101 (lane L2): compiled code on a mounted JDK
    // virtual thread for any non-synchronized method, learned off by its first
    // pin (performance only; default OFF again: a virtual thread parked under
    // a compiled frame pins where HotSpot unmounts, so the Quarkus
    // `@ShouldNotPin` classes fail; opt in with `=1`;
    // `VmConfig::vthread_compiled_learned`).
    // Interpreter round i5 wave 106 (lane L6): the learned switch's ENTRY arm
    // (an interpreted virtual-thread frame enters any non-denied compiled
    // body); needs `CRATONVM_VTHREAD_COMPILED_LEARNED` (performance only;
    // default ON since i7 wave 144; `VmConfig::vthread_compiled_learned_entry`).
    // default ON since interpreter round i7 (A/B w142lto)
    // Interpreter round i5 wave 113: a JDK virtual thread's interpreter
    // records no per-method armed branch profile (performance only; default
    // ON, `=0` off; `VmConfig::vthread_skip_branch_arm`).
    // Interpreter round i5 wave 114 (lane L6): an interpreted JDK virtual
    // thread frame enters the compiled body of any non-synchronized,
    // non-denied APPLICATION method (no caller walk); needs
    // `CRATONVM_VTHREAD_COMPILED_LEARNED` (performance only; default OFF;
    // `VmConfig::vthread_compiled_app_entry`).
    // Interpreter round i5 wave 114 (lane L6): a JDK virtual thread's
    // interpreted `invokestatic` counts on the call site's entry, as the
    // static fast door does, not on the shared profile store per call
    // (performance only; default OFF; `VmConfig::vthread_local_call_count`).
    // Interpreter round i5 wave 114 (lane L6): the static and non-virtual
    // invoke fast doors run on a mounted JDK virtual thread too, judging a
    // compiled callee as the general arms do there; implies
    // `CRATONVM_VTHREAD_LOCAL_CALL_COUNT` (performance only; default OFF;
    // `VmConfig::vthread_nonvirtual_doors`).
    // Interpreter round i6 wave 115 (lane L6): the virtual / interface
    // invoke fast door runs on a mounted JDK virtual thread too, judging its
    // callee as the general `VirtualBytecode` arm does there (performance
    // only; default OFF; `VmConfig::vthread_virtual_door`).
    // Interpreter round i6 wave 119, lane L2 (performance, opt-in): the single-pass OSR door binds
    // an `invokespecial` whose JVMS 6.5 selection start differs from its constant-pool class to that
    // owner (`jit_bridge::classify_invoke_site`) instead of denying the loop.
    // Interpreter round i6 wave 119, lane L2 (performance, opt-in; stage 3 of `i24-L3`): compiled
    // polls test this OS thread's loop poll word through a TLS mirror that only a compiled-entry
    // door of the word's VM publishes, falling back to the baked STW flag when the mirror is null.
    // Interpreter round i6 wave 120, lane L2 (debugger parity, default ON, kill "0"; stages 1-2 of `i43-L1`): a
    // compile a debugger may read stores every register- and XMM-homed local to its home slot at
    // every safepoint and records each home's kind, and `StackFrame.GetValues` on a parked thread's
    // compiled activation answers from those slots.
    // t1 wave 5, lane S (performance, default ON since i25 2026-10-28, =0 kills; i25-L2 stage 2): the interpreter-to-compiled door does its bookkeeping in fewer thread-local and locked operations (one JIT_SIGNALS access for the entry retire, one shared exceptional-stash depth read, the entry guard's push/pop through the door form without the diagnostic quiescence tallies). Read once per VM (VmConfig::jit_door_thread_block).
    // Spring AOT round 2026-11-07: default ON, =0 off.
    // t1 wave 8, lane I25 (performance, default ON since j27 2026-10-28, =0 kills; i25-L2 stage 2b; needs jit-door-thread-block): the door's chain entry raises one process-wide striped counter (DOOR_DEPTH) instead of three (GLOBAL_JIT_DEPTH, the collector's quiescence depth, ACTIVE_JIT_EXECUTIONS); each of the three folds it into its readers. Read once per VM (VmConfig::jit_door_one_depth_counter).
    // Interpreter round i6 wave 117 (lane L6): a compiled pin with no OSR
    // activation on the stack, while an entry arm is on, denies the entry
    // doors only and leaves the learned OSR door open (performance only;
    // default ON, `=0` off, no effect with the entry arms off;
    // `VmConfig::vthread_entry_pin_denies_entry_only`).
    // Interpreter round i6 wave 118 (lane L6): a JDK virtual thread's leaf
    // check answers a body's scan from a per-VM memo, not per call
    // (performance only; default OFF; `VmConfig::vthread_leaf_verdict_memo`).
    // Interpreter round i6 wave 120 (lane L6): the application entry arm
    // refuses a callee under a denied frame within the caller-walk depth, as
    // the learned entry arm does (performance only; default OFF;
    // `VmConfig::vthread_app_entry_caller_walk`).
    // Interpreter round i6 wave 122 (lane L6): the interpreted lambda door
    // runs on a JDK virtual thread too, with no tier-up block there
    // (performance only; default OFF; `VmConfig::vthread_lambda_door`).
    // JIT round 23 wave 5, lane L6 (page r23-w2-lambda-recursion-overflows-at-380-levels-FIXED-20261021.md, "rep 0"): perf / stack depth, default ON, kill "0". The by-name static lambda arm files the impl owner BEFORE dispatching when the named class is already initialized, so a SAM recursion's first chain reaches the memoised / compiled direct arm from its second level (`LambdaSwitches::owner_memo_before_call`).
    // default ON since interpreter round i7 (A/B w142lto)
    // Interpreter round i6 wave 125 (lane L6, page `i125-L6`): `execute`
    // pushes a CACHED frame on a virtual thread only, so the OSR offer can
    // judge it (performance only; default ON since i7 wave 144;
    // `VmConfig::vthread_execute_cached_frame`).
    // default ON since interpreter round i7 (A/B w142lto)
    // Interpreter round i6 wave 120 (lane L6): a JDK continuation's slice
    // runs on its carrier's own OS thread (no compiled or JNI-local carrier
    // frame), not on a runner (performance only; default OFF, `=0` keeps the
    // runner; `VmConfig::vthread_slice_on_carrier`).
    E { group: Group::THREADS, token: "vthread-slice-on-carrier", on_key: Some("CRATONVM_VTHREAD_SLICE_ON_CARRIER"), off_key: None, off_word: None, since: "2026-10-19" },
    // t1 wave 5 (lane E2; stage 2 of the i94 proposal): with `vthread-slice-on-carrier` on, a carrier WITH compiled
    // frames below `enterSpecial` runs the slice too: its per-OS-thread JIT records are set aside for the slice
    // (`ChainSegment`), own-stack readers stop at the slice base, and blocked-peer captures above it go to the
    // carrier (performance only; default OFF; `VmConfig::vthread_slice_on_carrier_compiled`).
    E { group: Group::THREADS, token: "vthread-slice-on-carrier-compiled", on_key: Some("CRATONVM_VTHREAD_SLICE_ON_CARRIER_COMPILED"), off_key: None, off_word: None, since: "2026-10-26" },
    // Interpreter round i4 wave 94 (lane L6): a finished JDK continuation's
    // interpreter site caches go to a per-VM pool and seed the next
    // continuation thread (performance only; default OFF;
    // `VmConfig::vthread_recycle_exec_caches`).
    // Interpreter round i4 wave 94 (lane L6): a JDK continuation's first
    // slice finds its thread by the `Thread.tid` index only, and its death
    // purges terminated registry entries in batches (performance only;
    // default OFF; `VmConfig::vthread_lean_registry`).
    E { group: Group::THREADS, token: "vthread-lean-registry", on_key: Some("CRATONVM_VTHREAD_LEAN_REGISTRY"), off_key: None, off_word: None, since: "2026-11-27" },
    // Interpreter round i4 wave 94 (lane L6): a JDK continuation's carrier
    // sleeps at once instead of spinning 200 us for its runner's slice
    // (performance only; default OFF; `VmConfig::vthread_carrier_sleeps`).
    E { group: Group::THREADS, token: "vthread-carrier-sleeps", on_key: Some("CRATONVM_VTHREAD_CARRIER_SLEEPS"), off_key: None, off_word: None, since: "2026-11-27" },
    // Interpreter round i5 wave 106 (lane L6): the carrier's and the runner's
    // hand-off spins yield the core every 64 turns (performance only;
    // default OFF; `VmConfig::vthread_handoff_spin_yields`).
    // Interpreter round i5 wave 101 (lane L6): the AQS owner record is not
    // written while the per-lock owner index is retired (performance only;
    // default OFF; `VmConfig::owner_record_needs_index`).
    // Interpreter round i5 wave 101 (lane L6): JNI `MonitorEnter` /
    // `MonitorExit` leave HotSpot's NPE / IMSE pending (parity; default ON;
    // `VmConfig::jni_monitor_exceptions`).
    // Interpreter round i5 wave 101 (lane L6): the launcher ends the main
    // thread's `java.lang.Thread` before the non-daemon wait, as HotSpot's
    // `LEAVE()` detaches it (parity; default ON;
    // `VmConfig::main_thread_ends_before_destroy`).
    // Interpreter round i5 wave 102 (lane L6): once the main thread's Java
    // side ended, the launcher's waiting thread is listed as HotSpot's
    // `DestroyJavaVM` thread (parity; default ON;
    // `VmConfig::destroy_java_vm_thread`).
    // Interpreter round i4 wave 95 (lane L6): a JDK continuation's thread is
    // registered, mounted and retired with one registry publish each, its
    // death purges the terminated tail by a kept count outside the GC
    // barrier's lock (performance only; default ON since
    // the wave-97 triage, `=0` off; `VmConfig::vthread_one_publish`).
    // Interpreter round i4 wave 95 (lane L6): a JDK continuation's death
    // sweeps the inflated monitors only when its thread holds a monitor
    // entered outside structured locking, by a per-thread count (performance
    // only; default ON since the wave-97 triage, `=0` off;
    // `VmConfig::vthread_death_sweep_by_count`).
    // Interpreter round i4 wave 95 (lane L6): the virtual-thread manager drops
    // terminated JDK continuations' entries past a bounded tail (a leak fix;
    // default ON, `=0` keeps them; `VmConfig::vthread_retire_manager_entries`).
    // Interpreter round i4 wave 94 (lane L6): per-phase timings of every JDK
    // continuation slice, printed at exit (`VmConfig::dbg_vthread_census`).
    // Interpreter round i6 wave 115 (lane L6): one stderr line per
    // `monitorexit` of a monitor the executing frame's record does not name
    // that the monitor-table leniency released (or, strict, refused); the
    // positive control of `CRATONVM_MONITOREXIT_FRAME_STRICT`.
    // Interpreter round i6 wave 115 (lane L6; stage 3 of proposal
    // `i23-L7-proposal-per-frame-locked-monitors`): a `monitorexit` of a
    // monitor no interpreter frame's record accounts for throws
    // `IllegalMonitorStateException`, as HotSpot's interpreter does, instead
    // of releasing an acquisition a compiled caller, JNI or a native made
    // (HotSpot parity; default ON since wave 134 by user decision, `=0` off).
    // WARNING(i6-user-flip): CRATONVM_MONITOREXIT_FRAME_STRICT default ON by user decision 2026-10-19; =0 restores the old path
    // Interpreter round i7 wave 135 (lane L6; proposal
    // `i23-L7-proposal-per-frame-locked-monitors`, "Wave 135"): neither the
    // strict arm nor the compiled-holder proof throws at a `monitorexit` of a
    // method whose block monitors are balanced (javac's shape: its operand is
    // always the frame's own lock, so HotSpot never throws there); the miss is
    // an under-stated record and is released (parity-neutral hang guard,
    // default ON, `=0` off).
    // t1 wave 3 lane K (parity, default ON, `=0` off): a `synchronized` method's own monitor follows HotSpot -- the
    // frame's own `monitorexit` of it releases it and the return / unwind throws IllegalMonitorStateException, and a
    // method monitor not owned at the return / unwind throws instead of a warn line (`VmConfig::monitorexit_method_lock_parity`).
    // Interpreter round i2 wave 56 (lane L6): the JDWP suspension's settle
    // wait waits up to 10 s for a descheduled running thread, not 250 ms.
    // Interpreter round i2 wave 56 (lane L6): a thread suspended in a native
    // that runs on lists its frames through a window the native opened.
    // Interpreter round i2 wave 58 (lane L6): a handshake pause (frame trace,
    // loop exit) excuses a thread running a native instead of waiting for it,
    // as HotSpot's handshake treats `_thread_in_native` as safe.
    // Interpreter round i2 wave 58 (lane L6): a native frames window opened
    // under compiled code records the JIT entry chain; the reader walks it.
    // Interpreter round i2 wave 63, lane L6: a thread parked or held by a
    // debugger / JVMTI suspension waits on its own suspend word and parker,
    // taking the debug-state lock only when the word moves; default off.
    // Interpreter round i2 wave 64, lane L6: the JDWP suspension settle wait
    // walks the debug state only when an arrival moved the per-VM count, and
    // then only the threads still on their way; default off.
    // Interpreter round i6 wave 117 (lane L1, performance; default ON since
    // the wave-129 triage, `=0` off): a JDWP suspension's settle wait walks
    // the debug state once and then
    // drains only the threads whose listing was published since (i101-L1
    // part 3), instead of re-walking every pending thread each round.
    // Interpreter round i6 wave 121 (lane L1, HotSpot parity, default off
    // until `L1W119RawJdwpNativeThisObject` agrees under
    // `CRATONVM_DEBUG_COMPATIBLE_BLOCKING_BYTECODE=0 --compatible`): a
    // blocking stand-in's `Object.wait` Java levels answer their receiver
    // (the waited monitor) and the `0L` timeout to JDWP and the C table
    // (i119-L1).
    // Interpreter round i6 wave 126 (lane L1, HotSpot parity, default on,
    // `=0` escape): a liveness-dead reference local of a thread blocked in a
    // native region (or an unmounted virtual thread) reads through its
    // blocking deposit's slot origin, as a live one does, instead of `null`
    // (`LockSupport.park(Object)`'s blocker; i47-L1).
    // Interpreter round i6 wave 130 (lane L1a, HotSpot parity, default on,
    // `=0` off): the same dead-local read, only in a VM started with a JDWP
    // port, where every local is a GC root (so a dead slot's referent cannot
    // die under it); `LockSupport.park(Object)`'s blocker (i47-L1, i126-L1).
    // Interpreter round i2 wave 64, lane L6: a stop-the-world request unparks
    // the threads parked on their suspend word (with
    // CRATONVM_DEBUG_THREAD_SUSPEND_WORD), so such a park waits up to the
    // word's 50 ms backstop instead of polling the flag every 1 ms; default off.
    // Interpreter round i2 wave 65, lane L6: a thread parked at its suspend
    // point on its suspend word (with CRATONVM_DEBUG_THREAD_SUSPEND_WORD)
    // waits inside a GC-blocked region, so a stop-the-world pause neither
    // wakes it nor waits for it; default off.
    // Interpreter round i2 wave 66 (lane L6): with CRATONVM_DEBUG_PARK_GC_BLOCKED,
    // the JDWP server's heap work goes to an idle parked thread outside its
    // GC-blocked region, else to the heap service, and only then to a
    // GC-blocked park; a park whose word already moved takes its next locked
    // round before it opens its region again; default off.
    // Interpreter round i2 wave 66 (lane L6): with CRATONVM_DEBUG_PARK_GC_BLOCKED,
    // a park re-entering its GC-blocked region after heap work or a relisting
    // that left its frames, the collections and its last root publish as they
    // were raises the flag over the standing deposit instead of rebuilding it;
    // default off.
    // Interpreter round i2 wave 68 (lane L6): with CRATONVM_DEBUG_PARK_GC_BLOCKED,
    // a parked debuggee opens its GC-blocked region only when the first
    // stop-the-world pause of its hold wakes it (enrolled with the barrier
    // until then), so a hold that sees no collection deposits nothing;
    // default off.
    // Interpreter round i2 wave 68 (lane L6): with
    // CRATONVM_DEBUG_PARK_REGION_REUSES_DEPOSIT, a park region left for heap
    // work keeps the root snapshot its entry deposited when no collection
    // folded into the region, instead of refreshing it from the frames;
    // default off.
    // Interpreter round i2 wave 66 (lane L1): a wait HotSpot performs inside
    // the VM, not in a Java `wait` / `park` (`Reference.waitForReferencePendingList`,
    // the `waitForReferenceProcessing` yield, a JVMTI function waiting for
    // another thread to stop), opens `NativeContext::begin_vm_wait_region` and
    // reads RUNNABLE, as HotSpot's `_thread_blocked` inside a JVM entry does,
    // not WAITING (default on; `=0` is the kill switch).
    // Interpreter round i2 wave 67 (lane L1): the stand-in waits the wave-66
    // unrecorded-wait census listed say the kind of the JDK code they stand
    // for: `ReferenceQueue.remove` an `Object.wait` on the queue's `lock`
    // (timed for `remove(ms)`: TIMED_WAITING), the UNIX `Process.waitFor`
    // family a `ReentrantLock` condition park (timed: TIMED_WAITING), a
    // foreign `Process.waitFor(long, TimeUnit)` a sleep, and the synthetic
    // `ConcurrentHashMap` resize stripe's host-lock wait a VM-internal wait
    // (RUNNABLE) (default on; `=0` is the kill switch).
    // Interpreter round i2 wave 69 (lane L1): the `JdkLockSupport` row of the
    // `--compatible` HotSpot-alignment table (its legacy key): over a real JDK
    // the seven retired `LockSupport` stand-ins are not registered, so the JDK
    // 25 bodies run (`setBlocker` around `Unsafe.park`, `getBlocker` reading
    // `parkBlocker`), as under `--real-jdk` (default on; `=0` is the kill
    // switch, as is `CRATONVM_COMPAT_ALIGN=-jdk-lock-support`).
    E { group: Group::COMPAT, token: "compat-jdk-lock-support", on_key: Some("CRATONVM_COMPAT_JDK_LOCK_SUPPORT"), off_key: None, off_word: Some("0"), since: "2026-11-02" },
    // Interpreter round i2 wave 69 (lane L1): a VM-spawned child's `waitFor`
    // stand-ins publish ONE `ConditionObject` per process, kept in the
    // synthetic process's own slot, so every waiter of one process names the
    // same lock as on HotSpot (UNIX `ProcessImpl`'s final `condition`). Builds
    // on `CRATONVM_STAND_IN_WAIT_OBJECTS` (default on; `=0`: one per call).
    // Interpreter round i2 wave 70 (lane L1): the `JdkStampedLock` row of the
    // `--compatible` HotSpot-alignment table (its legacy key): over a real JDK
    // the 31 `StampedLock` / lock-view stand-ins are not registered, so the
    // JDK 25 bodies run (the real `state` word, `LockSupport.park(this)` on
    // the JDK's wait queue), as under `--real-jdk` (default on; `=0` is the
    // kill switch, as is `CRATONVM_COMPAT_ALIGN=-jdk-stamped-lock`).
    E { group: Group::COMPAT, token: "compat-jdk-stamped-lock", on_key: Some("CRATONVM_COMPAT_JDK_STAMPED_LOCK"), off_key: None, off_word: Some("0"), since: "2026-11-03" },
    // Interpreter round i3 wave 72 (lane L6), the park-lazy follow-ups, each
    // default OFF (`truthy_word`): a pause initiator wakes a park's running
    // phase instead of the phase polling in 100 us slices; a blocking park's
    // root deposit runs under the frozen-band memo; `Unsafe.unpark` of a
    // platform thread takes one registry lookup (that one on by default since the wave-72 triage, `=0` off).
    E { group: Group::THREADS, token: "park-lazy-stw-wake", on_key: Some("CRATONVM_PARK_LAZY_STW_WAKE"), off_key: None, off_word: Some("0"), since: "2026-11-05" },
    E { group: Group::THREADS, token: "park-deposit-frozen-band", on_key: Some("CRATONVM_PARK_DEPOSIT_FROZEN_BAND"), off_key: None, off_word: Some("0"), since: "2026-11-05" },
    // Wave 72 (lane L6): a park skips (or leaves) its running phase while a
    // JDWP / C JVMTI suspension is in force (`experimental-debug` builds).
    E { group: Group::THREADS, token: "park-lazy-suspend-aware", on_key: Some("CRATONVM_PARK_LAZY_SUSPEND_AWARE"), off_key: None, off_word: None, since: "2026-11-05" },
    // Interpreter round i3 wave 73 (lane L6): `Unsafe.unpark` of a platform
    // thread sets the permit with one swap and takes the `ParkState` mutex
    // only when the target waits; a park with the permit set takes it
    // without the mutex (HotSpot's `Parker::_counter`).
    E { group: Group::THREADS, token: "park-atomic-permit", on_key: Some("CRATONVM_PARK_ATOMIC_PERMIT"), off_key: None, off_word: Some("0"), since: "2026-11-06" },
    // Wave 73 (lane L6): `Unsafe.unpark` of a platform thread reads the
    // registry id from the mirror's `Thread.eetop` (no `java_tid` index lock).
    E { group: Group::THREADS, token: "unpark-eetop", on_key: Some("CRATONVM_UNPARK_EETOP"), off_key: None, off_word: Some("0"), since: "2026-11-06" },
    // Interpreter round i3 wave 76 (lane L6): `Unsafe.unpark` wakes the
    // park state's waiters after releasing its mutex, only when it set the
    // permit, and `Thread.interrupt` wakes the target's state once (HotSpot's
    // `Parker::unpark`).
    E { group: Group::THREADS, token: "park-wake-after-unlock", on_key: Some("CRATONVM_PARK_WAKE_AFTER_UNLOCK"), off_key: None, off_word: Some("0"), since: "2026-11-09" },
    // Interpreter round i5 wave 107 (lane L6): on Windows an untimed park
    // waits on its state's wake event (`WaitForSingleObject`, set by the
    // unpark), as HotSpot's Windows `Parker`, not on the condvar.
    E { group: Group::THREADS, token: "park-untimed-event", on_key: Some("CRATONVM_PARK_UNTIMED_EVENT"), off_key: None, off_word: Some("0"), since: "2026-10-16" },
    // Interpreter round i5 wave 108 (lane L6): on Windows a Java-level timed
    // park's wake event is raised after the waker released the park state's
    // mutex, as HotSpot's Windows `Parker::unpark` (`SetEvent` holding no lock).
    E { group: Group::THREADS, token: "park-event-signal-after-unlock", on_key: Some("CRATONVM_PARK_EVENT_SIGNAL_AFTER_UNLOCK"), off_key: None, off_word: Some("0"), since: "2026-10-16" },
    // Interpreter round i5 wave 114 (lane L1): on Windows a platform thread's
    // Java-level park returns from its wait on the wake event without
    // re-taking the park state's mutex, as HotSpot's Windows `Parker::park`.
    E { group: Group::THREADS, token: "park-event-lockless-wake", on_key: Some("CRATONVM_PARK_EVENT_LOCKLESS_WAKE"), off_key: None, off_word: Some("0"), since: "2026-10-17" },
    // Interpreter round i5 wave 109 (lane L6): on Windows a `Thread.sleep`
    // whose length is not a multiple of 10 ms raises the system timer
    // resolution to 1 ms for the wait (`timeBeginPeriod(1)`), as HotSpot's
    // Windows `PlatformEvent::park(millis)` does (`HighResolutionInterval`).
    E { group: Group::THREADS, token: "sleep-high-resolution-interval", on_key: Some("CRATONVM_SLEEP_HIGH_RESOLUTION_INTERVAL"), off_key: None, off_word: Some("0"), since: "2026-10-16" },
    // Interpreter round i3 wave 79 (lane L6): a platform thread's park spins
    // on its permit for up to this many nanoseconds (capped at 50 us; never
    // on a 1-CPU host) before its running phase waits on the condvar
    // (unset = 10000 ns since the wave-79 triage; `0` turns it off).
    E { group: Group::THREADS, token: "park-lazy-spin-ns", on_key: Some("CRATONVM_PARK_LAZY_SPIN_NS"), off_key: None, off_word: Some("0"), since: "2026-11-11" },
    // Interpreter round i3 wave 82 (lane L6): a platform thread's start takes
    // every `Arc` its registry entry holds in one read and the child adopts
    // them, instead of six `threads` write locks re-installing them (default
    // off).
    E { group: Group::THREADS, token: "thread-start-one-publish", on_key: Some("CRATONVM_THREAD_START_ONE_PUBLISH"), off_key: None, off_word: None, since: "2026-11-14" },
    // Interpreter round i3 wave 71 (lane L1): the `JdkCountDownLatch` row of
    // the `--compatible` HotSpot-alignment table (its legacy key): over a real
    // JDK the eleven `CountDownLatch` stand-in registrations are not made, so
    // the JDK 25 bodies run (the `Sync` AQS state, `LockSupport.park(this)`
    // with the `Sync` as blocker), as under `--real-jdk` (default on; `=0` is
    // the kill switch, as is `CRATONVM_COMPAT_ALIGN=-jdk-count-down-latch`).
    E { group: Group::COMPAT, token: "compat-jdk-count-down-latch", on_key: Some("CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH"), off_key: None, off_word: Some("0"), since: "2026-11-04" },
    // Interpreter round i3 wave 72 (lane L1): the `JdkAqls` row of the
    // `--compatible` HotSpot-alignment table (its legacy key): over a real JDK
    // the three `AbstractQueuedLongSynchronizer` state stand-ins (a
    // process-global side table) are not registered, so the JDK 25 bodies
    // read and CAS the real `state` field, as under `--real-jdk` (default on;
    // `=0` is the kill switch, as is `CRATONVM_COMPAT_ALIGN=-jdk-aqls`).
    E { group: Group::COMPAT, token: "compat-jdk-aqls", on_key: Some("CRATONVM_COMPAT_JDK_AQLS"), off_key: None, off_word: Some("0"), since: "2026-11-05" },
    // Interpreter round i3 wave 72 (lane L1): the `JdkCopyOnWrite` row of the
    // `--compatible` HotSpot-alignment table (its legacy key): over a real JDK
    // the `CopyOnWriteArrayList` stand-ins (which locked the list, not its
    // `lock`) are not registered, so the JDK 25 bodies run, as under
    // `--real-jdk` (default on since the wave-74 triage; `=0` or `CRATONVM_COMPAT_ALIGN=-jdk-copy-on-write`
    // turns it off).
    E { group: Group::COMPAT, token: "compat-jdk-copy-on-write", on_key: Some("CRATONVM_COMPAT_JDK_COPY_ON_WRITE"), off_key: None, off_word: Some("0"), since: "2026-11-05" },
    // Interpreter round i3 wave 74 (lane L1): the `JdkCopyOnWriteArraySet` row
    // of the `--compatible` HotSpot-alignment table (its legacy key): over a
    // real JDK none of the 24 `HashSet`-surface natives mirrored onto
    // `CopyOnWriteArraySet` is registered, so every call runs the JDK 25 body
    // with no native door or `cow_set_route` decision in front of it, as under
    // `--real-jdk` (default on since the wave-74 triage; `=0` or
    // `CRATONVM_COMPAT_ALIGN=-jdk-copy-on-write-set` turns it off).
    E { group: Group::COMPAT, token: "compat-jdk-copy-on-write-set", on_key: Some("CRATONVM_COMPAT_JDK_COPY_ON_WRITE_SET"), off_key: None, off_word: Some("0"), since: "2026-11-07" },
    // Interpreter round i3 wave 75 (lane L1): the `JdkPriorityBlockingQueue`
    // row of the `--compatible` HotSpot-alignment table (its legacy key): over
    // a real JDK the nine `PriorityBlockingQueue` stand-ins (a sorted array by
    // natural order in the heap's own fields, waits on the queue's monitor)
    // are not registered, so the JDK 25 bodies run, as under `--real-jdk`
    // (default on since the wave-75 triage; `=0` or `CRATONVM_COMPAT_ALIGN=-jdk-priority-blocking-queue`
    // turns it off).
    E { group: Group::COMPAT, token: "compat-jdk-priority-blocking-queue", on_key: Some("CRATONVM_COMPAT_JDK_PRIORITY_BLOCKING_QUEUE"), off_key: None, off_word: Some("0"), since: "2026-11-08" },
    // Interpreter round i3 wave 76 (lane L1): the `JdkConcurrentHashMap` row
    // of the `--compatible` HotSpot-alignment table (its legacy key, bit 64):
    // over a real JDK the `ConcurrentHashMap` stand-ins (a segmented VM-side
    // store the JDK's `table` never sees) and its view / iterator carriers
    // are not registered, the `Map` / `ConcurrentMap` interface doors hand a
    // CHM receiver to its own bytecode, and the JIT's direct
    // `ConcurrentMap.get` helper declines, so the JDK 25 bodies run, as under
    // `--real-jdk` (default on since the wave-76 triage; `=0` or
    // `CRATONVM_COMPAT_ALIGN=-jdk-concurrent-hash-map` turns it off).
    E { group: Group::COMPAT, token: "compat-jdk-concurrent-hash-map", on_key: Some("CRATONVM_COMPAT_JDK_CONCURRENT_HASH_MAP"), off_key: None, off_word: Some("0"), since: "2026-11-09" },
    // Interpreter round i3 wave 80 (lane L6): the `JdkProcess` row of the
    // `--compatible` HotSpot-alignment table (its legacy key, bit 65): over a
    // real JDK the `ProcessBuilder` stand-ins, the minted
    // `cratonvm/synthetic/Process*` classes and the 13 `java/lang/Process`
    // rows that served them are not registered, so `ProcessBuilder.start()`
    // builds the JDK's own `ProcessImpl`, as under `--real-jdk` (default on
    // since the wave-81 triage; `=0` or `CRATONVM_COMPAT_ALIGN=-jdk-process`
    // turns it off).
    E { group: Group::COMPAT, token: "compat-jdk-process", on_key: Some("CRATONVM_COMPAT_JDK_PROCESS"), off_key: None, off_word: Some("0"), since: "2026-11-12" },
    // Interpreter round i3 wave 80 (lane L1): the `JdkCompletableFuture` row
    // of the `--compatible` HotSpot-alignment table (its legacy key, bit 66):
    // over a real JDK the `CompletableFuture` stand-ins (`complete`,
    // `completeValue`, `completeExceptionally`,
    // `isCompletedExceptionally`, `thenCombine`, `allOf`, `anyOf`) are not
    // registered, so the JDK 25 bodies run, as under `--real-jdk` (default on since the wave-80 triage;
    // `=1` or `CRATONVM_COMPAT_ALIGN=+jdk-completable-future` turns it on).
    E { group: Group::COMPAT, token: "compat-jdk-completable-future", on_key: Some("CRATONVM_COMPAT_JDK_COMPLETABLE_FUTURE"), off_key: None, off_word: Some("0"), since: "2026-11-12" },
    // Interpreter round i3 wave 82 (lane L1): the `JdkThreadPools` row of the
    // `--compatible` HotSpot-alignment table (its legacy key, bit 67): over a
    // real JDK the `ThreadPoolExecutor` / `ScheduledThreadPoolExecutor`
    // stand-ins (routers that hand a real pool to its own JDK body) are not
    // registered, so the JDK 25 bodies run with no native door in front, as
    // under `--real-jdk` (default off; `=1` or
    // `CRATONVM_COMPAT_ALIGN=+jdk-thread-pools` turns it on).
    E { group: Group::COMPAT, token: "compat-jdk-thread-pools", on_key: Some("CRATONVM_COMPAT_JDK_THREAD_POOLS"), off_key: None, off_word: None, since: "2026-11-14" },
    // Interpreter round i6 wave 119 (lane L5): the `jdk-hashtable` alignment
    // row's own variable (`HotSpotAlignment::JdkHashtable`; `=0` turns it off).
    // default ON since interpreter round i7 (A/B w142lto)
    E { group: Group::COMPAT, token: "compat-jdk-hashtable", on_key: Some("CRATONVM_COMPAT_JDK_HASHTABLE"), off_key: None, off_word: Some("0"), since: "2026-12-21" },
    // Interpreter round i6 wave 120 (lane L5): the `jdk-logging` alignment
    // row's own variable (default OFF; `HotSpotAlignment::JdkLogging`).
    E { group: Group::COMPAT, token: "compat-jdk-logging", on_key: Some("CRATONVM_COMPAT_JDK_LOGGING"), off_key: None, off_word: Some("0"), since: "2026-12-22" },
    // Interpreter round i6 wave 122 (lane L1): the `jdk-string-get-bytes`
    // alignment row's own variable (default OFF;
    // `HotSpotAlignment::JdkStringGetBytes`, i74-L1 step 2).
    E { group: Group::COMPAT, token: "compat-jdk-string-get-bytes", on_key: Some("CRATONVM_COMPAT_JDK_STRING_GET_BYTES"), off_key: None, off_word: Some("0"), since: "2026-12-24" },
    // Interpreter round i6 wave 123 (lane L1): the `jdk-memory-usage`
    // alignment row's own variable (default OFF;
    // `HotSpotAlignment::JdkMemoryUsage`, i74-L1 step 2).
    E { group: Group::COMPAT, token: "compat-jdk-memory-usage", on_key: Some("CRATONVM_COMPAT_JDK_MEMORY_USAGE"), off_key: None, off_word: Some("0"), since: "2026-12-25" },
    // Interpreter round i6 wave 124 (lane L1): the `jdk-management-beans`
    // alignment row's own variable (default OFF;
    // `HotSpotAlignment::JdkManagementBeans`, i74-L1 step 2).
    E { group: Group::COMPAT, token: "compat-jdk-management-beans", on_key: Some("CRATONVM_COMPAT_JDK_MANAGEMENT_BEANS"), off_key: None, off_word: Some("0"), since: "2026-12-26" },
    // Interpreter round i6 wave 125 (lane L1): the `jdk-type-variable-impl`
    // alignment row's own variable (default OFF;
    // `HotSpotAlignment::JdkTypeVariableImpl`, i74-L1 step 2).
    E { group: Group::COMPAT, token: "compat-jdk-type-variable-impl", on_key: Some("CRATONVM_COMPAT_JDK_TYPE_VARIABLE_IMPL"), off_key: None, off_word: Some("0"), since: "2026-12-27" },
    // Interpreter round i6 wave 133 (lane L6): the `jdk-priority-queue`
    // alignment row's own variable (default OFF;
    // `HotSpotAlignment::JdkPriorityQueue`, i52-L5's next family).
    E { group: Group::COMPAT, token: "compat-jdk-priority-queue", on_key: Some("CRATONVM_COMPAT_JDK_PRIORITY_QUEUE"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // Interpreter round i3 wave 77 (lane L1, performance, opt-in): over a
    // real JDK with the real AQS, either mode, `ReentrantLock.lock()` /
    // `unlock()` are two reviewed `Intrinsic`s serving the uncontended and
    // reentrant shapes on the JDK's own fields (the JDK bodies run for every
    // other shape). Read once per VM by the native registry.
    // Diagnostic for the row above: the two intrinsics decline every call,
    // so a bench prices the fallback into the JDK body.
    // Interpreter round i3 wave 80 (lane L1, performance, default on since the wave-80 triage; meaningful
    // with the row above it): `ReentrantLock.lockInterruptibly()` and
    // `tryLock()` join the fast path as two more reviewed `Intrinsic`s (an
    // interrupted caller and every unserved shape run the JDK bodies).
    // Interpreter round i5 wave 102 (lane L1, performance, default off until
    // the A/B medians of `L1W102ProcessExitWakeBench`): a VM-spawned child's
    // waiter leaves with the code its OS wait object observed (no re-test), the
    // `--compatible` timed `waitFor` stand-in waits in one blocking region, and
    // the Windows `getExitCodeProcess` / `isProcessAlive` answer from one lookup.
    // Interpreter round i7 wave 136 (lane L1b, performance, default off;
    // `i135-L1b-proposal-process-wait-leftovers-of-i102` item 1): on Linux
    // 5.3+, `wait_for_handle` (the `--real-jdk` `waitForProcessExit0` reaper
    // of a VM-spawned child, `onExit()`'s waiter) polls a `pidfd_open`
    // descriptor with no mutex held and reaps once with `try_wait`, instead
    // of holding the child's mutex in `Child::wait` for the child's life.
    // Interpreter round i7 wave 139 (lane L1b, performance, default off;
    // `i135-L1b-proposal-process-wait-leftovers-of-i102` item 2):
    // `waitForProcessExit0` for a VM-spawned child waits in the lazy
    // in-native state (no root deposit on entry), as a lazy FFM downcall,
    // where the context can (`NativeContext::run_in_native_wait_lazy`).
    // Interpreter round i1 wave 50 (lane L5): the one HotSpot-alignment table
    // of `--compatible` (`types/src/hotspot_alignment.rs`), edited by a
    // comma list (`all`, `none`, `+row`, `-row`); the older
    // row switches stay as its legacy keys. And the JDK 25 bodies of
    // `AccessController.getContext` / `AccessControlContext.checkPermission`
    // over a real JDK (`security_manager.rs` `access_controller_jdk_bodies`).
    E { group: Group::COMPAT, token: "compat-align", on_key: Some("CRATONVM_COMPAT_ALIGN"), off_key: None, off_word: None, since: "2026-09-30" },
    // t1 wave 8 (lane I13): `invoke_or_native`'s first registry arm decides
    // native-vs-bytecode on the receiver's own copy of the name (correctness).
    // Interpreter round i2 wave 53 (lane L7): two positive receiver slots
    // per interpreter cast site (performance only).
    // Interpreter round i2 wave 54 (lane L7): `invokevirtual` probes the
    // signature-polymorphic route cache before its fast door when the
    // thread's site filter may hold the site (performance only).
    // Interpreter round i2 wave 54 (lane L7): the route cache's
    // instance-field arm for `VarHandle.compareAndExchange` over `int` /
    // `long` (performance only).
    // Interpreter round i2 wave 55 (lane L7): the strict `invokeExact`
    // return check answers the expected wrapper's box by class id, without
    // the class-manager lock (performance only).
    // Interpreter round i2 wave 56 (lane L7): the cast-site hit reads the
    // memo in the table entry instead of copying the site (performance only).
    // Interpreter round i2 wave 57 (lane L7): the signature-polymorphic route
    // cache's hit lends its site instead of cloning the `Arc` (performance
    // only).
    // Interpreter round i2 wave 62 (lane L7): the dispatch loop's back edge
    // re-reads the branch-profile arm only while an arm is possible and not
    // yet seen; the observer gates are hoisted and refreshed at the loop
    // top's slow path, which their writers trigger (performance only).
    // Interpreter round i3 wave 81, lane L1: the single-step / debugger gate
    // writers send the loops to the loop-top slow path, which re-reads the
    // gates there without hoisting them, so a `SingleStep` enabled while a
    // frame runs is posted from its next bytecode. Opt-in (default OFF) until
    // measured.
    // Interpreter round i4 wave 92, lane L1: a JVMTI `Exception` listener no
    // longer holds its VM off compiled code; the compiled catch doors post
    // the event, a compiled `athrow` leaves its site for the report, the
    // catch is predicted over the compiled activations, and the VM builds no
    // compiled local throw edge. Read per VM when its event manager is made.
    // Default ON since the round i4 wave-94 triage; `=0` reverts.
    // Interpreter round i6 wave 120 (lane L1, HotSpot parity, default off
    // until `L1W120JdiShadowedMethodEvents` agrees on the host): JDWP
    // `MethodEntry` / `MethodExit` of a Java method a reviewed `Intrinsic`
    // answers without running its bytecode, at index 0 and its last return
    // (i40-L1, frame-only method events).
    // Interpreter round i3 wave 82, lane L7: the dispatch loop's stack-dump
    // hook runs in the loop top's slow path, which the dump / sample requests
    // send the loops to, instead of on every bytecode (performance only;
    // default OFF until measured).
    // Interpreter round i4 wave 84, lane L7: the dispatch loop's top tests
    // one gate bit for both pending signals and the stack-dump hook
    // (performance only; default OFF until measured).
    // Interpreter round i4 wave 85, lane L2: an interpreted implicit NPE's
    // JEP 358 message is computed in `getMessage()` (getExtendedNPEMessage),
    // not at the throw (performance only; default OFF until measured).
    // Interpreter round i4 wave 84, lane L7: the static door's loader-split
    // guard answers from a per-site owner stamp in the invoke cache
    // (performance only; default OFF until measured).
    // Interpreter round i4 wave 92, lane L7: the static door's loader-split
    // guard (owner memo and owner stamp) is stamped with the class-name
    // generation instead of the class-definition epoch (performance only;
    // default OFF until measured; `VmConfig::interp_owner_memo_name_generation`).
    // Interpreter round i4 wave 94, lane L7: the native-callback memo is
    // stamped with the class-name generation, hierarchy-edge and resolution
    // epochs and the in-place provenance epoch instead of the definition /
    // provenance epochs (performance only; default OFF until measured;
    // `VmConfig::native_callback_memo_edge_epoch`).
    // Interpreter round i4 wave 95, lane L7: the per-thread memos keyed by
    // class id (by-name field slots, transient field descriptors, the
    // soft-reference touch memo) are stamped
    // with the hierarchy-edge and in-place provenance epochs instead of
    // `class_origin_epoch`, which moves on every definition (performance only;
    // default OFF until measured; `VmConfig::class_memo_narrow_epoch`).
    // Interpreter round i4 wave 94, lane L7: the special door's frameless
    // constructor call follows a chain of trivial superclass constructors up
    // to Object (performance only; default OFF until measured;
    // `VmConfig::interp_trivial_ctor_super_chain`).
    // Interpreter round i6 wave 115, lane L7: the special door's frameless
    // field-store constructor also answers `long` / `double` parameters and
    // constants (performance only; default OFF until measured;
    // `VmConfig::interp_field_ctor_wide_values`).
    // Interpreter round i6 wave 118, lane L7: the frameless field-store
    // constructor also answers numeric `ldc` / `ldc_w` / `ldc2_w` stores
    // (performance only; default ON since i7 wave 144;
    // `VmConfig::interp_field_ctor_ldc_values`).
    // default ON since interpreter round i7 (A/B w142lto)
    // Interpreter round i6 wave 119, lane L7: the special door's constructor
    // verdict lives on the call site's `CachedBytecodeMethod` (performance
    // only; default OFF until measured; `VmConfig::interp_ctor_verdict_on_entry`).
    // t1 wave 6, lane G6 (i147 cut 3): the special door's field-store
    // constructor also answers an inner / local class's constructor, stores
    // before a trivial `super()` chain (performance only; default OFF until
    // measured; `VmConfig::interp_field_ctor_pre_super_stores`).
    // t1 wave 8, lane I24 (correctness, default ON, `=0` restores the old answer): the special door's
    // frameless constructor refuses a declaring class with a finalizer, whose `<init>` return the
    // finalizer-at-`Object.<init>` hooks need (`invoke_fast::ctor_elision_keeps_finalizable`).
    // t1 wave 2 lane K (opt-in, perf): the frameless field-store constructor also runs on the Generational and
    // G1 heaps, its stores through the heap's field accessor with `op_putfield`'s barriers
    // (`field_fast::ctor_field_stores_accessor`; `VmConfig::interp_field_ctor_gen_g1`).
    // Interpreter round i6 wave 121, lane L7: stage 3 of the precise
    // interpreter oop maps; a local the verifier's map calls a non-reference
    // at the frame's mapped pcs is not a root (default OFF until the shadow
    // run; `VmConfig::interp_precise_oop_maps`).
    // Interpreter round i4 wave 94, lane L7: an owned `<clinit>` frame
    // refuses the fused array increment's static operand on its bytes
    // (performance only; default OFF until measured;
    // `VmConfig::interp_fused_array_static_operand_no_clinit`).
    // Interpreter round i4 wave 92, lane L7: a class-manager write guard's
    // release re-sweeps the held deferred-`new` retries only when a
    // definition, a resolution-epoch bump, a link / init completion or a new
    // retry moved since the last release sweep (performance only; default
    // OFF until measured).
    // Interpreter round i2 wave 63 (lane L7): the fused compare-and-branch
    // takes a warm `ldc` int bound, and the `ldc` arm stores its pc through
    // the hoisted frame (performance only, default off).
    // Interpreter round i2 wave 64 (lane L7): the `ldc` arm answers a warm
    // int / float inline, and the fused compare-and-branch takes a quickened
    // `getstatic` int bound (performance only; on by default since the wave-71 triage, `=0` off).
    // Interpreter round i7 wave 143, lane L3 (performance, default ON since the
    // round-i7 triage, `=0` off): warm reference `ldc` answered from the
    // per-thread constant-site table.
    // Interpreter round i2 wave 65 (lane L7): the `getstatic` arm answers a
    // quickened site inline, and the `ldc2_w` arm a warm long / double
    // (performance only, default off).
    // Interpreter round i2 wave 66 (lane L7): the `putstatic` arm stores to a
    // quickened site inline, and `getstatic; if<cond>` over a quickened int
    // static is fused (performance only, default off).
    // Interpreter round i2 wave 67 (lane L7): `getstatic #F; <int operand>;
    // iadd | isub; putstatic #F` on a quickened int static is fused
    // (performance only, default on since the wave-67 triage).
    // Interpreter round i2 wave 68 (lane L7): `aload_N; dup; getfield #F;
    // <int operand>; iadd | isub; putfield #F` on a quickened int instance
    // field is fused, and the fused static increment serves narrow and long
    // statics (performance only, both default on since the wave-68 triage).
    // Interpreter round i2 wave 69 (lane L7): the fused field increment
    // serves narrow and long instance fields, and both fused increments
    // decline from a byte-only screen before the out-of-line body
    // (performance only; the widening is default on since the wave-69
    // triage, the early decline stays off).
    // Interpreter round i2 wave 70 (lane L7): the fused field increment also
    // serves the assignment written without `dup`, `aload_N; aload_N;
    // getfield #F; ...; putfield #F` (`count = count + 1`; performance only,
    // on by default since the wave-72 triage, `=0` off; builds on `interp-fused-field-increment`).
    // Interpreter round i3 wave 71 (lane L7): the fused field increment also
    // serves the `dup` form whose value is used (`dup_x1` / `dup2_x1` after
    // the `getfield` or before the `putfield`: `a[size++] = x`, `return
    // ++count`; performance only; on by default since the wave-71 triage, `=0` off; builds on
    // `interp-fused-field-increment`).
    // Interpreter round i3 wave 72 (lane L7): the fused array element
    // increment `aload_N; iload_M; dup2; <x>aload; <operand>; iadd | isub;
    // [i2b | i2c | i2s]; <x>astore` (`a[i]++`, `hist[b] += k`), its value
    // forms (`dup_x2` / `dup2_x2`: `x = a[i]++`), and the group whose array is
    // already on the stack (`this.counts[i]++`; performance only; on by default since the wave-72 triage, `=0` off, was default
    // off; the last two build on `interp-fused-array-increment`).
    // ... and the same group entered at its `dup2` arm, the array and index
    // already on the stack however computed (`hist[x & 255]++`; performance
    // only, default off; builds on `interp-fused-array-increment`).
    // Interpreter round i3 wave 72 (lane L7): the fused field increment's
    // `aload_N; aload_N` group may store to another field of the same
    // receiver, `this.a = this.b + k` (performance only; on by default since the wave-72 triage; builds
    // on `interp-fused-field-increment-no-dup`).
    // Interpreter round i3 wave 73 (lane L7): the fused array element
    // accumulate `iload_S; aload_N; iload_M; <x>aload; iadd; istore_Z`
    // (`s += a[i]`; performance only, default off).
    // Interpreter round i3 wave 82, lane L7: the long and widening forms of
    // the fused array accumulate in the `lload_N` arms (performance only;
    // default OFF until measured).
    // Interpreter round i3 wave 83, lane L7: the fused static and field
    // increments' long operand also takes a warm `ldc2_w` long constant
    // (performance only; default OFF until measured; inert without the
    // `ldc2_w` inline hit and each side's wide switch).
    // Interpreter round i4 wave 89, lane L7: two fusions from the battery's
    // opcode-pair census (performance only; default OFF until measured).
    // Interpreter round i4 wave 91, lane L7: `iload_N; iconst | bipush |
    // sipush; <int op> [; istore_Z]` (performance only; default OFF until
    // measured).
    // Interpreter round i5 wave 101, lane L7: `lload; {lload | lconst_0 |
    // lconst_1}; <long op> [; lstore]` and `...; lcmp` as one dispatch
    // (performance only; default OFF until measured).
    // Interpreter round i5 wave 102, lane L7: the double twin (default off).
    // Interpreter round i5 wave 105, lane L7: the fused long group's operand
    // may be `iload; i2l` (performance only; default OFF until measured).
    // Interpreter round i5 wave 106, lane L7: `i2l; <long op> [; lstore]` over
    // the stack's long and int as one dispatch (performance only; default OFF
    // until measured).
    // Interpreter round i6 wave 115, lane L8: `iload{_N, n}; if<cond>` and
    // `aload_N; ifnull | ifnonnull` as one dispatch each (performance only;
    // default OFF until measured; `VmConfig::interp_fused_zero_branch` /
    // `interp_fused_null_branch`).
    // Interpreter round i6 wave 116, lane L8 (performance only; default OFF
    // until measured): `aload n; ifnull | ifnonnull` as one dispatch; a
    // quickened getfield hit runs the getfields after it; the dispatch loop's
    // lean loop-top instance (`VmConfig::interp_fused_null_branch_wide` /
    // `interp_fused_getfield_chain` / `interp_lean_loop_top`).
    // Interpreter round i6 wave 118, lane L8 (performance only; the switch
    // arms default ON since the wave-129 triage, `=0` off; the loop-step
    // fusion still default OFF): `tableswitch` / `lookupswitch` on the fast path; the
    // `iinc; goto` fusion runs the loop test at its target
    // (`VmConfig::interp_fast_switch_arms` / `interp_fused_loop_step_test`).
    // Interpreter round i6 wave 119, lane L8: a served load-pair `putfield`
    // runs the same pairs after it in one dispatch (performance only; default
    // OFF until measured; `VmConfig::interp_fused_store_chain`).
    // Interpreter round i6 wave 120, lane L8: the store chain's value may come
    // from `iload n` / `aload n` (performance only; default OFF until
    // measured; `VmConfig::interp_fused_store_chain_wide`).
    // Interpreter round i6 wave 115, lane L2
    // (`i100-L2-proposal-fall-through-carries-its-bail-site`): the re-tier
    // above arms only for a task whose optimizing build bailed on a row-less
    // field site of its own (performance only; default OFF until measured).
    // Interpreter round i5 wave 105, lane L2
    // (`i101-L2-pc-unknown-handler-search-matches-typed-rows-outside-their-range`):
    // a callee service (`run_jit_callee_handler`) propagates an EXPLICIT
    // throwable that carries no stamp and no precise frame -- the VM re-posted
    // it after the callee's bytecode ran and was searched at the real pc --
    // instead of searching the callee's table again with the pc unknown.
    // Parity fix, default ON; `=0` restores the pc-unknown search.
    // Interpreter round i5 wave 106, lane L2 (same page): every dispatch-error
    // re-post (`handle_jit_dispatch_error`) records that the throwable left the
    // method it was posted for, as the not-entrant re-dispatch already did, so
    // the call-site services and the doors propagate it without a pc-unknown
    // search; a compiled exit's stamp retires the record. Parity fix, default
    // ON; `=0` records the not-entrant re-dispatch only.
    // Interpreter round i3 wave 83, lane L7: the `aload n` arm fuses `aload
    // n; getfield` (a receiver past local 3) through the out-of-line
    // quickened read (performance only; default OFF until measured).
    // Interpreter round i3 wave 74 (lane L7): the fused array element
    // increment also takes the shifts and the divisions (`a[i] <<= k`,
    // `a[i] /= k`; a zero divisor declines; performance only, default on since the wave-74 triage;
    // inert without `interp-fused-array-increment`).
    // Interpreter round i3 wave 74 (lane L7): the fused array element
    // increment's local entry also serves `a[j] = a[j] + k` (no `dup2`;
    // performance only, default on since the wave-74 triage; inert without
    // `interp-fused-array-increment`).
    // Interpreter round i3 wave 75 (lane L7): the fused array element
    // increment remembers per method and pc a group its decoder refused on
    // its bytes, so the dispatch loop stops calling the helper there
    // (performance only, default on since the wave-75 triage; inert without
    // `interp-fused-array-increment`).
    // Interpreter round i3 wave 75 (lane L7): the element read `a[i]` fuses
    // when the array or the index local is past 3 (`aload_N; iload n;
    // <x>aload`, `aload n; iload{_M, n}; <x>aload`: the for-each loop's
    // hidden index; performance only, default on since the wave-75 triage).
    // Interpreter round i3 wave 76 (lane L7): the fused array element
    // increment takes another array element (`a[j] += b[k]`) or a quickened
    // static (`a[j] += SF`) as its operand (performance only, default on since the wave-76 triage;
    // inert without `interp-fused-array-increment`).
    // Interpreter round i3 wave 77 (lane L7): a per-pc miss budget for the
    // array increment's deferred operands (performance only, default off;
    // inert without `interp-fused-array-decline-memo`).
    // Interpreter round i3 wave 77 (lane L7): the array increment's element
    // operand with wide locals and a constant index expression (performance
    // only, default on since the wave-77 triage; inert without `interp-fused-array-increment-array-operand`).
    // Interpreter round i2 wave 53, lane L3: the native-call funnel marks a
    // raise and the capture lists the served JDK method at its throw.
    // Interpreter round i2 wave 54, lane L3: the served-raise mark carries a
    // native's noted JDK chain (default on), and keeps the receiver's class
    // for a virtual or interface call's selection (default on, as the
    // member-lookup family's retirement onto the mark); a JDK walk's
    // `StackFrameInfo` builds its element at the first read (default on);
    // reflection loads its members' signature types (default on since
    // 2026-10-07; `=0` turns it off).
    // zs s3/nd3: the declared reflection listings (getDeclaredFields0 / getDeclaredMethods0 / getDeclaredConstructors0, and their empty arms) LOAD their element class (Field / Method / Constructor) before allocating the result array, so the first listing of a VM that had not loaded it answers Field[] / Method[] / Constructor[] (HotSpot preloads them) instead of an array over class id 0. Every collector, both modes. Default ON, kill "0".
    // Interpreter round i2 wave 55, lane L3: `--compatible`'s native walk
    // hands out the JDK's frame classes (default on; `=0` keeps the carrier).
    // Interpreter round i2 wave 55, lane L3: a native walk's lazy
    // `StackFrameInfo` keeps its line in one `int[]` per walk, not a
    // pre-built element (default on; `=0` pre-builds it).
    // Interpreter round i2 wave 56, lane L3: `--compatible`'s generic
    // reflection hands out the JDK's `TypeVariableImpl` / `GenericArrayTypeImpl`
    // (`=0` keeps the carriers).
    // Interpreter round i2 wave 58, lane L3: `--compatible`'s declaration
    // annotations are the JDK's `AnnotationParser`'s (the VM's declaration-
    // annotation natives are not registered; the members carry their bytes).
    // Interpreter round i2 wave 61, lane L3: the served-raise families answer
    // for the method a by-name door selected (any kind), and the served
    // `Method.invoke` runs a static caller-sensitive target's adapter.
    // Interpreter round i2 wave 62, lane L3: the served-raise mark taken only
    // by its own construction's capture. (The wave's second carrier,
    // `CRATONVM_THROWABLE_SERVED_RAISE_ROW_KEY`, is retired in wave 65: the
    // reflective door's note is taken under `...DOOR_KEY` alone.)
    // Interpreter round i2 wave 63, lane L3: the keyed families' door carrier
    // (a static JDK door names the method it is entering for its call's
    // length; default off).
    // Interpreter round i2 wave 64, lane L3: a static JDK door's note takes
    // the method it selects from a per-thread memo (no class-manager read or
    // method scan per call; default off).
    // Interpreter round i2 wave 65, lane L3: a settling method handle's
    // static door keeps its served-raise note's selection on the handle
    // record (one store per call; default off).
    // Interpreter round i2 wave 66, lane L3: the --compatible carrier handle
    // keeps its settling door's served-raise selection in a per-thread side
    // line (needs the settled-door-key switch too; default off).
    // Interpreter round i3 wave 71, lane L3: the "is this class initialized"
    // test of the native, reflective and JIT slow paths answers from the VM's
    // initialized bitmap before the per-thread memo (on by default since the wave-71 triage, `=0` off).
    // Interpreter round i4 wave 92 (lane L3): `ensure_class_initialized_shared`'s
    // slow path reads the class's state and the ConstantDescs-cycle name test
    // in one class-manager read, and runs the cycle guard only in the claim
    // arm (default off).
    // Interpreter round i3 wave 72, lane L3: a by-name `invokestatic` through
    // the uninitialized owner of an inherited static answers "nothing to
    // initialize" from one superclass walk when the declaring class is
    // initialized, instead of a member resolution per call (default off).
    // Interpreter round i3 wave 73, lane L3: under --real-jdk the `Field`
    // value doors (`get` / `set` and the typed twins) are reviewed
    // `Intrinsic`s served by the natives --compatible runs, instead of the
    // JDK's `Field` bytecode and its accessor's method handle (default on since
    // wave 78, which took the 18 `Field` triples out of the retired-shadow
    // table; `=0` registers them as stubs the strict boot refuses).
    // Interpreter round i4 wave 91, lane L3: a positive by-name field answer
    // over a stub-free installed hierarchy survives class definitions in the
    // native accessors' name rings (default off; `VmConfig::field_name_ring_durable`).
    // Interpreter round i3 wave 75, lane L3: a planned static `Method.invoke`
    // target outside java/ and jdk/ dispatches through the settling static
    // door with the owner remembered on its plan, instead of the by-name
    // `invoke_by_class_id` tail per call (default on since the wave-75 triage).
    // Interpreter round i3 wave 75, lane L3: `Constructor.newInstance`'s plans
    // live in the per-VM reflect-door caches (read lock, class-name check on a
    // hit), and a public constructor skips the `exports` caller walk when it
    // cannot refuse (default on since the wave-75 triage).
    // Interpreter round i4 wave 90 (lane L3): a `Constructor` mint skips the
    // CratonVM tail's descriptor `String` when the side row holding the same
    // descriptor was filed (default off).
    // Interpreter round i4 wave 92 (lane L3): a `Constructor` mint and the
    // `getDeclaredConstructors0` listing keep the `Class[]` / `Constructor[]`
    // component ids in the per-VM reflect-door caches, and the mint's tail
    // descriptor `String` is built uninterned (default off).
    // Interpreter round i4 wave 95 (lane L3): `Class.getInterfaces0` reads the
    // mirror's stored name once, reuses its reverse-map class id, skips the
    // debug-only display name and keeps the `Class[]` component id in the
    // per-VM reflect-door caches (default off; bench L3W93ReflectionDataSplitBench
    // row getInterfaces-fresh).
    // Interpreter round i6 wave 118, lane L3 (opt-in; proposal i55-L1): core
    // reflection of a lambda proxy answers from the VM's one proxy record.
    // Interpreter round i5 wave 103 (lane L3): a throwable (and so the current
    // thread's `getStackTrace`) captured inside a `<clinit>` that a served
    // door runs lists the door's JDK class-init frames between the caller and
    // the `<clinit>`, as walks do (default on, `=0` off; probe
    // L3W103ClinitTriggerThrowableTraces).
    // Interpreter round i4 wave 94 (lane L3): the reflective door natives find
    // their caller by HotSpot's `JVM_GetCallerClass` rule over the walkers'
    // capture, the name list answering only the shapes the rule defers
    // (stage 3 of the i49-L3 caller-class proposal; default off).
    // Interpreter round i4 wave 94 (lane L3): a caller-class question sees the
    // lambda proxy frame between a method reference's call site and the
    // caller-sensitive method it names, as HotSpot does (default on, `=0`).
    // Interpreter round i4 wave 94 (lane L3): the served `Class.getMethod` /
    // `getMethods` take JDK 25's public-method walk (`getMethodsRecursive`,
    // `PublicMethods.MethodList.merge` / `getMostSpecific`) instead of their
    // two older walks (default off).
    // Interpreter round i3 wave 76, lane L3: a planned `Method.invoke` /
    // `Constructor.newInstance` reads its mirror words in one batch and builds
    // one argument vector; the boxing wrapper takes the return descriptor from
    // the plan (default on since the wave-76 triage).
    // Interpreter round i3 wave 77 (lane L3): the served `Method.invoke` /
    // `Constructor.newInstance` doors time their phases and print a per-phase
    // average to stderr every window of calls (a measurement instrument,
    // default off).
    // Interpreter round i3 wave 77 (lane L3): a `Method` plan keeps its
    // declaring class's access flags, an exact-class receiver of a planned
    // virtual call skips the two selection reads, and the boxing wrapper
    // skips the shape check of a return the body boxed itself (default on since the wave-77 triage).
    // Interpreter round i3 wave 83 (lane L3): a reflective construction of an
    // initialized class writes its defaults from the per-class allocation
    // recipe and builds its `<init>` arguments inline (default off).
    // Existing CPU-admission switch: declare it so the latched flag snapshot
    // and thread overrides can reach the AVX-512 reader in cpu_features.rs.
    E { group: Group::JIT, token: "avx512", on_key: Some("CRATONVM_AVX512"), off_key: None, off_word: Some("0"), since: "2026-10-30" },
    // Per-VM native-to-Java entry records and the allocation-known FFM callback key.
    // Interpreter round i3 wave 83 (lane L3): `interpreter::execute` shares the
    // empty exception table and probes the gate-pass memo with the key it
    // built once (default off).
    // Interpreter round i2 wave 66 (lane L5): the walk and bytes indexes open
    // each fat jar's outer archive once per VM, re-stat'ed before every reuse,
    // instead of once per nested spec (default off).
    E { group: Group::LOADER, token: "nested-jar-shared-outer", on_key: Some("CRATONVM_NESTED_JAR_SHARED_OUTER"), off_key: None, off_word: None, since: "2026-10-30" },
    // Interpreter round i2 wave 68 (lane L5): a nested-jar bytes-index row over
    // a stored nested jar holds its central directory only and reads each entry
    // from the outer file at the ask (default off).
    E { group: Group::LOADER, token: "nested-jar-bytes-directory", on_key: Some("CRATONVM_NESTED_JAR_BYTES_DIRECTORY"), off_key: None, off_word: None, since: "2026-11-01" },
    // Interpreter round i2 wave 69 (lane L5): over the directory-only rows, a
    // burst of reads keeps one outer fat-jar handle open (closed ~50 ms after
    // the burst's last read; Unix only) instead of one open/close per class
    // (default off).
    E { group: Group::LOADER, token: "nested-jar-outer-handle-reuse", on_key: Some("CRATONVM_NESTED_JAR_OUTER_HANDLE_REUSE"), off_key: None, off_word: None, since: "2026-11-02" },
    // Interpreter round i2 wave 69 (lane L5): the nested-jar walk and bytes
    // rows of a fat jar on the VM's application class path are built from the
    // class path's own bytes and parse (default off).
    E { group: Group::LOADER, token: "nested-jar-class-path-outer", on_key: Some("CRATONVM_NESTED_JAR_CLASS_PATH_OUTER"), off_key: None, off_word: None, since: "2026-11-02" },
    // Interpreter round i2 wave 63 (lane L5): the reviewed `Intrinsic` on
    // `Resource.getBytes` that answers the generic loader's `jar:nested:`
    // resource from the VM's copy of a plain nested jar (default off).
    E { group: Group::LOADER, token: "nested-jar-bytes-index", on_key: Some("CRATONVM_NESTED_JAR_BYTES_INDEX"), off_key: None, off_word: None, since: "2026-10-27" },
    // Interpreter round i2 wave 64 (lane L5): the same `Resource.getBytes`
    // intrinsic answering a `URLClassPath$JarLoader` resource over a plain
    // `file:` jar from the VM's mapping of that jar (default off).
    E { group: Group::LOADER, token: "jar-loader-bytes-index", on_key: Some("CRATONVM_JAR_LOADER_BYTES_INDEX"), off_key: None, off_word: None, since: "2026-10-28" },
    // Interpreter round i3 wave 71 (lane L5): a directory-only nested bytes
    // row whose fat jar has a kept outer handle skips its hit `stat`, the
    // pooled read checking the file instead (default off).
    E { group: Group::LOADER, token: "nested-jar-pooled-stat", on_key: Some("CRATONVM_NESTED_JAR_POOLED_STAT"), off_key: None, off_word: None, since: "2026-11-04" },
    // Interpreter round i3 wave 72 (lane L5): `ClassLoader.defineClass1/2`
    // parse the class file once -- the supertype pre-pass's parse is handed to
    // the define instead of parsing the bytes a second time (default off).
    // Interpreter round i6 wave 120 (lane L5): a `ProtectionDomain`'s
    // code-source URL decoded once per domain and GC address epoch by the
    // `defineClass*` natives (performance; default ON since the round-i7
    // triage, `=0` off; `LoaderFlags::define_pd_url_memo`).
    // Interpreter round i6 wave 133 (lane L5d): `Class.forName` with the
    // application or platform loader answers a name that loader already
    // answered a `forName` for from the VM's per-loader record, without its
    // Java `loadClass` (performance; default ON since the round-i7 user-decision
    // flip, WARNING i7-user-flip-20261022, `=0` off;
    // `LoaderFlags::builtin_for_name_record`).
    // Interpreter round i3 wave 72 (lane L5): the define pre-pass asks a
    // loader's `loadClass` for a supertype once per (loader, name), as
    // HotSpot's dictionary does, not once per define (on by default since the wave-72 triage, `=0` off).
    // Interpreter round i3 wave 73 (lane L5): the platform loader's (and a
    // null-parent loader's) `getResource` native asks only the runtime-image
    // class-path entries for its `jrt:` URL instead of building every entry's
    // URL and keeping the first `jrt:` one (same answer; default off).
    E { group: Group::IO, token: "inflater-reuse-buffers", on_key: Some("CRATONVM_INFLATER_REUSE_BUFFERS"), off_key: None, off_word: None, since: "2026-11-06" },
    // Interpreter round i3 wave 75 (lane L5): `Inflater.inflateBytesBytes`
    // hands zlib the caller's input in bounded windows, stepping again while
    // a step consumes its whole window, instead of copying the whole
    // remaining input per call (same bytes and counts; default on since the wave-75 triage).
    // Interpreter round i3 wave 74 (lane L5): a raw (`nowrap`) `Inflater`
    // keeps no ADLER-32 and answers `getAdler()` = 0, as zlib's
    // `strm->adler` of a raw stream does under HotSpot, instead of
    // checksumming every inflated byte (default on since the wave-74 triage).
    // Interpreter round i5 wave 100 (lane L5): a new `Inflater` stream's zlib
    // state goes in one of 63 lock shards of the handle table (the shard is
    // encoded in the handle), so streams inflated on different threads take
    // different locks; off, the one shared table and lock as before.
    // Interpreter round i3 wave 74 (lane L5): the `ArraysSupport.mismatch`
    // byte/char intrinsics and `vectorizedMismatch`'s element-index path
    // compare bulk payload copies instead of two `Value` reads per element
    // (same answer; default on since the wave-74 triage).
    // Interpreter round i3 wave 74 (lane L5): `ZipUtils.get16` / `get32` (every
    // CEN / LOC field read) are registered `Intrinsic` natives reading the
    // bytes in one call; an out-of-range or null argument runs the JDK body
    // (same answer; default on since the wave-74 triage).
    // Interpreter round i3 wave 75 (lane L5): the `ZipUtils` CEN / LOC header
    // accessors (`CENFLG`, `CENNAM`, ..., `LOCEXT`) are registered
    // `Intrinsic` natives answering the field in range; anything else runs
    // the JDK body (same answer; default off).
    E { group: Group::IO, token: "zip-cen-accessor-intrinsics", on_key: Some("CRATONVM_ZIP_CEN_ACCESSOR_INTRINSICS"), off_key: None, off_word: None, since: "2026-11-08" },
    // Interpreter round i3 wave 75 (lane L5): `ZipCoder$UTF8ZipCoder.compare`
    // is an `Intrinsic` comparing an all-ASCII entry name against the CEN
    // bytes in place, without the encode; anything else runs the JDK body
    // (same answer; default on since the wave-75 triage).
    // On a real JDK, keep the compare intrinsic for interpreter-only VMs and run the compiled JDK body with a JIT; =0 restores the intrinsic in both.
    // Interpreter round i3 wave 76 (lane L5): `ZipFile$Source.getEntryPos` is
    // an `Intrinsic` answering the hash walk's no-hash-match miss (`null`)
    // natively; a hash hit, a closed source or any odd layout runs the JDK
    // body (same answer; default off).
    E { group: Group::IO, token: "zip-entry-miss-intrinsic", on_key: Some("CRATONVM_ZIP_ENTRY_MISS_INTRINSIC"), off_key: None, off_word: None, since: "2026-11-09" },
    // Interpreter round i3 wave 77 (lane L5): the same `getEntryPos`
    // intrinsic (registered by this switch alone too) also answers a hash
    // match -- an ASCII exact match of the `UTF8ZipCoder` builds the
    // `EntryPos` record natively, a collision continues the walk -- so a hit
    // does not walk the chain twice; a directory match or any odd layout runs
    // the JDK body (same answer; default on since the wave-77 triage).
    // JIT round 19 proposal wave 10, lane zip10 (i75-L5): opt-in perf, default OFF. In a VM with a JIT the
    // `ZipUtils.get16` / `get32` and CEN / LOC accessor intrinsics are not registered, so the JDK's own
    // `ZipUtils` bytecode runs and compiles (no `native-shadow` refusal at the compiled `getZipEntry`'s sites);
    // `--nojit` keeps the intrinsics (native-api `NativeMethodRegistry::zip_header_reads_yield_to_jit`).
    // JIT round 19 proposal round close (2026-10-09): default ON, `=0` kill (A/B in the round record).
    // JIT round 19 proposal wave 10, lane zip10 (i73-L5): diagnostic, default OFF. With
    // CRATONVM_DBG_JIT_METHOD_STATS=1, the exit report adds `jit-read-path-census` lines: the class-path
    // read path's JDK methods (zip, jar, URLClassPath, URL, ParseUtil, ...) by invocations, with tier,
    // deopts, compile failures and the recorded refusal reason (jit `tiered::read_path_census_class`).
    // Spring AOT lane P2: a value other than 1/true/on/yes is a comma-separated list of `class.method`
    // substrings (e.g. `javac/code/Types.isSameType,javac/comp/Attr.attribClass`) that adds those methods
    // to that census (cap 200 rows) and un-cuts matching interp-frames/tierup-decline/promote-refuse rows.
    // Interpreter round i3 wave 77 (lane L5): `Reference.clear0()` clears
    // `Reference.referent` and decides the GC registry retirement on
    // `Reference`'s own slots and the `ReferenceQueue` sentinels' static
    // slots, resolved once per VM instead of by name per call (and so no
    // longer writes a subclass field that shadows the name; default on since the wave-77 triage).
    // Interpreter round i3 wave 79 (lane L5): the define pre-pass records the
    // defining user loader as an initiating loader of each supertype it
    // resolves, as HotSpot's `resolve_super_or_fail` files it in the loader's
    // dictionary, so `findLoadedClass` answers it (`--real-jdk`; default on since the wave-79 triage).
    // Interpreter round i3 wave 80 (lane L5): a new `Class` mirror takes the
    // real `java/lang/Class` layout (id, field count, slots, slot-0 overlay
    // verdict) from a per-VM memo instead of a class-manager write lock and
    // eight by-name field walks per mirror (default off; `runtime_flag_on`).
    // Interpreter round i6 wave 115 (lane L5): a new mirror reads its class's
    // module facts under the read guard that already reads its name, and asks
    // the defining-loader table once for `Class.classLoader`, `Class.module`
    // and the mirror pin (performance only; default OFF;
    // `VmConfig::class_mirror_one_loader_lookup`).
    E { group: Group::LOADER, token: "class-mirror-one-loader-lookup", on_key: Some("CRATONVM_CLASS_MIRROR_ONE_LOADER_LOOKUP"), off_key: None, off_word: None, since: "2026-12-17" },
    // Interpreter round i3 wave 81 (lane L5): the class-loader natives answer
    // "user-defined loader / bare `URLClassLoader`" for a `ClassLoader`
    // subclass from a per-VM memo keyed by class id (default on since the
    // wave-81 triage, find-loaded rows ~9% faster; `=0` reverts).
    // Interpreter round i3 wave 83 (lane L5): the class-loader natives answer
    // a user loader's namespace id from a per-VM address index valid for one
    // GC epoch, and the real-`ClassLoader`-layout verdict from a per-VM memo
    // by class id, instead of two process-global Vec scans and a by-name
    // field walk per ask (default off; `runtime_flag_on`).
    // Interpreter round i4 wave 90 (lane L5): a trusted bootstrap class keeps
    // Pass 2 and the structural scan at define and walks its methods for type
    // maps at link (with `define-type-maps-at-link`), instead of the full
    // Pass 3 HotSpot does not run for it (default off; `runtime_flag_on`).
    // --compatible boots turn it on (vm_init, compat-fast-boot `f100d3bd7e`); the ClassManager constructor default stays opt-in, so jdk-only and embeddings keep it off unless set.
    E { group: Group::LOADER, token: "define-boot-maps-at-link", on_key: Some("CRATONVM_DEFINE_BOOT_MAPS_AT_LINK"), off_key: None, off_word: None, since: "2026-10-25" },
    // Interpreter round i4 wave 91 (lane L5): a class link moves `Verifying`
    // -> `Preparing` under one class-manager write guard instead of two (one
    // write acquisition and one guard-release re-sweep fewer per link; default
    // off; `runtime_flag_on`, read once per VM).
    E { group: Group::LOADER, token: "link-state-one-write", on_key: Some("CRATONVM_LINK_STATE_ONE_WRITE"), off_key: None, off_word: None, since: "2026-11-24" },
    // Interpreter round i4 wave 93 (lane L5): a user loader's define of a
    // class naming no supertype but `java/lang/Object` skips the define
    // pre-pass's isolated-URL-loader probe; the pre-pass parses first and
    // hands the parse on (default off; `runtime_flag_on`).
    E { group: Group::LOADER, token: "define-prepass-object-only", on_key: Some("CRATONVM_DEFINE_PREPASS_OBJECT_ONLY"), off_key: None, off_word: None, since: "2026-11-26" },
    // Interpreter round i4 wave 94 (lane L5): the class store keeps a
    // `package -> class ids` index, so a built-in loader's package question
    // for a package with no loaded class is one probe, not a class-store walk
    // (default off; `runtime_flag_on`, read once per store).
    // Interpreter round i6 wave 117 (lane L5): a built-in loader's define that
    // breaks a recorded loader constraint is refused with HotSpot's
    // `LinkageError`, as a user loader's is (HotSpot parity; default OFF in
    // waves 118-133, default ON since wave 134 by user decision, `=0` off;
    // `ClassManager::define_builtin_constraint_refusal`).
    // WARNING(i6-user-flip): CRATONVM_DEFINE_BUILTIN_CONSTRAINT_REFUSAL default ON by user decision 2026-10-19; =0 restores the old path
    // Interpreter round i6 wave 133 (lane L5c): under the refusal above, a
    // built-in loader's define is admitted (counted) when the constraint's pin
    // names a VM fallback copy, a class HotSpot never makes (HotSpot parity;
    // default ON, `=0` off; inert while the refusal is off;
    // `ClassManager::define_builtin_constraint_fallback_pin`).
    // Interpreter round i4 wave 95 (lane L5): a class whose every vtable slot
    // is inherited takes a copy of the installed table it provably equals
    // (the declarer of its last slot) instead of the per-slot conversion and
    // signature-index rebuild (default off; `runtime_flag_on`, read once per
    // VM's `VtableManager`).
    E { group: Group::LOADER, token: "define-vtable-inherit-template", on_key: Some("CRATONVM_DEFINE_VTABLE_INHERIT_TEMPLATE"), off_key: None, off_word: None, since: "2026-11-28" },
    // Interpreter round i3 wave 79 (lane L5): the "worth it interpreted"
    // intrinsics (first the zip entry hit) register in a JIT VM too, and the
    // interpreter's compile gates let the JIT compile and bind the JDK body
    // behind them, so a decline reaches the compiled body (default off).
    // Interpreter round i3 wave 80, lane L4: the same, per row: a comma list of
    // row tokens (`finder-mint`, `method-type-intern`, `ste-to-string`) that
    // adopt it without the global switch. Wave 81 (lane L4): unset (and `=1`)
    // means `finder-mint` (`native-api` `INTRINSIC_BODY_COMPILES_ROWS_DEFAULT`),
    // so the row is default-ON and its off spelling is `=0` (also `none`, empty).
    // Interpreter round i2 wave 59 (lane L5): over a real JDK the launcher
    // runs `sun.launcher.LauncherHelper.checkAndLoadMain` for the main class
    // (default-ON; `=0` keeps the VM's own check).
    // Interpreter round i2 wave 60 (lane L5): with the helper, a `-jar`
    // launch passes the JAR (`LM_JAR`), so the JDK reads the manifest's
    // `Launcher-Agent-Class`, `Add-Opens` / `Add-Exports` and
    // `Enable-Native-Access` (default-ON; `=0` keeps `LM_CLASS`).
    // Interpreter round i6 wave 121 (lane L5): an `LM_JAR` launch with no
    // `-javaagent:` does not pre-load the main class, so the jar's
    // `Launcher-Agent-Class` sees it defined (HotSpot parity; default ON since
    // wave 134, `=0` off; `VmConfig::launcher_helper_jar_no_preload`).
    // Interpreter round i6 wave 117 (lane L5): a `-m` launch runs
    // `LauncherHelper.checkAndLoadMain` with `LM_MODULE`, as `java.c` does
    // (HotSpot parity; `--real-jdk`, default ON, `=0` keeps `LM_CLASS`;
    // `VmConfig::launcher_helper_module`), and the opt-in `--compatible` arm.
    // Interpreter round i3 wave 72 (lane L6): C JVMTI `GetThreadState` reads a
    // lock-free "no thread is suspended" hint before the debug-state lock
    // (default OFF).
    // Interpreter round i7 wave 135 (lane L1): the C JVMTI `GetThreadState`
    // reads the `SUSPENDED` bit from a per-thread word every suspension
    // mutator writes, not under the debug-state lock (performance switch,
    // default OFF; i64-L1 item 1, i130-L1b).
    // Interpreter round i4 wave 95, lane L7: the same from a `MethodEntry`
    // callback, for a frame an interpreter invoke pushed (the per-thread
    // agent-request word the dispatch loop reads in its slow path; i62-L1
    // stage 1). Default ON since the wave-129 triage (`=0` off).
    // Interpreter round i6 wave 134, lane L1 (i133-L8 shape 1): the same from
    // the `MethodEntry` of a contended `synchronized` entry, posted once the
    // monitor is held. Parity fix, default ON (`=0` off).
    // Interpreter round i6 wave 134, lane L1 (i133-L1): `PopFrame` /
    // `ForceEarlyReturn*` of a thread suspended while blocked entering a
    // monitor, recorded through its inspection window and taken once it holds
    // the monitor. Default OFF since wave 135 (i135-L1): HotSpot 25 answers
    // OPAQUE_FRAME for such a thread (measured), so `=1` is the divergent arm.
    // Interpreter round i7 wave 135, lane L1 (i133-L8 shape 2): the
    // `MethodEntry` of a nested dispatch loop's entry frame runs at a callback
    // point, so a `ForceEarlyReturn*` from it is served. Parity fix, default
    // ON (`=0` off).
    // Interpreter round i7 wave 137, lane L1 (i136-L1): a C agent's
    // `PopFrame` of a lambda target from its `MethodEntry` callback pops it
    // and the lambda door calls the target again, as HotSpot re-runs the
    // proxy's invoke. Parity fix, default ON (`=0` off).
    // Interpreter round i7 wave 138, lane L1 (i136-L1 residual): a C agent's
    // `PopFrame` of a compiled caller's interpreted callee from its
    // `MethodEntry` callback pops it and the JIT helpers' door calls the
    // callee again, as HotSpot deoptimizes the caller and re-runs its invoke.
    // Parity fix, default ON (`=0` off).
    // Interpreter round i7 wave 139, lane L1 (i138-L6): a JVMTI event
    // callback counts as a native frame, so Java it calls on a virtual thread
    // parks pinned, as on HotSpot. Parity fix, default ON (`=0` off).
    // Interpreter round i7 wave 138, lane L1 (i137-L1): a JDWP step counts a
    // lambda proxy method's row as a frame and completes at the proxy's
    // `return` (line -1), as HotSpot stops in the spun class's frame. Parity
    // fix, default ON (`=0` off).
    // Interpreter round i7 wave 139, lane L1 (i138-L1): a JDWP `PopFrames`
    // of a lambda's target pops it and the door calls the target again, as
    // HotSpot re-runs the proxy's invoke. Parity fix, default ON (`=0` off).
    // Interpreter round i7 wave 139, lane L1: a JDWP `PopFrames` of a
    // compiled caller's interpreted callee pops it and the JIT helpers' door
    // calls it again, as HotSpot deoptimizes the caller and re-runs its
    // invoke. Parity fix, default ON (`=0` off).
    // Interpreter round i2 wave 62, lane L1: `Instrumentation.getObjectSize`
    // and the C JVMTI `GetObjectSize` answer HotSpot's modelled sizes;
    // default-ON from interpreter round i2 wave 62 lane L1's switch-on
    // commit, `=0` turns it off.
    // Interpreter round i6 wave 133, lane L1 (i62-L6): with a C JVMTI env
    // bound and the native-excuse word carrying the callback, the funnel keeps
    // no second callback record for `GetThreadState`; a performance switch,
    // default OFF.
    // Interpreter round i2 wave 61, lane L6: a dump of many threads' stacks
    // (`Thread.getAllStackTraces`, `ThreadMXBean.dumpAllThreads`) takes one
    // frame-trace pause for all its running targets; default-ON from the
    // lane's last commit, `=0` turns it off.
    // Interpreter round i6 wave 133 (lane L7, HotSpot parity, default on,
    // `=0` off): `Thread.getThreads()`, `ThreadImpl.getThreads()` and
    // `dumpAllThreads` list the live threads in attach order, as HotSpot's
    // threads list does, not in the registry map's hash order (i102-L1 item 5).
    // Interpreter round i2 wave 56, lane L1: the in-process attach refuses
    // what HotSpot refuses (`attach` ids, `loadAgent` failures);
    // default-ON, `=0` turns it off.
    // Interpreter round i2 wave 58, lane L1: the Unix attach listener serves
    // the `load` operation (`VirtualMachine.loadAgent*` from another
    // process); default-ON from interpreter round i2 wave 59 lane L1's last commit, `=0` turns it off.
    // Interpreter round i2 wave 59, lane L1: the Unix attach listener answers
    // `properties` / `agentProperties` in HotSpot's format (`VMSupport`'s
    // serializers); default-ON from interpreter round i2 wave 59 lane L1's last commit, `=0` turns it off.
    // Interpreter round i2 wave 59, lane L1: the Unix attach listener starts
    // lazily on the client's `.attach_pid<pid>` trigger, and `SIGQUIT` no
    // longer kills the VM; default-ON from interpreter round i2 wave 59 lane L1's last commit, `=0` turns it off.
    // Interpreter round i2 wave 59, lane L1: no `jdk.attach.allowAttachSelf`
    // default where the listener serves `load` (HotSpot's `null`);
    // default-ON from interpreter round i2 wave 59 lane L1's last commit, `=0` turns it off.
    // Interpreter round i2 wave 60, lane L1: a dynamically loaded Java agent
    // prints HotSpot's `WARNING:` lines (and JPLIS's refusal line) on
    // stderr; default-ON from interpreter round i2 wave 60 lane L1's last commit, `=0` turns it off.
    // Interpreter round i3 wave 81, lane L1: a dynamic Java agent starts
    // through the JDK's `InstrumentationImpl.loadClassAndCallAgentmain` on an
    // "Attach Listener" thread, and a failed start prints JPLIS's stderr.
    // Default on. Since interpreter round i4 wave 84 (lane L1) also a startup
    // `-javaagent`'s premain (`loadClassAndCallPremain`, JPLIS's abort).
    // Interpreter round i3 wave 81, lane L1: the jvmstat perf-data file
    // `/tmp/hsperfdata_<user>/<pid>` (Unix), for `jps`, `VirtualMachine.list()`
    // and the attach provider's `testAttachable`. Default on; =0 opts out.
    // zt t10/pl2: jdk.internal.perf.Perf.attach0(<another VM's pid>) maps that VM's published perf data read-only (HotSpot's PerfMemory::attach for a foreign id: jvmstat's local monitoring, VirtualMachine.list()'s display names); Perf.detach (the JDK's cleaner) unmaps it. Every collector. Default ON, kill "0" (the zero-capacity buffer, as before).
    E { group: Group::COMPAT, token: "perf-attach-foreign", on_key: Some("CRATONVM_PERF_ATTACH_FOREIGN"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zt t11/pl3: when the perf-data file cannot be published (unsafe or unwritable hsperfdata directory), keep the image in process memory so Perf.attach(0) and the in-process counters still answer, as HotSpot 25 reverts to non-shared perf memory (create_standard_memory). Default ON since zt pl4 item 4 (2026-10-23), kill "0"; a name another VM of this process already published gets no image unless "1". Every collector.
    E { group: Group::COMPAT, token: "perf-data-in-memory-fallback", on_key: Some("CRATONVM_PERF_DATA_IN_MEMORY_FALLBACK"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zt t14/pl5: the perf-data image gets the HotSpot 25 counters it lacked that have a real source here (metaspace / class space min and max, sun.rt.createVmEndTime / vmInitDoneTime / jvmVersion / internalVersion, StatSampler's java.property / sun.property names; on ZGC the generation and space sizes, the pause collectors' lastEntryTime / lastExitTime, sun.gc.cause / lastCause). zt t15/pl6: also java.threads.started / live / livePeak / daemon, sun.cls.initializedClasses / linkedClasses / verifiedClasses / sharedClassLoadTime and, with the JIT on, java.ci.totalTime, sun.ci.totalCompiles / osrCompiles / standardCompiles / lastMethod / lastType / threads. zt t16/pl7: also the ten sun.gc.tlab.* (ThreadLocalAllocStats, published at each collection) and sun.cls.defineAppClasses / appClassBytes / unsafeDefineClassCalls. zt t18/pl8: also, with the JIT on, sun.ci.standardTime / osrTime / totalBailouts / lastFailedType / lastFailedMethod. Default on; =0: the 49-name set of zt t12/pl4.
    // zt t16/pl7 (zt t26/rt2: default ON, =0 kill, as HotSpot 25 publishes them by default). The perf-data image also gets HotSpot's class-trace timers sun.cls.classLinkedTime / classVerifyTime / classInitTime / defineAppClassTime and their .self (PerfClassTraceTime's inclusive and self-time accounting; two clock reads per link, verification, <clinit> and define), and sun.cls.time becomes their self times' sum. Off: the names are absent and sun.cls.time stays 0. Needs CRATONVM_PERF_HOTSPOT_COUNTERS.
    // zt t14/pl5: jdk.internal.perf.Perf.attach0(<id>) for an id with no perf data at all throws HotSpot's IllegalArgumentException ("Could not map vmid to user name") instead of answering a zero-capacity buffer. Default on; =0: the zero-capacity buffer, as before.
    E { group: Group::COMPAT, token: "perf-attach-missing-throws", on_key: Some("CRATONVM_PERF_ATTACH_MISSING_THROWS"), off_key: None, off_word: Some("0"), since: "2026-10-20" },
    // zt t20/pl9: sun.cls.defineAppClasses / appClassBytes also count the application loader's class-path defines (ClassManager::load_class, both modes), which HotSpot's application loader makes through defineClass1 / defineClass2. Default on; =0: the define natives' counts alone.
    // zt pl4 item 9 (2026-10-23): sun.cls.defineAppClasses / appClassBytes count the platform loader's defines and the class-path door's defines of platform- and tool-module image classes (HotSpot's platform / application loader defines them through JVM_DefineClass; the default CDS archive holds java.base classes only), which no longer count in sysClassBytes. Default ON, kill "0".
    // zt t30/cv: on Linux sun.threads.cpu_time.vm is the CPU time (CLOCK_THREAD_CPUTIME_ID, nanoseconds) the threads that ran the VM's stop-the-world operations spent in them -- every GC barrier pause (each collector's pauses, handshakes, other VM operations), booked to the requesting thread from the accepted request to complete_gc -- as HotSpot's counter is its VM thread's CPU; monotone. Windows stays 0, as HotSpot answers there. Default on; =0: a constant 0.
    // zt t21/pl10: on Windows sun.os.hrt.frequency and jdk.internal.perf.Perf.highResFrequency are the performance counter's frequency (QueryPerformanceFrequency, HotSpot's os::elapsed_frequency there; 10000000), and every tick counter and Perf.highResCounter is scaled to it. Default on; =0: nanoseconds (1000000000) on every OS. No effect elsewhere.
    E { group: Group::COMPAT, token: "perf-hrt-qpc-frequency", on_key: Some("CRATONVM_PERF_HRT_QPC_FREQUENCY"), off_key: None, off_word: Some("0"), since: "2026-10-20" },
    // Interpreter round i2 wave 60, lane L1: `SIGQUIT` prints a thread dump
    // through the attach listener's thread (Unix); default-ON from interpreter round i2 wave 60 lane L1's last commit, `=0` turns it off.
    // Interpreter round i7 wave 135, lane L1b (HotSpot parity): the VM
    // property list answers in HotSpot's measured order -- C JVMTI
    // `GetSystemProperties` (OnLoad and live) and `SystemProps$Raw.vmProperties()`
    // (`vm_properties::HOTSPOT_LIST_ORDER`; `VmConfig::vm_property_list_order`).
    // Default on; =0 off.
    // Interpreter round i7 wave 136, lane L1b (HotSpot parity):
    // `java.vm.compressedOopsMode` follows the heap size, the collector and
    // `-XX:±UseCompressedOops` as HotSpot 25's does (absent under ZGC)
    // (`vm_properties::hotspot_compressed_oops_mode`;
    // `VmConfig::vm_compressed_oops_mode_hotspot`). Default on; =0 the old
    // constant `Zero based`.
    // Interpreter round i7 wave 138, lane L1b (HotSpot parity): `-Xint` (the
    // last of `-Xint` / `-Xmixed` / `-Xcomp`) selects the `--nojit`
    // configuration and `java.vm.info` says `interpreted mode`
    // (`vm_init::vm_info_execution_mode_list`). Read by the launcher's early
    // pass from the environment, before the snapshot is latched. Default on;
    // =0 the old drop.
    // Interpreter round i2 wave 53, lane L4: run a real JDK handle's
    // `form.vmentry` before raising for it (step 2 of
    // `interpreter-L4-a-real-jdk-method-handle-answers-a-default-value-FIXED-20261020`).
    // Interpreter round i2 wave 54, lane L4: a real JDK `DirectMethodHandle`
    // over the plain `invokeStatic` / `invokeVirtual` linker is dispatched by
    // its `member` (`interpreter-L4-a-real-jdk-method-handle-answers-a-default-value-FIXED-20261020`).
    // Interpreter round i2 wave 54, lane L4: `asType` / `bindTo` of a real JDK
    // handle run the JDK's own bodies (`asType` stamped the receiver in place).
    // Interpreter round i2 wave 55, lane L4: the varargs / collector /
    // spreader adapters of a real JDK handle run the JDK's own bodies, and a
    // real `AsVarargsCollector` collects at the doors (proposal
    // `i54-L4-proposal-every-adapter-native-hands-a-real-handle-to-the-jdk`).
    // Interpreter round i2 wave 55, lane L4: a real JDK handle with a real
    // `form.vmentry` runs it before the doors' LambdaForm interpreter; a real
    // DirectMethodHandle whose entry the held generator natives fabricated is
    // dispatched by its member.
    // Interpreter round i7 wave 144, lane L1 (i144-L1): while a JDWP session
    // is attached, method-handle calls run the JDK's real forms (the
    // `DirectMethodHandle$Holder` frame a step completes in), not the direct
    // `linkTo*` lanes. Parity fix, default ON (`=0` off).
    // ... and `asCollector` / `asSpreader` of a real handle (default off).
    // Interpreter round i2 wave 56, lane L4: the held
    // `InvokerBytecodeGenerator` natives run the JDK's own generator on a
    // `lookupPregenerated` miss (proposal
    // `interpreter-L4-proposal-spin-the-lambdaforms-real-handles-need-FIXED-20261020`).
    // Interpreter round i2 wave 57, lane L4: a `VarHandle` read into a
    // primitive call site its variable does not convert to is `asType`'s
    // `WrongMethodTypeException`
    // (`interpreter-L4-a-varhandle-read-into-a-narrower-primitive-site-answers-zero-FIXED-20261021`).
    // Interpreter round i2 wave 58, lane L4: `invoke` of a handle with a
    // reference return at a primitive call site (and an `asType` copy of one
    // to a primitive return) unboxes as `asType` does at run time: `null` is
    // an NPE, a wrapper that does not widen a `ClassCastException`
    // (`interpreter-L4-a-method-handle-invoke-unboxes-a-null-or-foreign-box-to-zero-FIXED-20261022`).
    // Interpreter round i2 wave 58, lane L4: W6-1's own `VarHandle` return
    // refusal is not asked on the return path (proposal
    // `i57-L4-proposal-retire-the-w6-1-varhandle-return-text`).
    // Interpreter round i2 wave 58, lane L4: `guardWithTest`'s adapter answers
    // its target's own `MethodType` when the target's return was retyped.
    // Interpreter round i2 wave 58, lane L4: one stamp over an adapter handle is
    // judged like a cast chain (Groovy 5's mixed-width `plus`).
    // Interpreter round i2 wave 59, lane L4: a method handle's declared type
    // and parsed cast chain memoised per handle (one return plan per door).
    // Interpreter round i2 wave 59, lane L4: a handle declared `void` discards
    // its leaf's value (`dropReturn`).
    // Interpreter round i2 wave 59, lane L4: `dynamicInvoker()` typed by the
    // call site's target's `type()`.
    // Interpreter round i2 wave 60, lane L4: `--compatible` static method /
    // field handles and static VarHandles record their owner's `ClassId`,
    // as `--real-jdk`'s always did.
    // Interpreter round i2 wave 61, lane L4: method handles over a lambda
    // proxy's members carry the receiver; a serializable proxy's
    // `writeReplace()` answers its `SerializedLambda`.
    // Interpreter round i2 wave 61, lane L4: the field-accessor link under
    // `--compatible` too.
    // Interpreter round i2 wave 62, lane L4: unbound instance method handles
    // record their owner's `ClassId`; the receiver cast answers from it.
    // Interpreter round i2 wave 63, lane L4 (opt-in): field handles record
    // their owner's `ClassId`, field slots resolve by it, and the owner
    // refuses another loader's namesake receiver.
    // Interpreter round i2 wave 64, lane L4 (opt-in): an instance field handle
    // carries its owner's slot from mint; the door and the link skip the lookup.
    // Interpreter round i2 wave 64, lane L4 (opt-in): `findSpecial` handles
    // record their dispatch class's `ClassId`; the `SPECIAL` arm calls by it.
    // Interpreter round i2 wave 65, lane L4: `bindTo` of an instance field
    // handle keeps its owner's `ClassId` (and minted slot); the bound arm
    // resolves the slot by id. Kill switch `=0`.
    // Interpreter round i2 wave 65, lane L4 (opt-in): an owned field-accessor
    // link admits every subclass of the owner, not one receiver class.
    // Interpreter round i2 wave 66, lane L4 (opt-in): the field-accessor link
    // serves a bound instance field getter / setter too.
    // Interpreter round i2 wave 66, lane L4 (opt-in): the `bindTo` fast mint
    // fills lines for unbound instance field getters / setters too.
    // Interpreter round i2 wave 67, lane L4 (opt-in): `bindTo` writes the
    // bound field link on the copy it mints.
    // Interpreter round i2 wave 67, lane L4 (default on since the wave-67
    // triage): `invoke` at a linked
    // field accessor's declared type is served by the link.
    // Interpreter round i4 wave 91, lane L4 (performance, default off): a
    // linked field call judges its site through a per-thread copy of the
    // interned descriptor, not under the field-link sites lock.
    // Interpreter round i4 wave 94, lane L4 (performance, opt-in): a linked
    // synthetic field accessor is served before the real-handle lanes.
    // Interpreter round i4 wave 95, lane L4 (performance; default on since the
    // wave-97 triage, `=0` off): the JDK
    // invoker linker at a site naming an application class reuses the linker
    // member filed per basic type, with the site-type table's type.
    // Interpreter round i4 wave 95, lane L4 (performance; default on since the
    // wave-97 triage, `=0` off): a real
    // field accessor at a site of its own type is served before the linker.
    // Interpreter round i5 wave 100, lane L4 (performance, opt-in): the
    // field-before-linker lane remembers a passed site check per thread.
    // Interpreter round i5 wave 108, lane L4 (performance, opt-in): the
    // before-linker field lanes return a primitive getter value raw.
    // Interpreter round i5 wave 101, lane L4 (performance, opt-in): a bound
    // handle proven to be `bindTo` of a real instance field accessor is served
    // before the linker.
    // Interpreter round i5 wave 102, lane L4 (performance, opt-in): a real
    // static / virtual method handle at a site of its own type is called
    // directly before the linker.
    // Interpreter round i4 wave 95, lane L4 (HotSpot parity, default on, `=0`
    // off): `MethodHandleNatives.resolve` / `init` mark a `@CallerSensitive`
    // JDK method `MN_CALLER_SENSITIVE`, so the finders bind its caller.
    // Interpreter round i2 wave 68, lane L4 (default on since the wave-68
    // triage): `invoke` at a field accessor's declared type links it, as
    // `invokeExact` does.
    // Interpreter round i2 wave 68, lane L4 (opt-in): a `bindTo` fast-mint
    // hit replays the bound field pre-link the slow path wrote.
    // Interpreter round i2 wave 69, lane L4 (on by default since the wave-71 triage): the call that writes a
    // field-accessor link is served by that link, not the door.
    // Interpreter round i2 wave 69, lane L4 (default on since the wave-69
    // triage): a `bindTo` fast-mint
    // line keeps a second admitted receiver class.
    // Interpreter round i2 wave 70, lane L4 (opt-in, a pure refactor):
    // `invoke`'s field link reuses the declared type it already compared.
    // Interpreter round i2 wave 70, lane L4 (on by default since the wave-71 triage): a field-accessor link's
    // receiver-independent derivation is memoised per source shape.
    // Interpreter round i3 wave 71, lane L4 (on by default since the wave-71 triage): a finder's field
    // accessor handle shares its member strings and `type` with every other
    // finder handle of the same field.
    // Interpreter round i3 wave 71, lane L4 (on by default since the wave-71 triage): the linking call of a
    // field accessor is served from what its link attempt admitted.
    // Interpreter round i3 wave 72, lane L4 (performance; on by default since the wave-72 triage): `invoke`
    // on a real handle at a converting site takes the site's `MethodType`
    // from the bound-type table instead of re-resolving it per call.
    // Interpreter round i3 wave 79, lane L4 (correctness; default ON since
    // 2026-10-11, `0` turns it off): a boot-loader caller's converting `invoke`
    // site resolves its `MethodType` with the boot loader
    // (`MethodType.fromDescriptor(desc, null)`), not the system loader.
    // Interpreter round i3 wave 73, lane L4 (performance, opt-in): the
    // `MethodHandleNatives` field natives answer a found member from a
    // per-thread, VM- and epoch-keyed memo.
    // Interpreter round i3 wave 81, lane L4 (performance, opt-in): `resolve`
    // of a METHOD `MemberName` answers a found member from a per-thread, VM-
    // and epoch-keyed memo (key adds the caller class, lookup mode and the
    // speculative bit).
    // Interpreter round i5 wave 108, lane L3 (correctness, default on, `=0`
    // off): `resolve` settles a statically bound virtual method as
    // `REF_invokeSpecial` (HotSpot's `direct_call`).
    // Interpreter round i5 wave 100, lane L4 (performance, opt-in):
    // `MethodHandleNatives.resolve` reads only the declared methods of the
    // member's name instead of copying every class's whole method table.
    // Interpreter round i5 wave 102, lane L4 (performance, opt-in): the field
    // half -- resolve and the layout tests read only the declared fields of one name.
    // Interpreter round i5 wave 103, lane L4 (performance, opt-in): the
    // superinterface step of method resolution reads only the declared
    // methods of one name per interface.
    // Interpreter round i3 wave 81, lane L4 (correctness, default ON, kill
    // switch `=0`): the `--compatible` method finders refuse another nest's
    // private method or constructor, as HotSpot's resolution does.
    // Interpreter round i4 wave 92, lane L4 (correctness, default ON, kill
    // switch `=0`): a `PRIVATE`-mode `--compatible` Lookup asks its lookup
    // class, as HotSpot does: the field finders refuse another nest's private,
    // another package's package-private and a non-subclass's protected field
    // (`member is ...`), and the method finders another package's
    // package-private method (`no such method`).
    // Interpreter round i4 wave 92, lane L4 (correctness, both modes, default
    // ON, kill switch `=0`): a lambda proxy's mirror renders in a method
    // type as the proxy's own name, not `Object`, and the name resolves back.
    // Interpreter round i3 wave 73, lane L4 (performance, opt-in): a real
    // field accessor's `invokeExact` / `invokeBasic` is served from its
    // memoised handle record.
    // JIT round 22 wave 3, lane runtime (HotSpot parity, default ON, `=0` the
    // old answer): the before-linker lanes keep `checkCustomized`'s count and
    // leave the due call to the linker, which customizes the handle.
    // Interpreter round i3 wave 80, lane L4 (performance, default on since the wave-80 triage): `invoke` on a
    // real field getter at a boxing / widening / receiver-cast site is served
    // from its record once its `asTypeCache` holds that site's adapter.
    // Interpreter round i3 wave 74, lane L4 (performance, opt-in): a reviewed
    // `Intrinsic` over `MethodType.makeImpl` answers a type this thread
    // interned before in the same collection epoch from a per-thread memo.
    // Interpreter round i3 wave 75, lane L4: `Object.clone` answers its
    // per-class questions (`Cloneable`, side-table overlay) from a memo.
    // Interpreter round i3 wave 76, lane L4 (performance, opt-in):
    // `interpreter::execute` skips its first-call JIT gate block in a VM
    // without a JIT (no skip key allocations, no gate read locks).
    // Interpreter round i3 wave 76, lane L4 (performance, opt-in): a native
    // re-entering its own JDK body keeps the method's declared index, and the
    // re-entry skips the by-name walk and `execute`'s by-name scan.
    // Interpreter round i3 wave 77, lane L4 (performance, opt-in):
    // `interpreter::execute` names its method with the class's and the
    // method's own shared `Arc<str>`s (no per-call name copies).
    // Interpreter round i3 wave 79, lane L7 (performance, opt-in):
    // `interpreter::execute` pushes a CACHED frame from a per-VM method record
    // (no owned metadata box, no padded-code memo lock per entry; the
    // quickened stream and the fused-array decline memo live on the record).
    // Interpreter round i5 wave 114, lane L7 (performance, opt-in): a native's
    // re-entry into Java through `interpreter::execute` makes no copy its
    // compiled-body return does not read (the source file is copied only for
    // a frame that names it; the empty exception table is shared).
    // Interpreter round i6 wave 115, lane L7 (performance, opt-in):
    // `interpreter::execute` finds its method by a per-VM memo of the declared
    // index (verified by name and descriptor) instead of the linear by-name
    // scan a native re-entry into a `*$Holder` class paid on every call.
    // Interpreter round i6 wave 118, lane L7 (performance, opt-in): the
    // first-wins record of a `CONSTANT_Class` resolution is kept for every
    // real class of a non-bootstrap loader, not only an isolated URL loader's.
    // Interpreter round i6 wave 119, lane L7 (performance; default ON since
    // wave 134 by user decision, `=0` off): the
    // virtual-dispatch cache files the JDK `ClassLoader` resource body for a
    // user-loader receiver class instead of the `Bridge` native that would
    // re-enter it.
    // WARNING(i6-user-flip): CRATONVM_USER_LOADER_RESOURCE_BYTECODE default ON by user decision 2026-10-19; =0 restores the old path
    // Interpreter round i6 wave 132, lane L5b (performance, opt-in; only with
    // `user-loader-resource-bytecode`): a compiled caller's call of that body
    // enters it through the JIT helpers' frame template, not the native.
    E { group: Group::LOADER, token: "user-loader-resource-bytecode-compiled", on_key: Some("CRATONVM_USER_LOADER_RESOURCE_BYTECODE_COMPILED"), off_key: None, off_word: None, since: "2026-10-19" },
    // Interpreter round i6 wave 120, lane L7 (HotSpot parity, opt-in until
    // the battery has run it): a user loader is asked once per name; a
    // resolution's drive answers from the loader's own definitions and its
    // initiating record before calling `loadClass`.
    E { group: Group::LOADER, token: "loader-dictionary-before-drive", on_key: Some("CRATONVM_LOADER_DICTIONARY_BEFORE_DRIVE"), off_key: None, off_word: None, since: "2026-12-22" },
    // Interpreter round i6 wave 121 (lane L5): the readers of a user loader's
    // dictionary by name ask its one door (`initiating_records::loader_dictionary`)
    // (HotSpot parity; default OFF; `VmConfig::loader_dictionary_one_door`).
    E { group: Group::LOADER, token: "loader-dictionary-one-door", on_key: Some("CRATONVM_LOADER_DICTIONARY_ONE_DOOR"), off_key: None, off_word: None, since: "2026-12-23" },
    // Interpreter round i6 wave 122 (lane L5): the `Lookup`-kind user-loader
    // drives answer from the loader's dictionary through the one request
    // prologue (HotSpot parity; default OFF; `VmConfig::loader_request_door`).
    E { group: Group::LOADER, token: "loader-request-door", on_key: Some("CRATONVM_LOADER_REQUEST_DOOR"), off_key: None, off_word: None, since: "2026-12-24" },
    // Interpreter round i6 wave 124 (lane L5): a method reference's resolution
    // does not judge its failures against the built-in GUESS for a JDK-global
    // owner name a non-transparent user loader has not answered yet (HotSpot
    // parity; default ON, `=0` restores; `VmConfig::global_name_owner_unguessed`).
    // Interpreter round i6 wave 129 (lane L7): a resolution's drive of a
    // non-parallel-capable user loader re-checks the loader's dictionary for
    // every name under the loader's monitor and publishes its answer before
    // releasing it (HotSpot's `resolve_instance_class_or_null` order; parity;
    // default ON, `=0` restores the `java/`-only recheck;
    // `VmConfig::loader_drive_under_loader_lock`). Wave 130 (lane L7): the
    // natives' VM-side door (`Class.forName` and friends) too.
    // Interpreter round i6 wave 125 (lane L5): a JDK-global name a user loader
    // refused through the checked door's `BaseMiss` (its chain reaches the
    // application loader; the global route answers the built-in class) is filed
    // in the loader's initiating RECORD, which `findLoadedClass` reads, not the
    // capped memo; the degrade cases record nothing (HotSpot parity; default
    // ON, `=0` restores memo-only refusal; `VmConfig::global_name_refusal_record`).
    // Interpreter round i6 wave 133 (lane L5): a JDK-global name other than
    // `java/` that a user loader answered for the VM (the checked door's
    // successful drive, `Class.forName` with the loader, a transparent loader
    // the global route answers for) is written to its initiating RECORD, which
    // `findLoadedClass` reads (HotSpot parity; default ON, `=0` restores the
    // `java/`-only record; `VmConfig::global_name_initiating_record`).
    // Interpreter round i3 wave 76, lane L4 (opt-in): over a real JDK,
    // `--compatible` registers no `MethodType.methodType` bridges, so the four
    // factories run the JDK's bytecode (one `makeImpl`), as `--real-jdk` does.
    // Interpreter round i3 wave 77, lane L4 (performance, default on since wave
    // 78): under `--real-jdk` without a JIT, a repeat `Lookup` field finder is
    // minted from the JDK body's first answer.
    // Interpreter round i3 wave 82, lane L4 (performance, opt-in): under
    // `--real-jdk`, a repeat `Lookup` METHOD finder (`findVirtual`,
    // `findStatic`, `findSpecial`, `findConstructor`, `unreflect`) is minted
    // from the JDK body's first answer.
    // Interpreter round i4 wave 90, lane L4 (performance, opt-in): a method
    // finder mint line filed while a static method's holder was uninitialized
    // is re-filed with the handle's barrier-free form once the holder is.
    // Interpreter round i5 wave 108, lane L4 (performance, opt-in): a
    // `findStatic` / `findConstructor` of a new class is minted from the line
    // filed for another class of the same shape (loader, package, flags).
    // Interpreter round i5 wave 109, lane L4 (performance, opt-in): the field
    // and method finder mint memos (and the shape mint) also in a
    // `--compatible` VM whose `JdkInvoke` row runs the JDK's finder bodies.
    // Interpreter round i5 wave 109, lane L4 (opt-in): `invokeWithArguments` on
    // a real handle runs the JDK's body, so its spun LambdaForm frames exist.
    // Interpreter round i5 wave 114, lane L4 (performance, opt-in): the
    // `asType` intrinsic answers a real handle's `asTypeCache` hit itself.
    // Interpreter round i6 wave 115, lane L4 (opt-in): a `void` leaf behind a
    // method-handle door's reference return lets a virtual thread unmount;
    // the remounted frame's `void` return pushes the door's `null`.
    // Interpreter round i6 wave 116, lane L4 (performance, opt-in; under
    // `mh-iwa-jdk-body`): the array `invokeWithArguments` body's steps run
    // from the intrinsic, and the spread invoker is reused per epoch.
    // Interpreter round i6 wave 130, lane L4 (performance, opt-in; under
    // `mh-iwa-native-steps` + `mh-iwa-spreader-memo`): the native steps' spreader
    // and linker rows from one per-thread line.
    // Interpreter round i6 wave 132, lane L4 (parity, default on): the served
    // `invokeWithArguments` counts and customizes its `asType` adapter as the
    // spread invoker's `checkCustomized` does.
    // Interpreter round i6 wave 132, lane L4 (performance, opt-in, no-JIT VMs
    // only): `Invokers.checkCustomized` / `checkExactType` as `Intrinsic`s.
    // Interpreter round i7 wave 137, lane L4 (performance, opt-in, no-JIT VMs
    // only): `checkSpreadArgument` / `internalMemberName` as `Intrinsic`s.
    // Interpreter round i6 wave 117, lane L4 (performance, opt-in): the direct
    // method lane before the linker while no hidden frame was ever observed.
    // Interpreter round i6 wave 118, lane L4 (performance; default ON since
    // wave 134 by user decision, `=0` off): a
    // `VIRTUAL` handle's call selects its declared class by owner id.
    // WARNING(i6-user-flip): CRATONVM_MH_VIRTUAL_BY_OWNER_ID default ON by user decision 2026-10-19; =0 restores the old path
    // Interpreter round i6 wave 124, lane L4 (parity, default on): `MethodHandleNatives.init`
    // of a constructor leaves `MemberName.type` to the JDK (an interned `MethodType`).
    // Interpreter round i7 wave 135, lane L3 (performance; default ON since i7 wave 144): the record
    // intrinsic hashes/compares boxed-primitive components from their `value`.
    // default ON since interpreter round i7 (A/B w142lto)
    // t1 wave 9 (lane STRREC): correctness, default ON, kill "0". The record intrinsic reaches a reference component that
    // needs Java code through `Objects.hashCode` / `Objects.equals` (HotSpot's `ObjectMethods` frames), not a direct virtual call.
    // Interpreter round i6 wave 118, lane L4 (parity, default ON since wave 133, `=0` off;
    // `VmConfig::mh_compat_array_accessors_jdk`): `--compatible`'s array
    // accessor factories run the JDK's bodies, not the pinned bridges.
    // Interpreter round i6 wave 119, lane L4 (parity, default ON since its triage, `=0` off;
    // `VmConfig::indy_generic_via_makesite`): the generic JDK indy route links
    // through `CallSite.makeSite`, and refused lambda sites take it.
    // Interpreter round i6 wave 120, lane L4 (performance, default ON since the round i7
    // follow-up (WARNING: i7-default-flip, page i147-orch), `=0` off, `--nojit`
    // only; `VmConfig::mh_interp_basic_frame_push`): an interpreted
    // `invokeBasic` pushes its `LambdaForm` entry's frame.
    // Interpreter round i7, proposal i147-orch (performance, default ON, `=0` off, read only
    // under `mh-interp-basic-frame-push`, `--nojit` only; `VmConfig::mh_interp_special_frame_push`):
    // an interpreted `linkToSpecial` pushes its member's frame.
    // Interpreter round i8, page i147-orch (performance, default on since 2026-10-24 with `=0`
    // off -- WARNING: i8-default-flip; `--nojit` only, read under
    // `mh-interp-basic-frame-push`; `NbFlags::mh_interp_exact_bound_species_per_handle`): the
    // interpreted `invokeExact` linker push decides the bound-field lane per `Species_LL` handle.
    // Interpreter round i9, page i147-orch (performance, default on since 2026-10-24, WARNING:
    // i9-default-flip, `=0` off, `--nojit` only; `NbFlags::mh_real_ctor_cast_adapter_lane`):
    // `invokeExact` of a typed one-argument constructor's `asType` adapter at `(Object)Object`
    // checks the cast and allocates and runs `<init>` natively.
    // t1 wave 2, lane M (page t1-H-proposal-jdk-only-jit-reflective-ctor-slower-than-nojit, retired 2026-11-07): performance, opt-in A/B
    // lever (`NbFlags::mh_real_ctor_cast_adapter_lane_under_jit`): the cast-adapter lane above also serves a VM that
    // runs a JIT (it is `--nojit` only otherwise).
    E { group: Group::JIT, token: "mh-real-ctor-cast-adapter-lane-under-jit", on_key: Some("CRATONVM_MH_REAL_CTOR_CAST_ADAPTER_LANE_UNDER_JIT"), off_key: None, off_word: Some("0"), since: "2026-10-26" },
    // Interpreter round i7 wave 138, lane L4 (performance, opt-in, `--nojit`
    // only; `VmConfig::mh_iwa_interp_frame_push`): a warm interpreted
    // `invokeWithArguments` under the IWA body arm pushes the body's and the
    // linker's frames instead of the `Intrinsic`'s re-entry.
    // Interpreter round i6 wave 117, lane L4 (performance; default ON since
    // the wave-129 triage, `=0` off; `VmConfig::indy_generic_direct_invoke`): a linked generic indy calls its
    // target's `MethodHandle.invoke` native directly.
    // Interpreter round i6 wave 133, lane L2b (performance, opt-in;
    // `VmConfig::indy_generic_thread_row`): a `Linked` generic indy row is
    // copied per thread as a heap-free marker, so the next execution skips the
    // per-entry `resolution_cache` probe and the method-key hash.
    // Interpreter round i6 wave 134, lane L2c (HotSpot parity, default ON, `=0`
    // off; `VmConfig::indy_generic_keep_primitive_z`): a generic indy's `Z`
    // operand stays an `int` where the target handle's type says `boolean`.
    // Interpreter round i3 wave 82, lane L4 (performance, opt-in): under
    // `--real-jdk`, `asTypeUncached` on a direct or bound handle mints the
    // converting adapter from a filed answer two samples confirmed.
    // Interpreter round i4 wave 88, lane L4 (performance, opt-in): `invoke()` of
    // a real nullary constructor handle at an `()Object` site is served directly.
    // Interpreter round i2 wave 62, lane L4: `invoke` on a real handle whose
    // type names a class this model cannot spell runs the JDK's `asType`.
    // Interpreter round i2 wave 62, lane L4: a real static field handle
    // initializes its field's class before the access.
    // JIT round 21 lane native (`jr21-iwa-small-varargs-call-runs-the-collector-lambda-forms-FIXED-20261015.md`):
    // `invokeWithArguments` on a real `AsVarargsCollector` over a direct handle collects in place. Performance,
    // DEFAULT-ON, `=0` kill.
    // Interpreter round i2 wave 57, lane L4: `VarHandle.getMethodHandleUncached`
    // answers this model's invoker for a synthetic `VarHandle` (proposal
    // `interpreter-L4-proposal-give-synthetic-varhandles-the-jdks-method-handle-table-FIXED-20261021`).
    // Interpreter round i6 wave 123, lane L4 (parity, opt-in, `--compatible`):
    // a minted field `VarHandle` carries a real `VarForm` over an embedded
    // `cratonvm/internal/VhForm*` class (proposal `i57-L4`, stages 1-2).
    // Interpreter round i6 wave 123, lane L4 (parity, opt-in): a copyable
    // handle's `asType` / `explicitCastArguments` are judged by its `type`
    // alone; `MH_DESC` is the leaf slot (proposal `i42-L4`).
    // Interpreter round i6 wave 123, lane L4 (performance, opt-in): an
    // interpreted `invokehandle` site's successful link is remembered per
    // thread (proposal `i100-L4`, item 4).
    // Interpreter round i5 wave 100, lane L4 (HotSpot parity, default on, `=0`
    // off): `VarHandle.toMethodHandle(GET | SET)` answers a bound, uncrackable handle.
    // Interpreter round i2 wave 54, lane L4: the MethodHandle doors' `asType`
    // refusals name the call site (proposal
    // `interpreter-L4-proposal-one-astype-refusal-record-for-every-invoke-door-FIXED-20261018`).
    // Interpreter round i2 wave 55, lane L4: `MethodHandle.linkTo*` called
    // from bytecode with a concrete descriptor reaches the erased native
    // (`interpreter-L4-linkto-intrinsics-are-not-reached-from-bytecode-FIXED-20261019`).
    // Default-ON kill switch: `CRATONVM_JIT=-execute-sync` stops
    // `interpreter::execute` from taking an `ACC_SYNCHRONIZED` method's monitor
    // itself. Presence-parsed.
    // Default-ON kill switch (every mode since 2026-09-25, after the
    // `--compatible` census): a registered native that answers an
    // `ACC_SYNCHRONIZED` method runs under that method's monitor, and such
    // native entries stay out of the inline cache (`interpreter/invoke.rs`,
    // `native_sync_enabled`). `CRATONVM_JIT=-native-sync` writes `0`, which
    // disarms it; `1` / `true` re-arms it.
    // Default-ON kill switch, hence `off_key` only:
    // `CRATONVM_JIT=-code-ptr-memo` puts `jit::validate_code_ptr` back on the
    // global `Mutex` it used to take on EVERY compiled call. It exists so the
    // memo can be A/B-ed inside one binary -- a 1.5%-of-CPU symbol cannot be
    // measured across two builds on a host whose run-to-run spread is 20%.
    // See `cratonvm_jit::code_ptr_memo_enabled`.
    // `field-phases` — the same cycle breakdown for a quickened `getfield`.
    // Written after four structural explanations for the field ratio were
    // proposed from reading the code and refuted by measurement.
    // Diagnostic dump for FullHeap probe methods' handler-local liveness plan.
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
    // `nonvirtual-fast-door` — off routes every monomorphic `invokestatic`
    // and `invokespecial` cache hit through the general dispatcher.
    E { group: Group::JIT, token: "nonvirtual-fast-door", on_key: None, off_key: Some("CRATONVM_JIT_NO_NONVIRTUAL_FAST_DOOR"), off_word: None, since: "2026-09-02" },
    // `frame-slot-reuse` — off routes every frame's buffers back through
    // the thread pools on return instead of retiring the frame in place.
    // 2026-09-08. Opt-in restore of the pre-door refusal of `synchronized`
    // callees. That refusal was the single largest reason any fast door
    // declined anything -- 888,105 of 2.47 M interpreted calls on a
    // `java.text` collator workload, 36% of them -- because ICU's normaliser
    // drives `StringBuffer` one character at a time. See
    // `invoke_fast::door_sync_enabled`.
    // 2026-09-11. Off restores the per-call `resolve_method_ref` that every
    // cached-native invoke used to pay for two facts that are constants of
    // the call site: each parameter's descriptor tag and the return tag.
    // See `CachedInvokeTarget::Native::facts`.
    E { group: Group::JIT, token: "cached-native-facts", on_key: None, off_key: Some("CRATONVM_JIT_NO_CACHED_NATIVE_FACTS"), off_word: None, since: "2026-09-11" },
    // `frame-emplace` — off builds the frame on the Rust stack and moves it
    // into the slot instead of constructing it there.
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
    // JIT round 16 (lane coldpath), default ON with `=0` as the kill switch:
    // an OSR entry credits the activation's interpreted back edges to its
    // method's tier-up counter (`interpreter::osr_entry_tierup_armed`), and
    // `Class.getGenericInterfaces()`'s Signature-attribute answer is built once
    // per class and handed out as a copy, as HotSpot's `genericInfo` does
    // (`lang_class::native_class_get_generic_interfaces`).
    // JIT round 16 (lane coldpath, wave 2), default ON with `=0` as the kill
    // switch: the per-member generic reflection memo (CP16-1) and HotSpot's
    // rendering of a parameterized type's owner and a wildcard's bounds.
    E { group: Group::COMPAT, token: "generic-reflect-memo", on_key: Some("CRATONVM_GENERIC_REFLECT_MEMO"), off_key: None, off_word: Some("0"), since: "2026-10-01" },
    // `iface-select-memo` — off makes every `invokeinterface` cache hit
    // retake the class-manager read lock and rewalk the receiver hierarchy
    // to re-verify maximally-specific selection.
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
    // `field-site-cache-loader` — default-ON since interpreter round i1 wave
    // 25 (lane L5); `0` / `-field-site-cache-loader` is the kill switch, read
    // with `field-site-cache`'s spelling (`field_access.rs`).
    // JIT round 21 (lane osrreentry, page jr21-osr-early-reentry-request-rows-FIXED-20261026.md): an early OSR entry (osr-early-reentry) that ends its activation credits the schedule's threshold of back edges to the tier-up counter, so the method-entry compile is not starved (vm `interpreter::try_osr_offer`). Default ON, "0" credits only the interpreted back edges.
    // JIT round 21 (lane osrreentry, same page): the loop-work credit re-nominates the method when it carries the counter across a multiple of 64, not only when it lands on one (vm `interpreter::loop_work_retry_boundary_crossed`). Default ON, "0" restores the exact-multiple test.
    // Default-ON overflow bound on the shadow-stack push both backends emit.
    // `-shadow-end-guard` restores the pre-guard behaviour in which an
    // overrunning push stores straight on through the allocator arena — an
    // out-of-bounds write, so this is a bisection-only switch. It was read by a
    // live `getenv` until this row existed, which is precisely how a stale
    // export in a parent shell disarms a memory-safety bound with no way for
    // the launcher, `-XX:`, or a test override to see it or say otherwise.
    // gcd d2/f (found by JIT round 13): default ON. Once any shadow push has
    // bailed on that guard, its oops are published nowhere, so every later
    // moving young cycle is refused (`vm/src/jit/conservative_roots.rs`,
    // `shadow_bail_refuses_moving`). `=0` restores the old, unsound trust.
    // gcd d3/o: opt-in (default OFF). Since d3/o the refusal above is per
    // thread (only while a bailed push may still be live on a proving
    // thread's shadow stack); `=1` restores d2/f's process-wide sticky
    // refusal of every cycle after the first bail.
    // gcd d2/f: opt-in (default OFF). The single-pass prologue zeroes its
    // operand-spill band so a returned frame's leftover reference is not a
    // root of the next frame at that address (`jit/src/x64/frames.rs`,
    // `emit_prologue`).
    E { group: Group::JIT, token: "shadow-stack", on_key: Some("CRATONVM_SHADOW_STACK"), off_key: None, off_word: None, since: "2026-06-04" },
    E { group: Group::JIT, token: "sp-coalesce", on_key: None, off_key: Some("CRATONVM_SP_NO_COALESCE"), off_word: None, since: "2026-06-04" },
    // Opt-in since 2026-08-20: the `JMP`-back form reuses one native frame
    // per activation, which no other execution mode here does.
    // JIT round 18 wave 9, lane osrframe: the sibling tail call never JMPs into
    // a helper that takes the VM context (it can walk the stack while the
    // frame-chain mirror names the torn-down caller). A correctness fix,
    // default ON, so a KILL SWITCH, `"0"`.
    // JIT round 19 proposal wave 7, lane tail7 (OSRFRAME18-2): the tail form
    // back for helpers the per-helper classification proves cannot reach the
    // VM (`Never`: JMP; a fast-arm leaf twin: CALL with the frame up, return
    // the answer, decline to the ordinary call). Opt-in.
    // JIT round 19 proposal wave 7, lane tail7 (MEGA619-1 = THROWS18-6's caller
    // half): a single-pass call in return-tail position (an `xreturn` follows,
    // no VM entry before the RET) skips its post-call frame-record republish.
    // Opt-in.
    // JIT round 19 proposal wave 8, lane tail8 (TAIL7-19-1): the lazy
    // return-tail republish also across a short VM-free straight-line window
    // before the `xreturn` (`x.m(); return this;`, `return x.m() + 1;`).
    // Implies the plain return-tail form. Opt-in.
    // JIT round 19 proposal wave 8, lane tail8 (TAIL7-19-3): four more leaf
    // twins at a tail-position thin-helper call (`ThreadLocal.get`,
    // `Unsafe.compareAndSet{Int,Long}` at the instance arm,
    // `ArraysSupport.vectorizedHashCode` at the static arm). Opt-in.
    // JIT round 19 proposal wave 9, lane tail9 (TAIL819-1): a per-site hit/miss counter on both
    // arms of a single-pass guarded nested splice (`x64/inlining.rs` `emit_guarded_nested_inline`),
    // printed per site at exit as `[SP_NESTED_SPLICE_CENSUS]`. Nothing is emitted with it off.
    // JIT round 21 lane fuzzvm (OSRSPLICE21-1, `docs/feature-designs/jit-osr-exit-and-recompile.md`):
    // every int-family argument a single-pass splice stores into a callee local is checked to be
    // sign-extended (`x64/inlining.rs` `emit_dbg_splice_int_arg_check`); a violation is a SIGILL
    // crash report with rdx=0x53504c4943454931 ("SPLICEI1"). Default OFF; nothing is emitted off.
    // JIT round 19 proposal wave 11, lane tail11 (TAIL9-20-1's measurement arm): the single-pass
    // post-call RBP/identity republish stores emitted as same-length NOPs. TIMING ONLY, UNSOUND
    // (stale frame mirror); for an interleaved A/B of a non-allocating, non-throwing loop.
    // JIT round 19 proposal wave 11, lane tail11 (DEOPT10-1): the call-site service reads an
    // empty-slot trap's charge reason from the body the stash named
    // (`vm::jit::helpers::trapped_callee_deopt_reason`). Opt-in.
    // JIT round 19 proposal wave 14 (TAIL12-1): the door / first-call sinks ask the division-guard
    // re-tier too (an optimizing body entered from the interpreter). Opt-in.
    // JIT round 19 proposal wave 12, lane tail12 (r19-trapped-callee-sink-never-withdraws-a-chain-
    // trapping-splice): the helper / first-call sinks record a chain's call-site de-spec as the
    // wildcard, as the door sink does. Correctness fix, default ON; `0` restores the named record.
    // JIT round 19 proposal wave 12, lane tail12 (r19-unrolled-copies-hand-guards-the-post-call-
    // chain-snapshot): unrolled copies bind a spliced call's successor to the guard snapshot and keep
    // copied post-call snapshots post-call-only. Correctness fix, default ON; `0` restores the old copier.
    // i74-L2 (2026-10-11): a body the by-name callee door publishes under an inherited method's
    // receiver key reports the declaring class in its frames (source file, line, caller class).
    // Correctness fix, default ON; `0` restores the receiver class.
    // JIT round 19 proposal wave 14 (TAIL12-2): an unrolled copy's post-call snapshot names its own
    // cloned call, so each clone gets a chain post-call exit (needs the marks row above). Opt-in.
    // Interpreter-side like `trivial-getter`: the lock-free static-field read
    // path in `vm::vm_object`, default-ON with an opt-out-only spelling.
    // Its invokevirtual/invokeinterface twin. Same shape, same default-ON
    // opt-out-only spelling; see `env_cache::jit_virtual_bytecode_callee`.
    // The dispatch-layer half of the same feature, switched separately so the
    // templates can be priced against the kernels they sit on rather than only
    // against an un-intercepted VM.
    // `FileChannelImpl.read/write(ByteBuffer)` as one native call instead of
    // twenty JDK frames (`native-io::file_channel_fast_read`). Default-ON,
    // opt-out-only, same shape as `vector-intrinsics` above: the switch gates
    // REGISTRATION so the off arm is the un-intercepted VM.
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
    // `sc-preresolved` — dial the address an `InetSocketAddress` ALREADY holds
    // instead of re-resolving its hostname on every connect. Default-ON and
    // opt-out-only; `=0` restores the per-dial `getaddrinfo`, which is the
    // "off" arm for
    // `internal/fixed-suite-bugs/netty/blocking-connect-re-resolves-the-destination-hostname-FIXED-20260905.md`.
    // Default-**ON** (`unwrap_or(true)` in `jit::strict_callee_roots_enabled`),
    // despite the prose on that function calling it an opt-in.
    E { group: Group::JIT, token: "threshold", on_key: Some("CRATONVM_JIT_THRESHOLD"), off_key: None, off_word: None, since: "2026-06-12" },
    E { group: Group::JIT, token: "ir-frame-block-max-homes", on_key: Some("CRATONVM_JIT_IR_FRAME_BLOCK_MAX_HOMES"), off_key: None, off_word: None, since: "2026-09-29" },
    // Round 16 wave 9, lane throwpath: the capture memo's hit / miss / made /
    // refused census, printed every 4096 lookups (its positive control).
    // Round 16 wave 11, lane throw2 (TP16-12): the fill-frame trim's
    // `is_a(holder)` answers, remembered per thread. Opt-in (default OFF)
    // until measured.
    // Round 16 wave 15, lane throwfill (TP16-4's rest): a throwable capture
    // walks its compiled frames with walk-scoped lookups -- one code-range
    // snapshot per walk, a repeated return address answered once, the
    // artifact's sorted safepoint-id index, one line-census flush. The same
    // frames. Opt-in (default OFF) until measured.
    // Round 17 wave 1, lane throwfill (TF17-0): a throwable capture's time per
    // phase (walk, layout, screens and rules, appends, store), averaged and
    // printed per window of 1024 captures.
    // Round 17 wave 2, lane walk (TF17-5): a capped throwable capture walks only
    // the innermost cap-plus-slack compiled activations of each chain entry,
    // verifies the cut lies outside the kept frames, and redoes the walk whole
    // otherwise. The same trace. Opt-in (default OFF) until measured.
    // Round 19 wave 1, lane throwfill (THROWS18-1): the fused capture also lays
    // out a walk the OSR / call dedupe rewrites (the dedupe first, its bci
    // overrides laid out with the walk) instead of handing it to the plain
    // door. The same trace. Opt-in (default OFF) until measured.
    // Round 19 wave 1, lane throwfill (THROWS18-3): a walk-scoped compiled walk
    // remembers its last two return addresses, not one, so two compiled
    // methods calling each other hit the memo. The same frames. Opt-in
    // (default OFF) until measured.
    // Round 19 wave 2, lane throws2 (WALK17-4): a compiled-frame walk (a
    // throwable capture, `Thread.getStackTrace`, `StackWalker`) answers a
    // repeated (artifact, return address, safepoint-id word) activation from
    // a per-walk two-way memo. The same frames. Opt-in (default OFF) until
    // measured.
    // Round 19 wave 2, lane throws2 (WALK17-3 step 1): a `StackWalker` walk
    // defers each frame's line to the frames it writes and builds its ordered
    // list without cloning the capture. The same frames. Opt-in (default OFF)
    // until measured.
    // Round 19 wave 4, lane throws4 (THROWS3-3): an optimizing-tier direct
    // self-call's sentinel path hands the callee-deopt service a lean bit (it
    // answers "propagate" without draining the signal record when nothing is
    // stashed and the callee has no table) and, for a void / int / reference
    // result, drops the `dispatch_threw` peek. Opt-in (default OFF) until
    // measured.
    // Round 19 wave 4, lane throws4 (THROWS2-1): the whole compiled walks of
    // `Thread.getStackTrace` and `StackWalker` share each activation's inline
    // chain with the artifact (as THROWS18-2 does for the throwable capture).
    // The same frames. Opt-in (default OFF) until measured.
    // Round 19 wave 9, lane walk9 (WALK8-3): with `stackwalk-jdk-pipeline`
    // on, the native `StackWalker.forEach` runs the action through the same
    // JDK pipeline (`ReferencePipeline$Head.forEach` over the traverser), so
    // the per-frame `accept` is a compiled call instead of a native-to-Java
    // `invoke_virtual`. The same frames. Opt-in (default OFF) until measured.
    // Round 19 wave 12, lane walk12 (WALK11-1): one `StackTraceElement[]`
    // build fills the first element of each (exact class, name, method,
    // file) as before and copies it (own `lineNumber`) for every later frame
    // of that key, instead of three interns, the mirror and the origin per
    // element. The same element fields. Opt-in (default OFF) until measured.
    // JIT round 20 wave 18, lane xWalk (WALK20-2, same book): a compiled activation's / inlined level's capture entry shares the class's and method's own Arc<str> names instead of copying them (five fewer allocations per compiled frame of a StackWalker / Thread.getStackTrace capture). The same text. Default ON, kill "0".
    // JIT round 20 wave 18, lane xWalk (docs/feature-designs/real-jdk-mode.md): Class.getName() is claimed LEAF; a class's answer (the interned binary-name String) is memoized after its first call behind a global root and answered without the funnel (the same object every call, HotSpot's identity); a mirror whose name slot changed, a primitive / legacy mirror or a first call declines to the funnel. Default ON, kill "0" (the old registration, non-leaf).
    // r21 walk (JIT round 21 wave 1, lane walk; book `docs/feature-designs/jit-ir-optimizations.md`).
    // WALK21-1: a native StackWalker walk deeper than 128 frames keeps the walk-lines build (one int[]
    // of lines per 128 frames, the frame's index in its chunk) and the class kit for EVERY frame, where
    // frames past the first 128 took the eager `populate_sfi` build (three interns, a filled
    // StackTraceElement, a fresh ResolvedMethodName per frame). The same frames. Perf: opt-in.
    // WALK21-2: a pipeline StackWalker walk resolves the StackFrameTraverser class check and the
    // traverser / frame-buffer field slots once per VM and thread (keyed by vm identity and class ids)
    // instead of nine locked by-name resolutions per walk and one per fetched batch. The same slots.
    // Perf: opt-in.
    E { group: Group::JIT, token: "stackwalk-pipeline-layout-once", on_key: Some("CRATONVM_STACKWALK_PIPELINE_LAYOUT_ONCE"), off_key: None, off_word: None, since: "2026-10-18" },
    // WALK21-3 (HotSpot parity): the native walks' reflection-frame screen and the getCallerClass
    // finder rule answer JDK 25's isReflectionFrame (Method, Constructor, or assignable to
    // MethodAccessor / ConstructorAccessor) for a jdk.internal.reflect frame with a class id,
    // instead of guessing from the simple name (`*Accessor` / `*AccessorImpl`). Correctness:
    // default ON, kill "0" (the name rule).
    // WALK21-4: a batched pipeline walk takes the capture with its lines deferred and resolves a frame's
    // line when its batch builds it (written into the frame's slot of the walk's lines). Perf: opt-in.
    E { group: Group::JIT, token: "stackwalk-deferred-lines", on_key: Some("CRATONVM_STACKWALK_DEFERRED_LINES"), off_key: None, off_word: None, since: "2026-10-18" },
    // WALK21-5: the walker's option bits are read from ordinals resolved once per VM, thread and enum
    // class instead of copying each constant's name per walk. Perf: opt-in.
    // WALK21-6: a pipeline walk invokes makeStackTraverser / doStackWalk by this VM's class ids
    // (invoke_by_class_id) instead of the by-name invoke. Perf: opt-in.
    E { group: Group::JIT, token: "stackwalk-pipeline-invoke-by-id", on_key: Some("CRATONVM_STACKWALK_PIPELINE_INVOKE_BY_ID"), off_key: None, off_word: None, since: "2026-10-18" },
    // WALK21-9 (JIT round 21 wave 3): StackWalker.getCallerClass under the finder rule reads the innermost
    // frames of the walk capture (8, widened x4) instead of the whole stack with lines. Perf: opt-in.
    E { group: Group::JIT, token: "stackwalk-caller-class-tail", on_key: Some("CRATONVM_STACKWALK_CALLER_CLASS_TAIL"), off_key: None, off_word: None, since: "2026-10-19" },
    // WALK21-10 (JIT round 21 wave 3): a batched pipeline walk captures a window of the innermost frames
    // at its start and the frames below its anchor only when a fetchStackFrames runs past it. Perf: opt-in.
    E { group: Group::JIT, token: "stackwalk-window-capture", on_key: Some("CRATONVM_STACKWALK_WINDOW_CAPTURE"), off_key: None, off_word: None, since: "2026-10-19" },
    // JIT round 20 wave 11, lane pNat (WALK12-3, same book): Class.isAssignableFrom is registered LEAF (kind Bridge, stated); a resolved non-array, non-proxy (class, class) pair is answered without the funnel and the false tail's name clones, every other call declines to the old native on the funnel. Default ON, kill "0".
    // JIT round 21 lane hash (NATIVE21-1; `jr21-hash-object-hashcode-identity-arm-20261014.md`): both tiers'
    // `Object.hashCode()` box-hash prefix gets an identity arm -- a plain receiver of exactly `java/lang/Object`
    // answers its installed identity hash from the header (jit `ir_lower::identity_hash_read_bytes`); every
    // decline keeps the call. Performance, DEFAULT-ON, `=0` kill (also off under ir-identity-hash-inline=0).
    // Same lane and page: the identity arm also compares the class the site's inline cache was seeded with
    // when the VM proves its `hashCode()I` selection is `Object.hashCode` (vm `class_hash_code_is_identity`).
    // Performance, DEFAULT-ON, `=0` kill (exact-Object compare only).
    // JIT round 21 lane hash2 (`jr21-hash2-object-hashcode-class-table-20261014.md`): the identity arm also
    // answers a plain receiver of ANY class whose per-VM verdict byte says it inherits `Object.hashCode`
    // (vm `JitRealm::identity_hash_classes`, filed by the identity-hash leaves; jit
    // `ir_lower::identity_hash_class_table_test_bytes`), profile or not. Performance, DEFAULT-ON, `=0` kill.
    // JIT round 21 lane hashlock (HASH2-21-1; `jr21-native-identity-hash-crossings-pay-the-full-funnel-FIXED-20261029.md`):
    // both tiers' box-hash prefix answers an `Object`-declared `equals(Object)` inline as `this == arg` for a
    // plain receiver of exactly `java/lang/Object`, of the site's seeded class, or of any class whose per-VM
    // verdict byte says it inherits `Object.equals` (vm `JitRealm::identity_equals_classes`, filed by the
    // identity-hash leaves and the compile-time query; jit `ir_lower::identity_equals_class_table`).
    // Performance, DEFAULT-ON, `=0` kill.
    // JIT round 21 lane hashlock (`jr21-hash-hashed-object-locks-always-inflate-20261014.md`): a compact
    // instance's INFLATED mark word keeps its identity-hash bits (and a hash minted while it is locked takes the
    // short form and is recorded there), so `hashCode` of a hashed, once-locked object reads the header -- heap
    // accessor, native leaf and both tiers' compiled read -- instead of the process-wide monitor index (vm
    // `MonitorTable::inflated_mark_hash`). Performance, DEFAULT-ON, `=0` kill (INFLATED words stay bare).
    // Interpreter round i3 wave 82, lane L2: a throwable's captured frames are
    // kept in a `long[]` holder stored in its own `Throwable.backtrace` (frame
    // ids into a per-VM table), as HotSpot keeps its backtrace in the object,
    // instead of the address-keyed registry every collection sweeps; readers
    // decode the holder. A trace the holder cannot name keeps the registry.
    // The same traces. Opt-in (default OFF) until measured; read once per VM.
    // Interpreter round i3 wave 74, lane L2: under `--real-jdk` over a real
    // JDK, `StackTraceElement.of(Object, int)` (the array behind every first
    // `getStackTrace()` / `printStackTrace()`) is a reviewed `Intrinsic` that
    // fills the elements and stores each `computeFormat()` answer natively,
    // instead of the JDK body's interpreted allocation loop and per-element
    // `computeFormat()`. The same elements. Read once per VM by the native
    // registry. Default on since the wave-74 triage (`=0` reverts).
    // Interpreter round i3 wave 75, lane L2: a `StackTraceElement` the VM
    // fills for a class it filled one for before (any array, either mode)
    // copies the origin fields and interned class name from a per-VM record,
    // and `--real-jdk`'s `StackTraceElement.of` intrinsic reuses the class's
    // `computeFormat()` answer. The same elements. Read once per VM by the
    // native registry. Default on since the wave-75 triage (`=0` reverts).
    // Interpreter round i3 wave 76, lane L2: the `StackTraceElement[]` fills
    // (either mode) derive each frame's dotted class name from its captured
    // internal name instead of resolving the class by name and probing the
    // process-wide dotted-name cache; a hidden class keeps the lookup. The
    // same elements. Read once per VM by the native registry.
    // Default on since the wave-76 triage (`=0` reverts).
    // Interpreter round i3 wave 76, lane L2: under `--real-jdk` over a real
    // JDK, `StackTraceElement.toString()` is a reviewed `Intrinsic` that
    // renders the JDK body's text in Rust (the `final` class reads only its
    // own fields). The same text. Read once per VM by the native registry.
    // Default on since the wave-76 triage, registered only in a VM without a JIT.
    // Interpreter round i4 wave 91, lane L2 (page i77-L2 walk element keeps
    // interned names): a walked frame's first element stores the frame's own
    // interned `name` String instead of decoding and re-interning it. Default
    // off, read once per VM (`StackTraceFillSwitches::walk_element_interned_names`).
    // Interpreter round i3 wave 79, lane L2: under `--compatible` over a real
    // JDK, `StackFrameInfo.toStackTraceElement()` runs its JDK body (not the
    // `Bridge`), and `StackTraceElement.of(StackFrameInfo)` is the reviewed
    // `Intrinsic` `--real-jdk` uses. Read once per VM by the native registry.
    // Opt-in (default OFF) until measured.
    // Round 18 wave 10, lane throws: a `fillInStackTrace()` refresh of a built
    // throwable leaves a `null` suppressed list (suppression disabled by the
    // four-argument constructor) alone, as HotSpot does, instead of
    // installing `SUPPRESSED_SENTINEL` and turning `addSuppressed` back on.
    // A correctness fix, default ON, so a KILL SWITCH, `"0"`.
    // Round 16 wave 12, lane throw3 (TP16-14): the callee exception-table
    // verdict answered by the info's identity (the JIT cache's retire epoch
    // unchanged since it was proved) before its three names. Opt-in (default
    // OFF) until measured.
    // Round 16 wave 14, lane throw4 (throw3's backlog B4): the array and
    // `putfield` reason-10 precise NPE stubs file a trap-site key with their
    // action, as the String arm does since wave 13. A correctness fix, default
    // ON, so a KILL SWITCH, `"0"`.
    // Round 16 wave 14, lane throw4 (TP16-20): a statically bound call site a
    // compile left on the dispatch helper because its callee's class was not
    // loaded (or, for a static callee, not initialized) yet asks for one more
    // compile of its method once the dispatch helper first serves that callee
    // from a compiled body, and the displaced body is made not entrant so
    // baked callers forward to the new one. Opt-in (default OFF) until measured.
    // JIT round 23 wave 8, lane W8-L2 (CF-W8-2): OPT-IN (perf; neutral on n23-w8g, R21CfChain
    // --real-jdk pending 350/301/360 on vs 332/325/367 off). An optimizing body that left a
    // statically bound call unbound because its callee was cold waits on it, and is recompiled at
    // most once per VM when that callee is later served compiled (vm `jit_bridge.rs`
    // `note_cold_callee_late_bind_waiter`). Needs `late-bind-recompile`.
    // JIT round 18 wave 9, lane eagercold
    // (`r18-eager-static-callee-chain-compiles-uninitialized-constructors-blind-FIXED-20261006.md`):
    // the optimizing door's eager static-callee compile (d7d3a79658) compiles
    // only a callee that has run at least once (its interpreter invocation
    // counter is non-zero). A regression fix restoring the pre-d7d3a79658
    // answer for never-run callees, default ON, so a KILL SWITCH, `"0"`.
    // Same lane: a constructor splice refused only because the class was not
    // initialized yet registers the compiling method as a late-bind waiter on
    // that constructor, so the first compiled dispatch of it asks the one
    // late-bind recompile (which now splices it). Perf, opt-in, default OFF.
    // Same lane: a top-level optimizing compile (upgrade, optimizing OSR)
    // starts its own eager-chain fan-out budget instead of inheriting the last
    // by-name compile's leftover on the thread. Opt-in, default OFF.
    // JIT round 19, lane eager19 (EAGERCOLD18-1): under the background
    // compiler the ordinary eager callee chain never compiles a callee that has
    // never run (floor max(EAGER_CHAIN_MIN_INVOCATIONS, 1)); under
    // BG_COMPILE=0 the chain is unconditional. Opt-in, default OFF.
    // Same lane (EAGERCOLD18-2): an eagerly compiled callee whose invocation
    // count is below the policy's C2 minimum is compiled single-pass (C1), so
    // the tiered C1 -> C2 upgrade reaches it with a profile. Opt-in, default OFF.
    // Same lane (EAGERCOLD18-4): the interpreter fast doors fold a method's
    // FIRST invocation into the profile store at once, so "has run" is exact.
    // Opt-in, default OFF.
    // Same lane (EAGERCOLD18-5): the eager-chain fan-out budget is owned by
    // the jit crate's compile stack (reset when a depth-0 compile enters), one
    // budget per top-level compile on every door. Opt-in, default OFF.
    // Same lane (TP16-21): a synchronized callee refused because its
    // self-locking body is not published yet remembers the compiling method;
    // the self-locking body's publication asks the late-bind recompile.
    // Opt-in, default OFF.
    // Same lane (TP16-22): the optimizing (upgrade) door's own direct-bind
    // refusals that can lift (class not loaded / not initialized) remember the
    // method as a late-bind waiter too. Opt-in, default OFF.
    // Same lane (TP16-23): a by-name door publication that replaces a body
    // with different code makes the displaced body not entrant, forwarding to
    // its successor, as the C2 supersede (hm5) and the late bind already do.
    // Opt-in, default OFF.
    // JIT round 19 wave 2, lane eager2 (EAGER19-1 / EAGERCOLD18-3): an
    // optimizing compile plants an uncommon trap at a `new` whose class is
    // loaded but not initialized (instead of bailing to single-pass), and the
    // trap asks the late-bind recompile once per (method, bci). Opt-in.
    // Same lane (EAGER19-2): the eager chain compiles and reports an
    // inherited callee under its DECLARING class's key, so the tiered
    // manager's C1 -> C2 supersede reaches it. Opt-in.
    // Same lane (EAGER19-3): a late-bind pending mark is consumed only by a
    // publication whose compile started after the request; an older one
    // re-asks the recompile. Opt-in.
    // t1 wave 11 lane JTIER (TIERMGR21-2): every admission to a tier-manager in-flight slot draws a ticket the
    // request carries to its completion; only the holder's ticket releases the slot, and a completion the
    // re-created state never admitted (invalidate_class of an identity-less key while it compiled) leaves the
    // state alone (jit `tiered::tier_slot_ticket_enabled`). Correctness, default ON; `=0` restores the
    // `(tier, bci)` slot match.
    // Same lane (EAGER19-5): the optimizing door's in-place callee binder
    // uses the ordinary chain's warmth gate, traces its refusals and
    // remembers late-bind waiters. Opt-in.
    // Same lane (EAGER19-6): the late-bind republish makes the displaced
    // body not entrant only when its stub can forward
    // (`supersede_forward_admissible`), as hm5 does. Opt-in (perf).
    // JIT round 20, lane stash (EAGER2-1; `r19-late-bind-mark-is-consumed-before-a-put-that-may-refuse`):
    // a late-bind republish whose `put` did not publish the offered body (refused, raced, VarHandle-stale)
    // puts its pending mark back and re-asks the recompile instead of losing both (vm
    // `jit_bridge::late_bind_restore_after_unpublished_put`). Default ON, `0` kills.
    // JIT round 20, lane stash (EAGER2-2; `r19-late-bind-pending-marks-outlive-a-refused-retry`): a late-bind
    // pending mark is kept only when the C2 retry door enqueued the recompile or found the method already
    // queued; a refused retry withdraws it (vm `jit_bridge::late_bind_mark_and_request`). Default ON, `0` kills.
    // Tiered compilation contract (docs/feature-designs/wire-tiered-manager.md): a late-bind C2 retry answered
    // `AlreadyQueued` is owed to the method and asked again once the task holding the slot finishes (jit
    // `tiered::MethodState::late_bind_retry_owed`); the task may have checked its pending mark before the mark was
    // set. Default ON, `0` kills.
    // t1 wave 11 lane JTIER (review): a method-entry request DROPPED before it compiled (stale epoch, overtaken
    // by an optimizing publish) hands the late-bind owed bit on as a `late-bind-owed` retry once the slot is free
    // (jit `tiered::CompilerCore::take_dropped_owed_retry`); before, only a compiled task's completion took it,
    // and the retry was lost. Correctness, default ON, `0` kills.
    // JIT round 20, lane stash (`r21-supersede-redirect-residuals`, item 1; perf): the callee publication door
    // arms the JIT cache's not-entrant hook before its `put`, so the supersede redirect patches a displaced body
    // from the first publication on instead of after the VM's first redefinition / trap eviction / C2 supersede
    // (vm `jit_bridge::arm_not_entrant_at_publish`). Opt-in (`1`): neutral in the round-20 A/B (R16AllocPremium, 5 reps).
    // JIT round 19 wave 3, lane eager3 (EAGER2-3): a real class-initialized
    // guard at an uninitialized `new` (the resolved allocation behind
    // Guard(ClassInitTest)), instead of EAGER19-1's unconditional trap. Opt-in.
    // Same lane: the eager-chain startup / compile-cost census, printed at exit
    // (compiles by door and tier, refusals by rule, how many ever ran).
    // The CharSequence->String intrinsic. DEFAULT-OFF as of 2026-08-24, so a
    // plain opt-in row rather than a kill switch.
    // Receiver de-speculation. DEFAULT-ON, so a KILL SWITCH: `=0` restores the
    // old behaviour. Same `off_word` shape as `osr-coverage-shadow` and
    // `xt-jit-coverage-handshake`, which are the other two default-ON rows.
    // Numeric: the de-speculation spare factor, default 2. A VALUE knob, like
    // `threshold` above -- the token carries a number, not an on/off.
    // Elide the SB-CRASH-04 full-GPR blind spill at a direct call whose caller
    // frame is provably oop-clean. Multi-valued, not a boolean: `0` never
    // elides (the pre-2026-08-26 behaviour), `args`/`2` and the default `3`
    // widen it -- so `off_word` is `0` and the token carries the mode.
    // Narrow the safepoint blind spill to the registers that can hold an oop.
    // DEFAULT-ON, so a KILL SWITCH: `=0` restores the full-GPR spill. Same
    // off_word shape as `call-spill-elision` beside it.
    // A call site's argument staging IS the publication of its argument oops,
    // so the pre-safepoint spill of those registers is a duplicate. DEFAULT-ON,
    // hence a KILL SWITCH: `=0` restores the full selection at the staged sites.
    // Opt-in PER SITE in the compiler, never globally -- an allocation site, a
    // safepoint poll, or an invoke shape that did not stage keeps the full spill
    // because it has not published anything. Declared here after
    // `flag_declaration_guard` caught it reading through a live `getenv`.
    // gcg g3/v: opt-in. Each single-pass oop map publishes the blind-spill slots its
    // safepoint's spills wrote; the band scan skips the others (jit/src/x64/safepoint.rs,
    // vm/src/jit/conservative_roots.rs).
    // Default-ON kill switches from the same wave: each reads
    // `runtime_var(..).map(|v| v != "0").unwrap_or(true)` or the `0`/`false`/
    // `off` spelling of it, so `=0` is the off word and there is no on_key
    // semantics beyond presence.
    // Presence-only, and named as a NEGATIVE, so it is an off_key with no on
    // spelling -- the same shape as `no-atomic-intrinsic` above it.
    // Compile-worker counts for the tiered manager's two lanes
    // (`tiered::compiler_thread_counts`): C1 defaults to 1 worker, C2 to
    // max(1, log2(cpus)); either is clamped to 1..=64.
    E { group: Group::JIT, token: "tier-c1-threads", on_key: Some("CRATONVM_TIER_C1_THREADS"), off_key: None, off_word: None, since: "2026-09-12" },
    E { group: Group::JIT, token: "tier-c1-threshold", on_key: Some("CRATONVM_TIER_C1_THRESHOLD"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "tier-c2-min-invocations", on_key: Some("CRATONVM_TIER_C2_MIN_INVOCATIONS"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "tier-c2-threads", on_key: Some("CRATONVM_TIER_C2_THREADS"), off_key: None, off_word: None, since: "2026-09-12" },
    // zs s9/rs2: with the switch (`1`/`on`) only the first C2 compile worker serves the queue from the start; the others are spawned parked (allocating nothing, so owning no allocator pages) and each is promoted for good when an enqueue leaves at least CRATONVM_TIER_C2_STANDBY_DEPTH (default 4, floor 1) requests queued on the C2 lane. Read once per compiler start. Opt-in: the host's boot RSS / CPU rows decide (zs s9/rs2 report).
    E { group: Group::JIT, token: "tier-c2-standby", on_key: Some("CRATONVM_TIER_C2_STANDBY"), off_key: None, off_word: None, since: "2026-10-16" },
    E { group: Group::JIT, token: "tier-c2-standby-depth", on_key: Some("CRATONVM_TIER_C2_STANDBY_DEPTH"), off_key: None, off_word: None, since: "2026-10-16" },
    // zt t2/fp: with CRATONVM_TIER_C2_STANDBY, the backlog that promotes the next parked C2 worker is CRATONVM_TIER_C2_STANDBY_DEPTH times the workers already live (2 x depth for the second, ...), the shape of HotSpot's UseDynamicNumberOfCompilerThreads; without it every promotion waits for the depth alone. Read once per compiler start. Opt-in (`1`/`on`): the zt t2/fp report's ST rows decide.
    E { group: Group::JIT, token: "tier-c2-standby-scale", on_key: Some("CRATONVM_TIER_C2_STANDBY_SCALE"), off_key: None, off_word: None, since: "2026-10-19" },
    E { group: Group::JIT, token: "tier-c2-threshold", on_key: Some("CRATONVM_TIER_C2_THRESHOLD"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "tier-osr-backedge", on_key: Some("CRATONVM_TIER_OSR_BACKEDGE"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "tier-osr-threshold", on_key: Some("CRATONVM_TIER_OSR_THRESHOLD"), off_key: None, off_word: None, since: "2026-06-22" },
    E { group: Group::JIT, token: "tier-pgo", on_key: Some("CRATONVM_TIER_PGO"), off_key: None, off_word: None, since: "2026-06-22" },
    // `tier-pgo` turns profiling on for a WINDOW; this turns it on for the
    // whole run, which is what a branch profile needs to be worth reading at
    // the moment a method tiers up. Affordable since the counters went
    // lock-free (measured 0.6% on an always-on run), and default OFF only
    // until that measurement has been repeated on a soak.
    // Presence-tested (`runtime_var_os(..).is_some()`), so any value turns it on.
    E { group: Group::JIT, token: "tiered", on_key: Some("CRATONVM_TIER_ENABLED"), off_key: None, off_word: Some("0"), since: "2026-06-22" },
    // Interpreter-side, but it lives with the execution-engine knobs like
    // `rootsnap-cache`. Default-ON; `0`/`off`/`false`/`no` is the kill switch.
    // Two readers: `CRATONVM_JIT_UNROLL` is the optimizing tier's unroller
    // (`ir_optimize::unroll_enabled`, `runtime_flag_default_on`), and
    // `CRATONVM_DISABLE_UNROLL` the single-pass loop paths'
    // (`x64/loop_rewrite.rs`, `x64/loop_unroll_admission.rs`). `-unroll` turns
    // both off: it sets the opt-out key and writes `off_word` "0" to the
    // default-ON one (round 18 wave 11, lane osrfollow;
    // `r18-unroll-minus-token-leaves-the-ir-unroller-on-FIXED-20261006.md`).
    E { group: Group::JIT, token: "unroll", on_key: Some("CRATONVM_JIT_UNROLL"), off_key: Some("CRATONVM_DISABLE_UNROLL"), off_word: Some("0"), since: "2026-05-22" },
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
    E { group: Group::JIT, token: "verify-schedule", on_key: Some("CRATONVM_JIT_VERIFY_SCHEDULE"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::JIT, token: "verify-types", on_key: Some("CRATONVM_JIT_VERIFY_TYPES"), off_key: None, off_word: None, since: "2026-07-31" },
    // Credit the pinned-peer depth even on a collector that cannot honour the
    // pin. Default OFF, which is the corrected behaviour; setting it restores
    // the ten-second H2 SIGSEGV, so the fix has a positive control rather than
    // only an absence of crashes.
    // gcd d2/i: a peer blocked in the compiled `monitorenter` helper answers
    // with its blocking deposit's coverage proof instead of as a helper
    // window. Opt-in (default OFF).
    // Default-ON since 2026-09-05, so it takes an `off_word`: presence alone no
    // longer decides it and `=0` has to be able to turn it off.
    // Default-ON kill switch as of 2026-09-08, hence `off_key` only: the JIT
    // half of a blocked-region wake (active compiled frames, register images,
    // shadow stack). It shipped opt-in and wired only into the LEAKED-region
    // fallback, so the wake that actually runs remapped interpreter frames and
    // nothing compiled -- a peer that blocked with compiled frames below it
    // resumed with every JIT oop at its pre-move address.
    // Default-ON kill switch, hence `off_key` only: a blocked peer's
    // conservatively-scanned native-stack words are written back on wake. The
    // objects were kept alive AND relocated while it slept, nothing else
    // rewrites those words, and the pin that nominally protected them is a
    // no-op on a Cheney copy.
    // Default-ON since 2026-09-08. It shipped opt-in, and the helper-window
    // DISCHARGE (default-on, same family) then made the pin load-bearing: a
    // discharged cycle relocates on the strength of "every window pinned", and
    // `is_heap_addr` -- the predicate it used to pin with -- drops a misaligned
    // interior pointer and a one-past-the-end cursor, which are the two shapes a
    // compiled loop leaves in a frozen peer's registers. `0` is the kill switch.
    // Root a frozen peer's derived pointers (interior / one-past-the-end
    // cursors) in the TAKE-OVER pass, not only exact object bases. Shipped
    // opt-in by gc-common w1-c; DEFAULT-ON since w2-c (same day), so a kill
    // switch with `off_word: "0"`. Argument on the page
    // docs/feature-designs/real-jdk-mode.md.
    // gcd d10/t (`common-c-linux-takeover-signals-every-thread-FIXED-20260929`): opt-in, the
    // Linux take-over signals only roster threads whose published JIT depth is
    // not zero (a thread with no compiled frame cannot be in compiled code),
    // for the first passes of a take-over; later passes signal the whole
    // roster. `CRATONVM_XT_ROOT_SCAN_AUDIT=1` checks the rule either way
    // (`JIT-ONLY SIGNAL MISS`).
    // JIT round 17 wave 6 (lane eintr2): JNI `AttachCurrentThread` takes the
    // take-over signal (`SIGUSR2`) out of the attaching thread's mask, as
    // HotSpot's attach does. Default ON (correctness), `=0` kills it.
    // Kill switch for the `process_vm_readv` peer-stack reader, so the reader
    // and the historical direct load are A/B-able inside one binary. Opt-in:
    // setting it restores the pre-fix behaviour exactly, SIGSEGV included.
    // Value token, milliseconds: `CRATONVM_JIT=xt-peer-deadline-ms=50`. Unset
    // means the built-in 20 ms, and `0` is rejected by the parser's own filter,
    // so there is no off state to spell.
    // gce e2/t: wait this many microseconds for cooperative arrivals before the
    // first take-over pass; a met quota runs one pass over the uncounted roster.
    // gci i1/fr: opt-in. A blocked peer whose published JIT depth is 0 is not a helper window (pure-native path; captures kept) -- gcg-g3h item 1.
    // gci i20/vh: G1 only, default ON since gci i24/jp (`=0` kills). A blocked peer whose deposit deferred a complete compiled chain is not a helper window (captures kept, no pins) -- gci-i20vh parked-carrier over-pin.
    // WARNING gci-i24jp-flip-20261015: default ON on G1 since 2026-10-15 (i24/jp); `=0` kills.
    // Covered-chain helper windows pin their candidates without retaining dead-frame residue as marking roots. Existing default-on reader, declared for group routing and per-VM overrides.
    // `zero-spid` is default ON and `CRATONVM_JIT_ZERO_SPID=0` restores the
    // pre-fix behaviour, so `off_word` is exactly `"0"` -- the same shape as
    // `verify-ir` above. `zero_sp_id_slot_enabled` reads the key and treats
    // `0`/`false`/`FALSE` as off; only `"0"` is spellable as a group token, and
    // the other two spellings keep working through the key itself.
    // `ir-gc-point-maps` is default ON and `CRATONVM_JIT_IR_GC_POINT_MAPS=0`
    // restores the two IR GC-capable sites that recorded no safepoint (the
    // cooperative poll and `Op::New`), so `off_word` is exactly `"0"` -- the
    // same shape as `zero-spid` below.
    // gcd d9/d: default ON, `=0` restores the service copy. A baked
    // single-pass direct `invokestatic` into a body whose method has no
    // exception table keeps no copy of its arguments: they are the callee's,
    // so a dropped argument is not kept alive by the caller for the whole call
    // (`jit/src/x64/op_invoke.rs`, `sp_direct_call_args_owned_by_callee`;
    // `vm/src/jit/helpers.rs`, `drop_declined_callee_stash`). gcd d10/f: also
    // a statically bound instance call (invoke kind 1), and only where the
    // caller's bytecode before the call commits nothing, so the rare hand-off
    // (a re-run of the caller from entry) never commits a side effect twice.
    // `arm64` is opt-in: the aarch64 backend compiles nothing unless it is set,
    // because nothing in this repository has proven that backend on hardware.
    E { group: Group::JIT, token: "arm64", on_key: Some("CRATONVM_JIT_ARM64"), off_key: None, off_word: Some("0"), since: "2026-09-12" },
    E { group: Group::JIT, token: "arm64-safepoints", on_key: Some("CRATONVM_JIT_ARM64_SAFEPOINTS"), off_key: None, off_word: Some("0"), since: "2026-09-03" },
    // Profiler and debugger symbol sinks for JIT code, all opt-in: `perf-map`
    // writes perf-<pid>.map, `jitdump` writes jit-<pid>.dump (Linux), `gdb` feeds
    // the GDB JIT interface (Linux). See `jit/src/code_events.rs`.
    E { group: Group::JIT, token: "gdb", on_key: Some("CRATONVM_JIT_GDB"), off_key: None, off_word: Some("0"), since: "2026-09-12" },
    // The two JIT-runtime sensors, both off by default and both for a lane
    // rather than for production. `helper-panics-fatal` turns a CONTAINED
    // helper panic — a silent wrong answer: a dropped store, an `instanceof`
    // answering 0 — into an abort at the first one, with the panicking helper
    // still on the stack (`vm/src/jit/helper_guard.rs`).
    // `assert-code-free-audit` panics at VM shutdown if any published compiled
    // body was unmapped outside the retirement queue, which three documents
    // call a must-be-zero invariant and nothing whole-VM checked (`vm-cli`'s
    // `maybe_dump_shutdown_reports`).
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
    // --- composition residuals, 2026-09-02 ----------------------------------
    // Three default-off diagnostics and four A/B switches from
    // `completablefuture-composition-is-20x-and-5-percent-compiled-CLOSED-20260902.md`.
    // Every one of them exists so a claim on that page can be re-priced in ONE
    // binary; two of them were built specifically to refute a hypothesis, and
    // one of those did.
    // Stage 0 (`quiet_read_census` / `CRATONVM_DBG_QUIET_READS`) was retired after measuring
    // `docs/feature-designs/jit-ir-optimizations.md`.
    // SBR round 2, wave 8 (lane `tlocal`). DEFAULT ON, `0` opts out: the `ThreadLocal.get()` /
    // `InheritableThreadLocal.get()` / `set(Object)` thin direct binds at every compile door
    // (`--real-jdk` only; the helpers serve the found-key case from the real
    // `Thread.threadLocals` table and decline the rest to the generic dispatch).
    // SBR round 2, wave 8 (lane `tlocal`). DEFAULT ON, `0` opts out: the `Unsafe.getReference` /
    // `getReferenceVolatile` reference-ARRAY read thin direct helpers (what `ConcurrentHashMap.tabAt`
    // makes); also off when `CRATONVM_JIT_UNSAFE_ACCESSOR_DIRECT=0`.
    // zt t3/nd: the JIT class-identity dispatch memos are kept across a class definition that changes no existing name answer, edge, in-place origin or redefinition (only their cached refusals are dropped), instead of being emptied on every definition. Default ON since g1gen (2026-10-22: the G1 control and the jit pass list of the carried page's item 9; ZGC E/D CPU 0.96), "0" kills.
    // 2026-09-11. The fourth of four routes to a direct compiled entry, and
    // the only one still refusing on the callee's exception table alone. See
    // `env_cache::jit_virtual_promote_handler_callee`.
    // t1 wave 8 lane TIERMGR (page `t1-L71-loader-path-jdk-bodies-stay-interpreted-FIXED-20261028.md` item 3): the
    // general `VirtualBytecode` arm counts, nominates and enters compiled `synchronized` callees like any other
    // (`execute_jit_call_decoded` holds the monitor; the virtual door already did). Default ON; `=0` restores
    // the old refusal. See `env_cache::jit_virtual_tierup_synchronized`.
    // --- composition residuals, part two: 2026-09-11 -------------------------
    // `composition-native-callback-and-the-promotion-question-20260902.md`.
    // Item 1's mechanism and its engagement census; item 2's second census,
    // the one that names why a NOMINATED site still refuses to promote.
    // The stack-walker residual-costs page, item 1 (2026-10-27): the callback memo's NativeContext::invoke half
    // (VmConfig::native_invoke_memo). Perf: default ON, kill "0".
    // The stack-walker residual-costs page, item 2 (2026-10-27): NativeContext::set_fields_typed stores a batch
    // holding references through one compact plan while the collector is not marking
    // (VmConfig::native_batch_reference_stores).
    // Perf: default ON, kill "0".
    // The JIT per-call-site native cache used to refuse every
    // capability-classified triple outright, which since 836631dcc (leaf-only
    // -> every registered native) has meant refusing `jdk/internal/misc/Unsafe`
    // -- 99 428 of the 100 805 general-resolver calls on the composition probe.
    // The gate now runs on the dispatch side instead, where the funnel runs it;
    // `=0` restores the refusal.
    // `CRATONVM_JIT_IR_COLD_ARG_STAGE` was declared here too, as a courtesy,
    // and dev declared it concurrently -- both rows merged with no conflict,
    // which is the append-anywhere hazard. Dev's row is kept; this note is the
    // tombstone so the next session does not re-add a third.
    E { group: Group::GC, token: "g1-evac-retry", on_key: None, off_key: Some("CRATONVM_G1_NO_EVAC_RETRY"), off_word: None, since: "2026-07-03" },
    E { group: Group::GC, token: "g1-parallel-evac", on_key: Some("CRATONVM_G1_PARALLEL_EVAC"), off_key: None, off_word: Some("0"), since: "2026-06-21" },
    E { group: Group::GC, token: "g1-young-pause-target", on_key: Some("CRATONVM_G1_YOUNG_PAUSE_TARGET"), off_key: None, off_word: Some("0"), since: "2026-08-18" },
    // g1ep7 (2026-10-28): perf, default ON, kill "0". The final remark drains the gray set on the evacuation pool, one
    // marking deque per job, instead of on the pausing thread alone.
    // g1ep7 (2026-10-28): perf, default ON, kill "0". A parallel G1 pause resurrects its dead finalizable objects (Phase
    // 3.5) in a second closure on the evacuation pool instead of through the serial evacuator on the driver.
    // g1sz (2026-10-26): perf, OPT-IN "1" (2026-10-27 A/B: KeepRounds 2.66x HotSpot on vs 2.42x off). The parallel G1 closure
    // prefetches a holder's collection-set referents before it copies them, so a chain node's two header misses overlap.
    // JIT round 23 wave 8 (lane L9): perf, OPT-IN "1" (n23-w8b A/B: no measurable win on KeepLong / R23W3Keep). The parallel
    // G1 evacuator does not push a fresh primitive-array copy onto the gray stack (nothing to scan; HotSpot skips typeArrays).
    // JIT round 23 wave 8 (lane L9): perf, OPT-IN "1" (n23-w8b A/B: no measurable win). An idle parallel-evacuation
    // worker's repeat probes try_lock the shared gray queue instead of blocking on it.
    // JIT round 23 wave 8 (lane L9): perf, default ON, kill "0". G1's cleanup scrub walks memoise a compact object's size
    // per walk, keyed by (class_id, field word) and the layout generation (same answer as object_total_size).
    // JIT round 23 wave 8 (lane L9): perf, OPT-IN "1" (n23-w8g A/B neutral: KeepLong 30 19.2 M both arms). After 8 young
    // pauses in a row that leave under 3% of the regions Free while a G1 mark cycle is open, the mark driver finishes the cycle.
    // JIT round 23 wave 8 (lane L9): perf, OPT-IN "1" (n23-w8g A/B neutral). The G1 concurrent marker sizes marked objects
    // through a per-step memo (same answer as object_total_size; layout-generation invalidated).
    // JIT round 23 wave 8 (lane L9): perf, default ON, kill "0". The serial G1 evacuator sizes each copy through a per-pause
    // memo (same answer as object_total_size; layout-generation invalidated).
    // JIT round 23 wave 9 (lane L9): perf, opt-in "1" (neutral on the n23-w9a A/B). The serial G1 evacuator's supply screen skips its repeat
    // header classification for a slot candidate the scan just proved (the implausible-header half still runs).
    // JIT round 23 wave 8 (lane L9): perf (neutral or mixed on the n23-w8h KeepLong A/B, then won the triage A/B; see the next line). The serial G1 evacuator's copy-drift check keys a short
    // compact header by its class id instead of two compact_field_count probes per copy (diagnostic only).
    // R23-TRI-FLIP: default ON after the round-23 in-session triage A/B (n23-w13m), kill "0".
    // JIT round 23 wave 8 (lane L9): perf (neutral or mixed on the n23-w8h KeepLong A/B, then won the triage A/B; see the next line). A serial G1 young/mixed pause pre-sizes its forwarding
    // map from the last serial pause's forward count (capped at 1M entries; a capacity hint only).
    // R23-TRI-FLIP: default ON after the round-23 in-session triage A/B (n23-w13m), kill "0".
    // JIT round 23 wave 8 (lane L9): perf (neutral or mixed on the n23-w8h KeepLong A/B, then won the triage A/B; see the next line). The serial G1 Phase-3 Cheney loop, which runs before its
    // pause retires any forward, skips the re-evacuation guard's pointer-map probe (it cannot hit there; debug still asserts).
    // R23-TRI-FLIP: default ON after the round-23 in-session triage A/B (n23-w13m), kill "0".
    // JIT round 23 wave 8 (lane L9): correctness (R22FlipGcRows -Xmx64m JIT OOME), default ON, kill "0". A G1 full-compaction
    // pause whose young worst case leaves no to-space budget still takes the cheapest Old candidate that fits the Free pool.
    E { group: Group::GC, token: "g1-humongous-marks", on_key: Some("CRATONVM_G1_HUMONGOUS_MARKS"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    E { group: Group::GC, token: "g1-ihop-counts-regions", on_key: Some("CRATONVM_G1_IHOP_COUNTS_REGIONS"), off_key: None, off_word: None, since: "2026-09-05" },
    // Lane W2-C, 2026-09-20. All five OPT-IN (`=1`): each moves a marking-cycle
    // policy number or a per-object cost, and this round moves a default on a
    // measurement rather than on an argument. See the field docs in
    // `types/src/flags.rs` and the A/B recipes on their consumers in `gc/src/g1.rs`.
    E { group: Group::GC, token: "g1-ihop-gross-growth", on_key: Some("CRATONVM_G1_IHOP_GROSS_GROWTH"), off_key: None, off_word: None, since: "2026-09-20" },
    // JIT round 23 wave 11 (lane L9): perf, opt-in "1" (neutral on n23-w11a; G1 -Xmx64m gate 1/12 OOME with it on). A G1
    // pause's SATB drain drops entries at or above their region's TAMS (allocated after the snapshot; HotSpot's SATB
    // filter) instead of carrying them as keep-alive roots.
    // JIT round 23 wave 13 (lane L9): perf, OPT-IN "1" until an A/B. A serial G1 pause re-grays only its SATB-sourced
    // keep-alive roots; a gray-set root's entry is rewritten to the copy by the remap (it used to be pushed twice).
    // JIT round 23 wave 9 (lane L9): perf, OPT-IN "1" (n23-w9c: cleanup_us halves but KeepLong ops drop, 16.8-19.2 M vs
    // 21.6 M). G1 cleanup's dead-reference scrub runs over the evacuation pool, one region per claim.
    E { group: Group::GC, token: "g1-ihop-backoff", on_key: Some("CRATONVM_G1_IHOP_BACKOFF"), off_key: None, off_word: None, since: "2026-09-20" },
    E { group: Group::GC, token: "g1-mark-side-tables", on_key: Some("CRATONVM_G1_MARK_SIDE_TABLES"), off_key: None, off_word: Some("0"), since: "2026-09-20" },
    // gch h4/g1 (`gch-h3os-g1-remark-unloads-classes-before-it-resurrects-soft-retained-referents`):
    // G1's final remark runs the reference rows first, marks their keep closure, then judges class /
    // loader liveness and unloads on the final bitmap. Default ON; `=0` restores the old order.
    E { group: Group::GC, token: "g1-remark-unload-after-keep", on_key: Some("CRATONVM_G1_REMARK_UNLOAD_AFTER_KEEP"), off_key: None, off_word: Some("0"), since: "2026-10-09" },
    // gch h4/g1 (`gch-h3jx-g1-compat-unstarted-thread-inherited-values-never-released`): the G1 remark
    // sweeps the never-started threads' inherited-ThreadLocal buckets on the completed mark. Default ON.
    E { group: Group::GC, token: "g1-remark-itl-sweep", on_key: Some("CRATONVM_G1_REMARK_ITL_SWEEP"), off_key: None, off_word: Some("0"), since: "2026-10-09" },
    // gci i6/g1t, default ON since 2026-10-13 (`=0` kills it): G1 marking cycles pin queued inherited values to their child Thread (metadata_pin) instead of rooting them.
    // gc wave 9 lane F (`gch-h3jx-g1-compat-unstarted-thread-inherited-values-never-released`, the
    // residual): a G1 cycle's initial mark and remark pin the never-started threads' inherited values to
    // their child (ephemeron), closed mark-exact at the remark. Default ON; `=0` roots them as before.
    E { group: Group::GC, token: "g1-remark-itl-ephemeron", on_key: Some("CRATONVM_G1_REMARK_ITL_EPHEMERON"), off_key: None, off_word: Some("0"), since: "2026-10-12" },
    // gch h4/g1 (`gcd-d10o-native-funnel-oome-debt-is-generational-only`): the native funnel's post-OOME
    // debt is armed on G1 too, paid by a complete marking cycle. Opt-in until vh4-g1 block F decides.
    E { group: Group::GC, token: "g1-mark-convergence-epoch", on_key: Some("CRATONVM_G1_MARK_CONVERGENCE_EPOCH"), off_key: None, off_word: Some("0"), since: "2026-09-20" },
    E { group: Group::GC, token: "g1-verify-holders", on_key: Some("CRATONVM_G1_VERIFY_HOLDERS"), off_key: None, off_word: None, since: "2026-09-05" },
    E { group: Group::GC, token: "g1-scrub-free", on_key: Some("CRATONVM_G1_SCRUB_FREE"), off_key: None, off_word: None, since: "2026-08-18" },
    E { group: Group::GC, token: "g1-verify-forwards-retired", on_key: Some("CRATONVM_G1_VERIFY_FORWARDS_RETIRED"), off_key: None, off_word: None, since: "2026-09-07" },
    E { group: Group::GC, token: "g1-evac-copy-watch", on_key: Some("CRATONVM_G1_EVAC_COPY_WATCH"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::GC, token: "g1-ref-write-watch", on_key: Some("CRATONVM_G1_REF_WRITE_WATCH"), off_key: None, off_word: None, since: "2026-09-08" },
    E { group: Group::GC, token: "g1-evac-ref-implausible-refuse", on_key: Some("CRATONVM_G1_EVAC_REF_IMPLAUSIBLE_REFUSE"), off_key: None, off_word: None, since: "2026-09-06" },
    E { group: Group::GC, token: "g1-cleanup-walk", on_key: Some("CRATONVM_G1_CLEANUP_WALK"), off_key: None, off_word: None, since: "2026-09-02" },
    // Round 9 wave 5, default OFF: a thread sets start bits inside the ZGC VM
    // TLAB chunk it owns with a plain store (edge words keep the atomic).
    // Default ON since round 9 wave 5 (`gc/src/zgc/vm_tlab.rs`,
    // `ZGC_VM_TLAB_DEFAULT_ON`, measured); `0` is the kill switch.
    E { group: Group::GC, token: "g1-uncommit", on_key: Some("CRATONVM_G1_UNCOMMIT"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::GC, token: "g1-card-clean", on_key: Some("CRATONVM_G1_CARD_CLEAN"), off_key: None, off_word: None, since: "2026-09-02" },
    E { group: Group::GC, token: "g1-card-screen-jit-pinned", on_key: Some("CRATONVM_G1_CARD_SCREEN_JIT_PINNED"), off_key: None, off_word: Some("0"), since: "2026-09-02" },
    E { group: Group::GC, token: "g1-eden-stripes", on_key: Some("CRATONVM_G1_EDEN_STRIPES"), off_key: None, off_word: None, since: "2026-09-02" },
    // JIT round 21, lane vhcae (VHCAE21-1; page docs/feature-designs/jit-barrier-lowering.md): under G1 a compiled cross-region reference store (the single-pass F-08 arm, the optimizing tier's gated putfield and G1 aastore) skips its jit_g1_post_write_barrier call when the collector's per-VM edge filter shows the call would do nothing: both addresses in the regions, the holder's card already dirty with its block offset covered, and the (source, destination) region edge recorded in the current reclassification epoch. Default ON, kill "0".
    // JIT round 21, lane g1vh (CF21-1 of docs/feature-designs/jit-ir-optimizations.md; page docs/feature-designs/jit-barrier-lowering.md; re-landed 2026-10-26 after merge 624fae0b49 dropped it): under G1 the optimizing tier's constant-VarHandle prefix serves the reference write modes inline (set / setVolatile / compareAndSet / compareAndExchange; getAndSet only while no mark runs): the SATB pre-gate clear, or a null old value (a CAS: a null expected), and the post barrier ruled out (null value, same region) or paid by the lean jit_g1_post_write_barrier after the access (behind the g1-edge-filter). Default ON, kill "0".
    // JIT round 23 wave 3 (lane L4; r23-w3-g1-churn-keep-row): perf, default ON, kill "0". aastore_element_assignable
    // answers a plain value of the array's exact component (or a subclass, via the locked class_is_subtype, which
    // publishes the closure) without building descriptor Strings on every store.
    E { group: Group::GC, token: "g1-workers", on_key: Some("CRATONVM_G1_WORKERS"), off_key: None, off_word: None, since: "2026-06-22" },
    // Lane W4-B: the CSet verifier's target SWEEP PERIOD in pauses. `0` (the
    // default) keeps the flat object budget above, whose coverage falls
    // linearly with the live set — 0.31% of a 160 MiB heap per pause, 0.065%
    // of a 768 MiB one. See `verify_sweep_pauses` in `gc/src/g1.rs`.
    // Lane B wave 2, 2026-09-20. `free-census-on-reclaim` is default ON (`=0`
    // is the kill switch) because the behaviour it replaces is a cache that a
    // test proves wrong; `narrow-drain-fixup` is opt-IN (`=1`) because the
    // drain, unlike the four pause drivers, never scans its remembered-set
    // sources and so has not earned the narrowing. See `free_census_on_reclaim`
    // and `narrow_drain_fixup` in `gc/src/g1.rs`; the 109 ms the second lever
    // is aimed at is written on its gate.
    // Lane A, 2026-09-20. Both landed default-ON and were flipped to opt-in (`=1`)
    // the same day on a measurement: each costs 6-13% on two of three G1
    // workloads and wins ~4% on the third. The tables are on the gates. See
    // `lane_a_card_cursor` and `lane_a_rset_prune_on_scan` in `gc/src/g1.rs`.
    // Lane A wave 4, 2026-09-21. THE BLOCK-OFFSET TABLE: the remembered-set
    // source walk enters a dirty card at a recorded object start instead of
    // sizing every object from the region's base to reach it. Opt-IN (`=1`),
    // and this one is opt-in for a correctness reason rather than a
    // performance one: every other lever in this group fails by over-scanning,
    // and a wrong block offset makes the walk decode a header at an address
    // that does not hold one. The entry is screened before it is used and a
    // refusal is counted on the `[GC] g1 block-offsets:` census line. See
    // `lane_a_block_offsets` in `gc/src/g1.rs` and
    // `docs/feature-designs/concurrent-gc-maturation.md`.
    // Lane D, 2026-09-20. `evac-local-queue` is default ON (`=0` restores the
    // take-one/push-all ordering); the two parallel-phase levers are opt-IN
    // (`=1`), so production keeps today's behaviour byte for byte. See
    // `evac_local_queue_enabled`, `parallel_seed_enabled` and
    // `parallel_mixed_enabled` in `gc/src/g1.rs`.
    // Lane D wave 2, 2026-09-20. The parallel evacuator's PER-WORKER engagement
    // census (objects scanned, bytes copied, queue lifts, idle time), default ON
    // and disarmed with `=0`. Default-on on a measurement: the counters are u64
    // adds on worker-local state and the only clock read is one `Instant` pair
    // per idle EPISODE, and the A/B is inside its own spread. Before it existed
    // there was no way to tell a pause that used 23 workers from one that woke
    // 23 workers and did the work on the driver. See `evac_census_enabled`.
    // Lane W7-W, 2026-09-21. The two DISTRIBUTIONS behind the evacuator's
    // load-balancing valve: children pushed per `process_object`, and the local
    // stack depth those pushes reach. `EVAC_LOCAL_PUBLISH_HIGH` is compared
    // against the SECOND of those, and no instrument published either, so
    // "does a real Java graph ever reach 256" had no answer. Opt-in rather than
    // default-on: unlike `g1-evac-census` it has no A/B behind it, and this
    // round's rule is that a new lever defaults to today's behaviour until a
    // measurement says otherwise. See `evac_share_census_enabled`.
    E { group: Group::GC, token: "g1-evac-share-census", on_key: Some("CRATONVM_G1_EVAC_SHARE_CENSUS"), off_key: None, off_word: None, since: "2026-09-21" },
    // gci i22/sz: opt-in. On the HotSpot young model, Eden before a mixed pause leaves room for the mixed phase's minimum old CSet (`calculate_desired_eden_length_before_mixed`).
    // gci i23/tn: opt-in. The mixed gate's `tight` waiver is measured against twice the BASE free trigger, not the live (ratcheting) one.
    // gci i20/ys: the young survival ratio is sampled from young-only pauses only (opt-in); a humongous request crossing IHOP asks for a `G1 Humongous Allocation` concurrent start first (default ON since 2026-10-17, `=0` kills).
    // WARNING gci-user-flip-20261013: default ON since 2026-10-15 (vi15-rf U PASS; vi19-sw/fz cleared it of the NPE); `=0` kills.
    // gci i27/gr (jr20), default ON (`=0` kills): after a G1 pause that kept a young region in place, an unheld weak / phantom referent or `Reference` there is cleared / pruned as HotSpot does.
    // gci i30/rp: default ON, =0 kills.
    // gci i31/gq: default ON, =0 kills.
    // gci i32/ec: default ON, =0 kills.
    // gci i31/gq: opt-in (G-2; flip rule vi31-gq block G).
    // gci i27/gr (i15rf residual): opt-in, needs `stw-undiscovered-refs-strong`. A G1 full collection's remark leaves alone the `Reference`s its STW pause found undiscovered.
    // A G1 full collection's young and compaction pauses preserve finalizer-only
    // References through this door, then discover them at the next collection.
    // Default ON; =0 restores premature referent clearing.
    // gci i27/gs (i14mp item 8): default ON since 2026-10-17 (`=0` kills), Generational. `Survivor Space` reports HotSpot Serial's survivor reserve (`semi / 9`, 64 KiB aligned), carved out of `Eden Space`.
    // gci i27/gs (i14mp item 8): default ON since 2026-10-17 (`=0` kills), Generational. The adaptive tenuring target's survivor capacity is aligned down to 64 KiB as HotSpot's to-space is.
    // WARNING gci-user-flip-20261013: default ON since 2026-10-15 (vi19-ys M5 + S PASS); `=0` kills.
    // JIT round 23 wave 10 (lane L9): perf, OPT-IN "1" (n23-w10a: KeepLong 30 12.0-14.4 M vs 16.8-19.2 M at =0). Parallel G1
    // promotion copies go into PLAB chunks of a shared Old region, and the pool cap becomes pool/2 workers, not pool/8.
    // gci i25/pl, default ON (`=0` kills): a parallel copy into a shared Survivor PLAB chunk skips the per-copy
    // `parallel_copy_end` publish (the region cursor its chunk claim advanced already covers it).
    // JIT round 23 G1 evacuation page: a G1 evacuation copy's destination mark word is ONE plain store of the snapshot with the old-generation stamp or the incremented age folded in (gen r5w6/pin10's helpers), not a store plus a locked fetch_or / compare-exchange loop on the line the copy just wrote (29% of SharedEvac::evacuate's self time on R23W6KeepLong -Xmx1g). Default ON, kill "0".
    // G1 near-full proxy-define page: an armed G1 full compaction whose copying pass left the Free pool under an eighth of the heap's regions slides the movable Old regions in place (HotSpot's G1FullCollector shape: mark from the roots and every non-compacted object, plan, adjust, memmove), so a near-full heap with garbage beside live data is compacted with no Free to-space. Default ON, kill "0".
    // G1 near-full proxy-define page: a fallible small allocation at the G1 TLAB reserve may claim a Free region once collections gave back a sixteenth of a region per claim since the reserve last refused (it was half a region), never the last Free region; a live-filling heap still earns nothing. Default ON, kill "0".
    // G1 near-full proxy-define page: the G1 TLAB / fallible-allocation reserve's floor is one region instead of two (heaps under 128 regions; at -Xmx64m two regions were 3% of the heap, the margin a near-full live set leaves, and HotSpot keeps no such reserve). Default ON since the near-full proxy row A/B, kill "0".
    // G1 evacuation page: an evacuation pause of an open G1 mark cycle re-grays its keep-alive copies under ONE seed-queue lock and ONE marker wake (it took the lock, two waker mutexes and a notify_all per copy: 2.8% of all samples on R23W6KeepLong with every young pause parallel); each address keeps the one-address verdict. Default ON, kill "0".
    // G1 evacuation page: G1's low-Free trigger floor (a young generation of a quarter of the Free pool before a Free-fraction pause) is measured in Eden regions, not young regions; survivors met it alone, so a heap whose young data all survives paused after every Eden region and re-copied its survivors each time. Default ON, kill "0".
    // r23 w6 G1 evacuation copy throughput (KeepLong): a futile-young streak under an open cycle aborts the cycle and runs one in-place full compaction (HotSpot's evacuation failure then Pause Full). Default ON, kill "0".
    // G1 near-full proxy-define page: the in-place full compaction takes Survivor regions into its compaction set and leaves them Old, as HotSpot's full collection compacts young regions. Default ON, kill "0".
    // G1 near-full proxy-define page: the in-place full compaction holds JIT-pinned regions (marked, not moved, dead objects filled) instead of leaving them out with every object a holder. Default ON, kill "0".
    // G1 near-full proxy-define page: the in-place full compaction slides objects into a held (JIT-pinned) region's dead runs and tail; only the held region's named objects stay put under g1-in-place-held-objects-move (none moves with that off). Default ON, kill "0".
    // G1 near-full proxy row (refused allocation): in a held region only the objects a root or a conservative JIT word names stay put; its other live objects pack down inside the region, so the lift can fill the run that opens. Default ON, kill "0".
    // G1 near-full proxy-define page: the in-place full compaction runs on the evacuation pool (parallel parse, mark, per-chunk moves and adjust walks); "0" runs it on the driver alone. Default ON, kill "0".
    // JIT round 23 G1 evacuation page: a parallel G1 copy inside the worker's own whole-region TLAB publishes the region's copy end with a Release store (the region has one writer), not fetch_max's locked compare-exchange loop (29% of SharedEvac::evacuate's self time). Default ON, kill "0".
    // gci i23/tn, default ON (`=0` kills): HotSpot's `_max_survivor_regions` -- at most ceil(young target / 8) fresh Survivor regions per evacuation pause; the rest is copied into Old.
    // WARNING gci-user-flip-20261013: default ON since 2026-10-15 (vi19-ys M5 + S PASS); `=0` kills.
    // gci i43/gy: default ON (G1 honours -Xmn / NewSize / MaxNewSize / NewRatio and SurvivorRatio like G1YoungGenSizer), =0 kills.
    // gci i45/gy: default ON (`=0` kills): G1 young bounds follow the committed region count, as HotSpot's G1YoungGenSizer::heap_size_changed (moves only under CRATONVM_G1_HEAP_RESIZE).
    // gci i45/sc: default ON (a typed -XX:SurvivorRatio sizes the Generational Serial eden, survivor reserve and tenuring target), =0 kills.
    // gci i45/sc: default ON (the TLAB sizer reads -XX:TLABSize / MinTLABSize / ±UseTLAB / ±ResizeTLAB / TLABWasteTargetPercent / TLABRefillWasteFraction / TLABAllocationWeight per VM; Generational and G1, ZGC since gci i48/zl3), =0 kills.
    // gci i30/ob: default ON (WARNING gci-user-flip-20261015, h4g1 item 6), =0 kills.
    // gci i30/ob: default ON, =0 kills.
    // gci i41/rs: default ON (the G1 full door's Survivor stamp inside its own compaction pause), =0 kills.
    // Lane E, wave 2 of 2026-09-20. All three shipped opt-IN (`=1`): wave 1's lesson was that two of three levers
    // landed default-ON on an argument rather than a number were wrong. `g1-heap-resize` has since been flipped on
    // its soak (below); the other two stay opt-in. See
    // `g1_heap_resize_enabled`, `g1_pause_cost_model` and
    // `g1_humongous_run_guard` in `gc/src/g1.rs`.
    // g1-heap-resize: default ON since 2026-10-26 (g1sz), kill "0" -- the soak passed after the young-pause work
    // (`g1_heap_resize_enabled`).
    // Lane W4-B: does a YOUNG pause refresh `evac_ns_per_byte`? Opt-in. Today
    // only the two mixed drivers do, so on a workload with no mixed collection
    // the estimate holds its constructor value 4 for the whole run — which is
    // the input `lane-e-proposals.md` P1's predictor is built on. See
    // `g1_evac_cost_from_young` in `gc/src/g1.rs`.
    // Lane W5-C, wave 5 of 2026-09-20. A counted fail-safe on the mark-cycle
    // back-off (`CRATONVM_G1_IHOP_BACKOFF`), whose only re-arm today is
    // old-generation GROWTH. Growth cannot see old objects DYING, so a program
    // that drops a large old data set and then allocates only short-lived
    // objects holds the gate shut over exactly the cycle that would have paid
    // for itself. Opt-in, and inert unless the back-off is also on. See
    // `check_ihop` in `gc/src/g1.rs` and `G1OldBurstProbe.java`.
    E { group: Group::GC, token: "g1-mark-backoff-deadline", on_key: Some("CRATONVM_G1_MARK_BACKOFF_DEADLINE"), off_key: None, off_word: None, since: "2026-09-21" },
    // Lane W6-B, wave 6 of 2026-09-20. The mark-cycle back-off's DECLINED path
    // costs two read-modify-writes on two process-global cache lines, and it
    // is taken at the ALLOCATION rate — a suppressed cycle never sets
    // `g1_is_marking_active()`, so nothing short-circuits the JIT mark driver
    // that every compiled `new` calls. Wave 3 measured 1 616 077 polls, wave 5
    // measured 41 641 213 against 130 mark cycles, both single-threaded. This
    // makes a declined poll two relaxed loads and a thread-local increment.
    // Opt-in, and inert unless the back-off is also on. See `check_ihop` in
    // `gc/src/g1.rs` and `G1PollStormProbe.java`.
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
    // epilogue. See `docs/feature-designs/concurrent-gc-maturation.md`.
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
    // `docs/feature-designs/concurrent-gc-maturation.md`.
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
    // `docs/feature-designs/concurrent-gc-maturation.md`.
    // `cuda_bridge::critical` — millisecond budgets, trimmed `u64`, latched in
    // an `AtomicU64` on first read. Value tokens:
    // `CRATONVM_GC=gpu-critical-wait-ms=250`.
    // AUDIT 2026-09-02. `gpu-dispatch-streams` sizes the round-robin pool the
    // handle-less dispatch path takes a stream from instead of creating and
    // destroying one per submission; set it to 1 for the strictest ordering.
    // `gpu-jit-array-writers=allow` picks the other side of the
    // JIT-versus-residency-cache trade for a method that stores into a
    // primitive array — see `vm::runtime::offload_jit_gate::ArrayWriterPolicy`
    // for the measurement that made blocking the JIT the default.
    // The fitted admission cost model of
    // docs/feature-designs/gpu-offload.md, opt-in. `=1`
    // replaces nothing -- it runs BESIDE `--gpu-min-work`, refusing work
    // the scalar threshold admits at a loss. Off by default because the
    // four fitted constants are one device's.
    // The compiled-tier GPU input-residency barrier (2026-09-04). ON by
    // default on x86_64 under `--gpu`; `=0` refuses to arm it, which
    // puts `offload_jit_gate` back to refusing JIT admission to every
    // method that writes a primitive array. See `jit::gpu_barrier`.
    // `=0` puts `offload_jit_gate` back to blocking the caller of ANY
    // analyzer-Eligible `invokestatic`, instead of only one the
    // DISPATCHER could actually launch. The control arm for that
    // narrowing; see `offload_jit_gate::target_can_ever_dispatch`.
    // `=0` restores the pre-2026-09-06 compiled site memo: a site whose
    // target is not in the offload registry the first time it executes is
    // written off as NotKernel forever, instead of asking the gate once the
    // class exists. The CONTROL ARM for the forward-reference fix -- with it
    // off, `GpuForwardRef forward` goes dark and `GpuForwardRef preload` does
    // not, on one binary. See `offload_jit_gate`'s module docs.
    // `=0` drops the caller-blocking half of `offload_jit_gate`: a caller
    // of an eligible kernel compiles, and offload silently ends there.
    // A CONTROL ARM, not a production setting -- it isolates the cost of
    // the refusal from everything else `--gpu` changes.
    E { group: Group::GC, token: "gpu-critical-lease-ms", on_key: Some("CRATONVM_GPU_CRITICAL_LEASE_MS"), off_key: None, off_word: None, since: "2026-07-31" },
    E { group: Group::GC, token: "gpu-critical-wait-ms", on_key: Some("CRATONVM_GPU_CRITICAL_WAIT_MS"), off_key: None, off_word: None, since: "2026-07-31" },
    // A/B levers declared 2026-09-02 with the four GPU-subsystem fixes.
    // `gpu-host-callback` restores the per-launch `cuLaunchHostFunc` the
    // completion reaper no longer registers (a host function blocks the
    // launches queued behind it on its stream). `gpu-device-pool` is
    // DEFAULT-ON with a "0" off-word: the bridge's device-allocation pool has
    // no observable semantics, so the only honest way to price it is one
    // binary both ways.
    // How many carriers the FFM element fast path remembers per thread. `1`
    // is the single slot it replaced, which is the control arm: kfusion's
    // integration alternates the TSDF volume with the images it reads, and
    // one slot published 38.3M native verdicts for 129M consults.
    E { group: Group::JIT, token: "arith-licm-trim-entered", on_key: Some("CRATONVM_JIT_ARITH_LICM_TRIM_ENTERED"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "vec-ewise-forms", on_key: Some("CRATONVM_JIT_VEC_EWISE_FORMS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "field-array-hoist", on_key: Some("CRATONVM_JIT_FIELD_ARRAY_HOIST"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "ref-hoist-oop-maps", on_key: Some("CRATONVM_JIT_REF_HOIST_OOP_MAPS"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::JIT, token: "aaload-licm-typecheck-fence", on_key: Some("CRATONVM_JIT_AALOAD_LICM_TYPECHECK_FENCE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    // JIT round 18 wave 11, lane mega5 (THROWS18-5/-6/-7 of `jit-r18-throws-proposals.md`): all three
    // default OFF (performance, unmeasured). The stub's whole probe in RAX/R10/R11 behind the gate, its
    // hit on the site's own pre-marshalled call tail (`runtime_lowering::mega_stage_once_enabled`); the
    // hashed stub's hit republishing the caller's identity and joining `.hit_join`
    // (`mega_lean_stub_hit_enabled`); the gate tested ahead of the MIC (`mega_gate_first_enabled`).
    // JIT round 19, lane mega19: all default OFF (performance, unmeasured). MEGA5-18-1: the single-pass
    // twins of the three rows above, each its own switch (`runtime_lowering::sp_mega_lean_stub_hit_enabled`,
    // `sp_mega_gate_first_enabled`, `sp_mega_stage_once_enabled`); MEGA5-18-3: a register-probe miss goes to
    // the resolving helper past the stub (`mega_probe_miss_to_helper_enabled`, either tier's stage-once);
    // MEGA5-18-4: a gate-first site's megamorphic slot stops taking MIC / inline-way publications
    // (`mega_gate_first_no_inline_publish_enabled`, `JitPICSlot::skips_inline_publication`).
    // JIT round 19 wave 2, lane mega2: both default OFF (performance, unmeasured). MEGA19-2: a single-pass
    // MIC-only (over-wide) site tests the megamorphic gate before its MIC compare and sends its screened
    // MIC misses to the hashed stub's gate entry (`runtime_lowering::sp_mega_mic_gate_enabled`); MEGA19-4: a
    // site whose slot the receiver profile seeded megamorphic emits no MIC and no inline ways
    // (`runtime_lowering::mega_seeded_skips_inline_ways`, both tiers).
    // JIT round 16 wave 17, lane mega5: both default OFF (performance, unmeasured). The blind virtual
    // arm's late probe (vm `helpers::blind_dispatch_late_probe_rewind`, shape 6) and the bind-time
    // policy question for the `ScopedMemoryAccess` thin helpers (`cratonvm_jit::scoped_memory_bind_admitted`, NIO2-1).
    // Optimizing tier (round 16 lane sync, SY16-2): relaxation 1 (a forward
    // branch) also after a committed STORE in a spliced body, builder and
    // planner. Default ON since round 16 (measured: SyncM p 14->8 ns;
    // `runtime_flag_default_on`), so a KILL SWITCH, `"0"`.
    // t1 wave 7, lane NBD (DISPATCH21-4): an IR inline-cache site (interface sites included) whose receiver a dominating checkcast to a non-array type proved a plain object skips the receiver kind screen (jit `ir_lower::ic_receiver_cast_excludes_arrays`). Perf, opt-in "1" until measured.
    // zt t12/df4: the `upcallStub` intrinsic's native-access check names `upcallStub`.
    // zt t12/df4: `--illegal-native-access=deny` refuses with HotSpot's text.
    // zt t14/x11: in the initPhase2 slot the JDK's ModuleBootstrap takes (and removes) jdk.module.illegal.native.access, as HotSpot's initPhase2 does, so the program sees no such property; before, it stayed visible until the first restricted call. Only with a deny / allow / warn value.
    // JIT round 16 wave 13 (lane ffmseg; FFD16-1): opt-in, a bound plain
    // downcall skips the native funnel (`helpers::jit_ffm_downcall_lean_enabled`).
    // JIT round 17 wave 2 (lane ffm3; FFMSEG17-3): opt-in, the segment rows
    // acquire a confined session owned by this thread without the access
    // window (`panama::ffm_confined_acquire_no_window`).
    // JIT round 17 wave 2 (lane ffm2): default on (`=0` off), a capture-state
    // segment smaller than the capture layout is HotSpot's
    // `IndexOutOfBoundsException`, not a write past its end
    // (`panama::ffm_capture_segment_size_check`).
    // JIT round 16 wave 11 (lane jnieintr): a JNI native in native (the
    // in-native package) is not signalled either; on with the package, `=0`
    // kills it (`native::jni::in_native_no_signal_from`).
    E { group: Group::JIT, token: "jni-in-native-no-signal", on_key: Some("CRATONVM_JNI_IN_NATIVE_NO_SIGNAL"), off_key: None, off_word: Some("0"), since: "2026-10-02" },
    // JIT round 16 wave 12 (lane inpkg): an FFM up-call and the embedding
    // flat API leave a JNI native's in-native state before they run Java or
    // touch the heap; a JNI native inside an FFM downcall stays counted. On
    // with the package, `=0` kills it (`native::jni::in_native_doors_from`).
    E { group: Group::JIT, token: "jni-in-native-doors", on_key: Some("CRATONVM_JNI_IN_NATIVE_DOORS"), off_key: None, off_word: Some("0"), since: "2026-10-02" },
    // JIT round 21 wave 19 (lane jnilazy, FF3-3;
    // `jr21-jnilazy-jni-in-native-lazy-20261014`): on with the package
    // (`=0` off), a JNI native call whose C code can hold no local reference
    // (a static native with primitive arguments, no local frame in use) goes
    // into C through the lazy in-native state of the FFM downcall -- one
    // compare-exchange in, one out, no root deposit -- and a pause claims and
    // scans the thread instead (`native::jni::in_native_lazy_from`,
    // `gc_and_alloc::claim_lazy_native_peers`).
    E { group: Group::JIT, token: "jni-in-native-lazy", on_key: Some("CRATONVM_JNI_IN_NATIVE_LAZY"), off_key: None, off_word: Some("0"), since: "2026-10-14" },
    // JIT round 16 wave 11 (lane jnieintr, FFM16-3): opt-in, the in-native
    // repeat for the quiet GC-safe FFM deposit without the JNI package
    // (`native::jni::repeat_armed_from`).
    E { group: Group::JIT, token: "ffm-in-native-repeat", on_key: Some("CRATONVM_FFM_IN_NATIVE_REPEAT"), off_key: None, off_word: None, since: "2026-10-02" },
    // JIT round 16 wave 14 (lane ffm3): a non-critical FFM downcall goes into
    // C through HotSpot's `_thread_in_native` transition (one store in, one
    // compare-exchange out, no deposit) and a collection claims and scans the
    // thread instead of waiting for it
    // (`panama::ffm_downcall_in_native_lazy`, `gc_and_alloc::claim_lazy_native_peers`).
    // JIT round 17 wave 5 (lane ffm4): default on (`=0` off), a liveness fix:
    // the counted state deadlocks a pause against a thread blocked in C.
    E { group: Group::JIT, token: "ffm-downcall-in-native-lazy", on_key: Some("CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY"), off_key: None, off_word: Some("0"), since: "2026-10-02" },

    // JIT round 21 lane native (FFL16-1 / FUNNEL2-19-4; `jit-r21-native-proposals.md`): the MIC helper asks the
    // FFM direct door only for `invokeExact` sites. Performance, DEFAULT-ON, `=0` kill.
    // JIT round 17 wave 2 (lane ffm3; EINTR17-1): opt-in, a counted FFM
    // downcall blocks the take-over signal for its C call (an up-call
    // unblocks it), and the Linux take-over does not signal a peer that
    // blocks it (`panama_libffi::TakeoverSignalMask`,
    // `xt_root_scan::takeover_mask_door_enabled`).
    // Same lane (FF3-2): opt-in, a heap-dump or inline-cache-grace pause (no
    // roots) also claims, freeze-only, the threads in C in a lazy in-native
    // downcall instead of waiting for them (`gc_and_alloc::no_roots_lazy_claim_enabled`).
    // Same lane (EINTR2-1): opt-in perf, the VM's blocking waits publish their
    // stack band from a callee-saved-register trampoline and the Linux
    // helper-window pass reads it without a signal
    // (`jvm_thread::helper_unparked_read_enabled`).
    // JIT round 19 wave 1, lane eintr19 (FFMEINTR18-1): a frame-trace pause
    // (cross-thread `Thread.getStackTrace`, a thread dump) claims, freeze-only,
    // the threads in C in a lazy in-native downcall with no compiled frame and
    // publishes their stack traces for them, instead of waiting for the C code
    // (`gc_and_alloc::frame_trace_lazy_claim_enabled`). A liveness fix, default
    // ON, KILL SWITCH "0".
    // Same lane (FFMEINTR18-4): the take-over excuses a frozen or claimed peer
    // from the barrier only when it is counted and not in a blocked region
    // (`gc_and_alloc::excuse_unblocked_only_enabled`). Defensive correctness,
    // default ON, KILL SWITCH "0".
    // Same lane (FFMEINTR18-5): a last-resort stderr report of a stop-the-world
    // wait, a parked barrier waiter, a lazy in-native leave or a closed signal
    // gate that is 30 s stuck (`thread_registry::stw_stall_report_after`).
    // Default ON (nothing prints on a healthy run), KILL SWITCH "0".
    // JIT round 20 wave 11, lane pAtom (W9L2-4, docs/feature-designs/jit-ir-optimizations.md):
    // the twelve Math/StrictMath `*Exact` rows register as leaf natives (no funnel); `=0` keeps the funnel.
    // JIT round 16 wave 11, lane bignum2: native-builtins math_bignum.rs. `bignum-borrow-mag` default OFF
    // (perf, `BignumView` operands for both `compareTo`s); `bigdecimal-init-double-keeps-intval` default ON
    // (JDK 25 identity: `new BigDecimal(double)` keeps `rb` for a compact value; "0" restores `intVal = null`).
    // JIT round 16 wave 14, lane bigdec: native-builtins math_bignum.rs. Default OFF (perf, proposal BN16-1):
    // registers `BigDecimal.round(MathContext)` / `plus(MathContext)` as stated `Intrinsic`s (`native_bd_round_mc`).
    // JIT round 17 wave 1, lane bignum: native-builtins math_bignum.rs. `bignum-pair-alloc` default OFF (perf,
    // BN2-2 minus the new door: `BigInteger` + `mag` from the two no-GC doors, no pin); `bignum-canonical-signum` default ON (`bi_alloc` stores signum 0
    // for a zero magnitude; "0" restores the text rule).
    // JIT round 17 wave 6 (lane eintr2): on the lean road, the BigDecimal receiver is not pinned across
    // the allocation of its `intVal` (an allocation is no GC point: `NativeHeapAccess`'s one rule).
    // Default OFF (perf).
    // JIT round 16, lane codegen: CG16-1 (the entry fast return through a
    // join phi) is default ON with `0` as its kill switch; CG16-2 (the
    // division fixups after the body) is default ON since round 16 (measured:
    // R16CodegenLoops nocall 2.51->1.51, intLoop 1.06->0.73, bigA 64->55;
    // `runtime_flag_default_on`, `0` its kill switch); CG16-3 (pressure-aware
    // list scheduling) is a default OFF performance lever; CG16-4 (an outlined
    // poll's map keyed by its own node pc) is a correctness fix, default ON.
    // JIT round 16 wave 3, lane codegen: CG16-6 (a loop phi's back-edge source
    // in the phi's register) and CG16-7 (a constant phi source as an
    // immediate), both default OFF performance levers.
    E { group: Group::JIT, token: "ir-phi-coalesce-back-edge", on_key: Some("CRATONVM_JIT_IR_PHI_COALESCE_BACK_EDGE"), off_key: None, off_word: None, since: "2026-10-01" },
    // JIT round 16 wave 4, lane codegen: CG16-8, the prologue's zero runs
    // through RAX (an encoding change), default OFF.
    // JIT round 17 (r16 codegen loop page): CG16P-1 (the back-edge poll's
    // slow path after the body), CG16P-2 (an invariant array's length from a
    // pre-header that deopts to the loop entry) and CG16P-4 (no home word for
    // an int/long constant every reader folds).
    // JIT round 20 wave 12, lane qLoop (W5L3-3, book `jit-r20-nio-proposals-w5-L3.md`): perf, default
    // ON (kill "0"). The main/post partial unroll takes a `long` induction variable (MemorySegment /
    // Buffer fill and copy loops), its `lcmp` test read through and the licence `n - d < n` and the
    // main test built at `long` width (jit `ir_optimize::main_post_long_iv_enabled`).
    // Experimental compare of resident long operands; opt-in.
    // Experimental direct resident-register int-to-long conversion; opt-in.
    // JIT rx27 wave 4 (lane regalloc): an If on (x & mask) == 0 / != 0 with a single-use, unnamed And branches on TEST x, mask; the And emits nothing and writes no home (the regex predicates' class-bit tests). Default ON since the 2026-10-29 regalloc flip (=0 kills).
    // JIT rx27 wave 6 (lane regalloc): a Guard on (x & mask) == 0 / != 0 deopts on TEST x, mask; when only the guard's own frame names the And (a pruned ifeq's operand), its deopt pad rebuilds the And's home from x before the stub reads the frame. Default ON since the 2026-10-29 regalloc flip (=0 kills).
    // JIT rx27 wave 8 (lane regalloc): a compare (fused or not) and ADD/SUB/AND/OR/XOR read their second operand in place (immediate, register or home word) instead of loading RCX first; only reads change, every home is written as before. Default ON since the 2026-10-29 regalloc flip (=0 kills).
    // JIT rx27 wave 9 (lane regalloc): a read into RAX right behind its own register publish emits nothing (verified after the body; a branch onto it retries without that value). Default ON since the 2026-10-29 regalloc flip (=0 kills).
    // JIT round 16 waves 2-4, lane fieldguard: the deopting layout guard is a
    // default OFF performance lever; the per-compile layout epoch snapshot
    // (FG-b3) is a correctness fix, default ON. IR field-store forwarding and
    // store null-check CSE are default ON since round 16 (measured as a bundle
    // with `ir-this-nonnull`: SyncM p 14->12 ns, alloc kernel -13 %), and
    // inline primitive putstatic is default ON since round 16 (measured: static
    // stores 5-13x, static-calls 16.1->1.7 ns, SyncM sp 29->7 ns); all three
    // `runtime_flag_default_on`, so KILL SWITCHES, `"0"`.
    // JIT round 17 wave 3, lane irlower: the deopting layout guard's lone
    // sites counted per (pc, own frame), so an unrolled or coarsened copy's
    // field site may take it. Opt-in; inert without `ir-layout-guard-deopt`.
    // JIT round 25 (item 4 of the r24 regex-rows redesign): a Ref-typed
    // receiver's compactness proven once by a deopting guard; the field reads
    // and primitive writes it dominates are one MOV (no compact test, legacy
    // arm, null test or epoch compare). Default ON since R25-FLIP (`=0` kills);
    // brings `ir-layout-guard-deopt`'s
    // deopting epoch guard with it.
    // JIT round 25 item 4 wave 2, both default ON since R25-FLIP (`=0` kills) and inert without
    // `ir-proven-receiver-fields`: proofs put down inside spliced bodies (a
    // chain resumes the field's own bci), and proofs read across blocks by
    // dominance in bodies with loops and exception entries.
    // JIT round 25 wave 6 (lane W6-E), opt-in: an establishing getfield on
    // another value also proves `this` compact. (W6-E's `-osr-recheck` was
    // retired in favour of `ir-proven-receiver-fields-preheader` below.)
    // JIT round 25 item 4 residual 2, default ON (`=0` kills), inert without
    // `ir-proven-receiver-fields-dominance`: a proof put down before a loop
    // covers its body, each OSR header entry that jumps past the guard
    // re-checking it in its stub (or refused).
    // JIT round 25 item 4 residual 4, default ON (`=0` kills), inert without
    // `ir-proven-receiver-fields-dominance`: a body with compiled handler arms
    // reads field proofs across blocks too (an arm is an ordinary CFG edge
    // from its call's block, entered only from the call's exceptional exit).
    // JIT round 16 wave 7, lane callbind: default OFF (the spliced unbox
    // intrinsic; CB16-3, the `Long` cached-range read).
    // Interpreter round i6 wave 132, lane L6 (page `i40-L2-proposal-guard-a-
    // pinned-private-interface-call-instead-of-refusing-the-method`, stage 1):
    // the method-entry and OSR doors pin an `invokeinterface` of the caller's
    // own private interface method when its receiver is the unstored `this`,
    // instead of refusing the method (performance only; default OFF;
    // `jit_bridge::private_iface_this_pin_proven`).
    // JIT round 16 wave 7, lane arraylist (AL16-1): default OFF performance lever.
    // al16 (2026-10-05): cold throw arms entered by an earlier conditional branch (`if (a || b) throw`).
    // For-each residual round (2026-10-06): plan the factory class at a site that has a receiver profile too.
    // For-each residual round (2026-10-06): an optimizing OSR body's in-place guard exit re-allocates scalar-replaced objects.
    // OSR round (2026-10-06): an optimizing OSR body's reason-9 pad may name a scalar-replaced local; the OSR exception transfer re-allocates it.
    // OSR round (2026-10-06): read a loop-invariant object's unaliased fields once in the pre-header and forward them through the loop.
    // al16 (2026-10-05): prove an exact receiver from the last store into a reused local slot.
    // al16 (2026-10-05): an OSR body spares a fresh allocation the opaque arm of an unrequested header would merge.
    // JIT round 23 wave 6, lane L3 (pages r23-w4-compiled-recursion-shallower-than-interpreter, r23-w2-lambda-recursion-overflows-at-380-levels): correctness (SOE depth within HotSpot's order), default ON, kill "0". The compiled self-recursion budget is 16 x the thread's Java stack size (-Xss / stackSize, HotSpot's 1 MiB default: 16 MiB) instead of a flat 4 MiB (vm `jit::helpers::SelfCallBudget`); the thread's guard floor still bounds it.
    // JIT round 23 wave 2, lane L3 (page r23-w2-xss-ignored-for-main-thread-compiled-recursion-FIXED-20261021.md): correctness, default ON, kill "0". The main thread's compiled self-recursion budget grows for a -Xss default as every platform thread started without a stackSize does (vm `vm_exec::main_thread_self_call_extra_budget`).
    // Interpreter round i4 wave 88, lane L7: the class-define listener's
    // supertype withdrawal spares bodies that inlined only non-overridable
    // members (constructors, static / private / final methods). Bug fix,
    // default ON; `=0` restores the name-only match.
    // JIT round 16 wave 3 lane monitor: the inline thin-lock enter / exit
    // without its never-taken tests (opt-in until measured).
    // JIT round 16 wave 13, lane mega: guarded + polymorphic default ON since wave 14 (poly 1221 -> 533 ms; `=0` off), package-private OFF. The
    // single-pass OSR door's guarded splices (`cratonvm_jit::plan_osr_door_guarded_splices`), their
    // three-or-four-class arm, and package-private overrides as guarded splice targets (MG16-3,
    // vm `receiver_resolution_is_dispatch_faithful`).
    // JIT round 18 wave 9, lane osrpoly: the optimizing OSR door declines its IR
    // attempt where the single-pass OSR body splices a polymorphic in-loop site the
    // optimizing body would keep as a dispatch (`R16Bench poly` 2x regression from
    // the always-opaque OSR door), and an OSR-door acceptance refusal built with
    // door-only opaque arms stays out of the method-entry refusal memo. Both
    // regression fixes, default ON (`=0` restores the previous rule).
    // JIT round 21, lane accept (jr21-osr-route-refusal-poisons-the-method-entry-memo-FIXED-20261015.md):
    // default ON (`=0` kills). Every optimizing OSR-route acceptance refusal stays out of the
    // method-entry refusal memo, not only one built with door-only opaque arms (jit
    // `osr_route_own_refusal_memo_enabled`).
    // t1 wave 7 lane NBA (ACCEPT21-3, `docs/feature-designs/wire-tiered-manager.md`):
    // the optimizing OSR route reads and writes the IR refusal memo under its own key, so a
    // method-entry acceptance refusal no longer skips its IR attempts (a sticky site-trap ban on
    // the shared key still holds). Perf, opt-in (default OFF) pending a host A/B.
    // r23 wave 2c, lane L8: perf, default ON, kill "0". The lean SegmentVarHandle get/set road also serves a real heap
    // segment (MemorySegment.ofArray) through its backing array once the JDK's checks pass (types
    // `NativeFlags::ffm_segment_vh_lean_heap`, native-builtins `lang_invoke::segment_vh_lean_heap`).
    // r23 wave 2g, lane L8: correctness, default ON, kill "0". A refused SegmentVarHandle get/set is re-run through the
    // JDK's VarHandleSegmentAs<X> static body so the exception carries HotSpot's frames (types
    // `NativeFlags::ffm_segment_vh_jdk_raise`, native-builtins `lang_invoke::segment_vh_raise_through_jdk`).
    // r23 wave 6, lane L8: correctness, default ON, kill "0". The JDK re-raise of a refused SegmentVarHandle access
    // covers the ordered and read-modify-write modes too, through VarHandleSegmentAs<X>Aligned.<mode> (types
    // `NativeFlags::ffm_segment_vh_jdk_raise_all_modes`, native-builtins `lang_invoke::segment_vh_raise_mode_through_jdk`).
    // r23 wave 5, lane L8: perf, default ON, kill "0". The compiled FFM element door asks its real-segment memo before
    // the carrier verdict table (types `NativeFlags::ffm_real_memo_first`, vm `jit_ffm_segment_get_body`).
    // r23 wave 5, lane L8: perf, default ON, kill "0". The compiled FFM element door resolves an Unsafe arena handle
    // through the lock-free JIT arena table instead of the locked ArenaStore copy (types
    // `NativeFlags::ffm_arena_jit_resolve`, native-builtins `unsafe_arena_jit_resolve`, vm `real_seg_serve`).
    // zt t31 (page zt31-direct-charbuffer-element-access-is-700x-hotspot): the scoped fold takes the aligned native-order short/char/int/long ScopedMemoryAccess accessors too (`cratonvm_jit::ir::ir_scoped_fold_aligned_enabled`).
    // gcj j3/x2: `ir-vh-read-binds` is the default-ON kill switch over every
    // optimizing-tier VarHandle read bind; the opt-in field / byte-array widening
    // it also carried (a mixed default) moved to `ir-vh-field-read-binds`.
    // JIT round 18 wave 11, lane osrfollow (perf, opt-in): the optimizing OSR
    // door builds without its every-header opaque merges first and rebuilds
    // with them only for a body whose guard exits need them (OSRPOLY18-2,
    // `cratonvm_jit::osr_narrow_opaque_door_enabled`); a single-use
    // `ExactClassIs` branches on its own flags (OSRPOLY18-1,
    // `ir_lower::ir_fused_class_test_branch_enabled`).
    // JIT round 19 wave 1, lane osrpoly19 (perf, opt-in): a guarded splice's
    // `ExactClassIs` sinks to its branch's block at equal loop depth
    // (OSRPOLY18-1, `ir_schedule::sink_class_test_to_branch_enabled`); the
    // optimizing tier's polymorphic guarded splice for OSR-door compiles only
    // (OSRFOLLOW18-3, `ir::osr_door_ir_polymorphic_splice_enabled`).
    // JIT round 19 proposal wave, lane osr19 (perf, opt-in): a bimorphic /
    // polymorphic guarded splice loads its receiver's class word once at the
    // chain head (`Op::ClassWordOf`) and compares it per class (OSRPOLY19-2 /
    // OSRFOLLOW18-2, `ir::ir_class_word_chain_enabled`); the narrowed OSR door
    // rebuilds every-header opaque when its body has no entry at the door's own
    // header (OSRFOLLOW18-1, `cratonvm_jit::osr_narrow_door_entry_rebuild_enabled`,
    // acts only under `osr-narrow-opaque-door`).
    // JIT round 19 proposal wave 2 (lane osr2), all opt-in: OSR19-2, the
    // one-class guarded splice's test as a class-word compare
    // (`ir::ir_class_word_single_test_enabled`); OSR19-1, the polymorphic-loop
    // OSR decline re-asked per loop header (`jit_bridge` /
    // `cratonvm_jit::osr_poly_decline_per_header_enabled`); FG16-5 /
    // IROPS19-6, a loop's int/long field cells kept in registers with the
    // stores sunk to the loop exit (`loop_field_forward::promote_enabled`).
    // JIT round 20 wave 11 (lane pLoop), default ON (`=0` kill): OSR4-2 / IROPS2-6 of
    // `docs/feature-designs/deopt-osr.md`, a call-free, deopt-free loop's
    // direct primitive static cells (`COUNTER += x`) kept in registers, written back at the exit
    // and by the poll pad (`loop_field_forward::static_promote_enabled`). Inert without
    // ir-loop-field-promote.
    // JIT round 20 wave 17, lane pAtom2 (handoff "Round-21 performance targets": the static counter
    // loops, `R20PLoopStaticCells`): perf, default ON (kill switch "0"). The loop static-cell promotion
    // above runs in a method-entry body too, not only in an OSR-door one (jit
    // `static_promote_method_entry_enabled`).
    // JIT round 20 wave 17, lane pAtom2 (page `jr20-loop-accumulator-fields-of-a-fresh-escaping-object-stay-in-memory-CLOSED-20261012.md`,
    // `R20PLoopStaticBarrier`): perf, default ON (kill switch "0"). The `int` / `long` fields of a fresh
    // object of this compile that nothing can publish (only field accesses through it, memory tokens,
    // a return, pure tests) are kept in registers through a call-free, deopt-free loop and written back
    // at its exit, in method-entry and OSR-door bodies (jit `loop_field_forward::fresh_promote_enabled`).
    // r21 loops
    // JIT round 21 wave 1, lane loops (HOIST13-2, `R21LoopAccum`): perf, opt-in. The field forward and the
    // loop cell promotion run in a METHOD-ENTRY body for the loops whose store census shows a field
    // accumulator of an invariant base (`this.sum += v`), and the forward counts the compiling method's
    // receiver as known non-null (jit `loop_field_forward::method_entry_promote_enabled`). Inert without
    // ir-loop-field-forward and ir-loop-field-promote.
    // JIT round 21 wave 1, lane loops (LOOPS21-2): perf, opt-in. Each loop-entry predicate planter's guard
    // carries its own speculation id and each planter asks `contains_speculation` with it, so a trapping
    // predicate withdraws only itself from the next compile (jit `deopt::predicate_own_speculation_enabled`;
    // the VM's trapped-callee sink names the id too).
    // JIT round 21 wave 1, lane loops (LOOPS21-3): consistency, default ON (kill switch "0"). `lib.rs` fills
    // `Graph::direct_static_fields` for the fresh-object loop promotion too, so that pass's A/B is no longer
    // tied to the static hoist / promotion / LICM read switches
    // (jit `loop_field_forward::fresh_promote_static_feed_enabled`).
    // JIT round 21 wave 1, lane loops (LOOPS21-1, `R21LoopAccum` array-elem): perf, opt-in. An `int` / `long`
    // array element with an invariant array and index (`a[k] += x`) is read once behind pre-header index
    // guards, carried in a register and written back at the loop exit
    // (jit `loop_field_forward::array_element_promote_enabled`). Inert without ir-loop-field-promote.
    // JIT round 21 wave 1, lane loops (w1 host run: the hot `R21LoopAccum` kernels are SPLICED into their
    // callers): perf, default ON since JIT round 24, kill "0". The field / static / fresh / element promotions reach a spliced callee's loop: a
    // base the splice null-checked before the loop counts as known non-null, and the exit write-back takes
    // the header's chain snapshot (jit `loop_field_forward::promote_spliced_enabled`).
    // JIT round 19 proposal wave 4 (lane osr4), opt-in: OSR2-1, a promoted
    // loop header's back-edge poll keeps its mode exit and its pad writes the
    // promoted cells back first (`ir_lower::ir_loop_field_promote_poll_writeback_enabled`);
    // OSR2-2, a class test of a fresh allocation folds to a constant
    // (`ir_optimize::fold_fresh_class_test_enabled`); IROPS2-4, the OSR entry
    // stub re-reads a hoisted float/double/reference direct static read
    // (`ir_lower::ir_osr_remat_static_read_wide_enabled`).
    // JIT round 19 proposal wave 5 (lane osr5), opt-in: OSR2-3 / OSR4-1, a promoted loop may hold
    // guards and primitive array accesses; its cell stores stay as silent stores and every deopt pad
    // inside the loop writes the cells back first (jit `loop_field_forward::pending_deopt_enabled`,
    // `ir_lower::plan_promoted_pending`). Inert without ir-loop-field-promote.
    // Same lane, opt-in: OSR2-4, a promoted loop may also exit from inside its body (`break`); each
    // such exit gets its own conditional write-back (jit `loop_field_forward::body_exits_enabled`).
    // Inert without ir-loop-field-promote.
    // JIT round 19 proposal wave 6 (lane osr6), opt-in: OSR5-2, a promoted loop whose deopt pad the
    // lowering would refuse is not promoted (a deopt point in a splice; an FP frame unless the pads
    // save XMM homes), and any remaining `promoted field cells` lowering refusal costs one rebuild
    // without deopt-pad promotions instead of the method (jit `ir_lower::ir_loop_field_promote_predict_enabled`,
    // `Graph::promote_pending_withheld`). Inert without ir-loop-field-promote-pending-deopt.
    // Same lane, opt-in: OSR5-4, a promoted loop's write-back pads (poll exits and deopt pads) save and
    // restore every XMM register their frame names around the field-helper calls, which lifts the
    // `promoted-field-cells-xmm` refusals (jit `ir_lower::ir_loop_field_promote_pad_xmm_save_enabled`).
    // JIT round 19 proposal wave 7 (lane osr7), opt-in: OSR6-1, a promoted loop's pending write-back
    // values (silent stores' values, the header value phi) are no longer deopt-named, so their home
    // stores can drop, and a pad describes each by its resident register when the value's live range
    // strictly contains the point (jit `ir_lower::ir_loop_field_promote_pending_resident_enabled`).
    // Inert without ir-loop-field-promote-pending-deopt.
    // Same lane, opt-in: OSR5-1, a deopt point inside a spliced body of a promoted loop takes the
    // write-back pad when it publishes a caller chain (resumes at the callee's own bci); a flat point
    // there still refuses (jit `ir_lower::ir_loop_field_promote_pending_chain_enabled`). Inert without
    // ir-loop-field-promote-pending-deopt.
    // Same lane, opt-in: OSR5-3, a promoted loop with a deopt point may keep a cell store under a
    // branch (a body memory phi); the pads read the walk's value phi at the join
    // (jit `ir_lower::ir_loop_field_promote_pending_branch_enabled`, `plan_promoted_pending`). Inert
    // without ir-loop-field-promote-pending-deopt.
    // JIT round 19 proposal wave 8 (lane osr8), opt-in: OSR7-1, each pending value a promoted loop's
    // deopt pad names is made live to just past the point in the slot plan's and the allocator's
    // ranges, so it keeps its register there instead of needing a home or refusing the compile
    // (jit `ir_lower::ir_loop_field_promote_pending_live_enabled`). Inert without
    // ir-loop-field-promote-pending-resident.
    // Same lane, opt-in: OSR7-2, the promotion admits body memory phis (a cell store under a branch)
    // in loops without a deopt point too, and in a loop with one (under
    // ir-loop-field-promote-pending-branch) walks every body memory phi so each join has its value
    // phi, kept alive for the pads (jit `loop_field_forward::join_walk_enabled`,
    // `Graph::promoted_join_phis`).
    // JIT round 19 proposal wave 3 (lane megaosr), opt-in: MEGAOSR19-1, the
    // optimizing OSR door declines a loop whose statically bound callee has a
    // site its own profile calls megamorphic, which a splice would make blind
    // (`cratonvm_jit::osr_decline_blind_mega_splice_enabled`).
    // JIT round 19 proposal wave 6 (lane osrentry6), opt-in: MEGAOSR19-3, an
    // activation whose loop header already has an enterable OSR body cached
    // (single-pass published, or optimizing accepted) is offered OSR after
    // `OSR_EARLY_REENTRY_BACKEDGES` back edges instead of the per-frame 1 000
    // (vm `interpreter::osr_early_reentry_due`, `env_cache::osr_early_reentry`).
    // JIT round 19 proposal wave 6 (lane osrentry6), opt-in: MEGA5-3's depth
    // walk, MEGAOSR19-1's decline also asks the bodies nested into the loop's
    // statically bound callee (`cratonvm_jit::osr_decline_blind_mega_nested_enabled`).
    // JIT round 19 proposal wave 7 (lane interp7), opt-in: STRGAP19-7, the
    // interpreter's `invokedynamic` string concat asks STRGAP19-1's exact-size
    // Latin-1 renderer first (vm `invokedynamic::execute_string_concat`,
    // `env_cache::interp_concat_latin1_fast`).
    // JIT round 19 proposal wave 8 (lane str8), opt-in: INTERP719-1, the cached
    // interpreter concat site carries STRGAP19-7's Latin-1 shape answer, decided
    // once at caching, instead of a per-call recipe scan (vm
    // `invokedynamic::execute_string_concat`, `ResolvedCallSite::StringConcat::latin1_shape`;
    // inert without interp-concat-latin1-fast).
    // JIT round 19 proposal wave 7 (lane interp7), opt-in: EAGER3-19-1 /
    // OSRENTRY6-1, the trap sink applies the site-trap policy only at a bci an
    // optimizing body of the method planted a site trap at (vm
    // `helpers::despeculate_trapped_method_inner`).
    // JIT round 19 proposal wave 7 (lane interp7), opt-in: OSRENTRY6-3, a
    // per-method back-edge accumulator lets MEGAOSR19-3's early window REQUEST
    // the OSR compile once the method's summed back edges cross the threshold
    // (vm `interpreter::osr_early_request_due`; needs
    // `CRATONVM_JIT_OSR_EARLY_REENTRY`).
    // t1 wave 8 lane ACCEPT (ACCEPT21-4, docs/feature-designs/wire-tiered-manager.md):
    // opt-in (default OFF), only under osr-early-request. An early OSR request the tiered manager
    // refuses charges nothing to the header or the method-wide budget (vm `interpreter::
    // osr_early_request_charges_nothing_enabled`). Pending the R21AcceptOsrEarly host A/B.
    // t1 wave 2 lane jit2 (B5 of docs/feature-designs/native-dispatch-architecture.md),
    // perf, opt-in: the single-pass OSR door gives a not-yet-compiled invokestatic callee the late-bind
    // JitCycleEdgeCell, filled by JitCache::put when the callee publishes (jit `jit_osr_late_static_cell_enabled`,
    // `JitCache::publish_osr_body_late_cells`; vm `jit_bridge::osr_late_static_cell_target`).
    // t1 wave 3 lane X (jit3; page t1-jit2-ir-osr-body-cells-never-registered), perf, default ON, `=0` kill (the opt-in
    // switch above still implies it): an optimizing OSR body's late / cycle-edge cells are registered so a callee's
    // publication fills them (jit `jit_ir_osr_cell_registration_enabled`, vm `jit_bridge::build_osr_optimizing_artifact`).
    // t1 wave 3 lane X (jit3; page t1-O-static-self-call-arm-is-the-fallthrough-for-any-unregistered-site), correctness,
    // default ON, `=0` kill: every single-pass door marks its raw static self-calls (JIT_SELF_CALL_GUARD row) and the
    // emitter refuses an invokestatic with no row, no info and no marker (jit `static_self_call_marker_enabled`).
    // t1 wave 5 lane X2 (page t1-X2-ir-direct-binds-plant-no-retire-cell), opt-in until measured: a dependency
    // invalidation (class change, new subtype, finalizable subtype) makes the method-entry bodies it withdrew not
    // entrant, as the trap eviction does, so an optimizing caller's baked CALL re-dispatches (jit
    // `jit_invalidate_not_entrant_enabled`, `JitCache::invalidate_matching_collecting_with`).
    // JIT round 19 proposal wave 11 (lane mon11), opt-in: the short-loop page's
    // `i + b` predicate, HotSpot's Tier3 `i >= 100 && i + b >= 2000` on RAW
    // summed back edges at the frame pop (vm `interpreter::credit_i_plus_b`;
    // needs `CRATONVM_JIT_LOOP_WORK_TIERUP`).
    // JIT round 19 wave 11 integration (2026-10-09): default ON, `=0` kill (A/B in the round record).
    // JIT round 19 proposal wave 11 (lane mon11), opt-in: MON10-1, a loop
    // inside a javac `synchronized` block keeps its single-pass OSR header
    // entry (vm `jit_bridge::is_javac_monitor_release_row`).
    // JIT round 19 wave 11 integration (2026-10-09): default ON, `=0` kill (A/B in the round record).
    // JIT round 19 proposal wave 12 (lane mon12), opt-in: MON11-1, the loop block
    // lock coarsening also admits a direct primitive `putstatic` in the block
    // (`cratonvm_jit` `ir_optimize::static_store_is_direct`).
    // JIT round 19 wave 12 integration (2026-10-09): default ON, `=0` kill (A/B in the round record).
    // JIT round 19 proposal wave 12 (lane mon12): MON11-4, the C1 -> C2
    // arm also promotes on HotSpot's Tier4 `i >= 600 && i + b >= 15000` over the
    // drained back edges (`cratonvm_jit::tiered::tier4_i_plus_b_due`).
    // zr7ca residual close (2026-10-25): default ON, `=0` kill (rows in docs/feature-designs/jit-ir-optimizations.md).
    // JIT round 19 proposal wave 4 (lane megaosr4), opt-in: MEGAOSR19-2, a
    // spliced body's surviving virtual / interface site gets its inline cache
    // seeded (MIC dominant receiver, megamorphic seed) from the CALLEE's own
    // receiver profile at the callee's bci
    // (`cratonvm_jit::ir_splice_callee_profile_seed_enabled`).
    // JIT round 19 proposal wave 4 (lane megaosr4), opt-in: MEGA2-4, a
    // single-pass MIC-only (over-wide) site's screened MIC misses go to the
    // hashed stub's gate entry without MEGA19-2's gate
    // (`runtime_lowering::sp_mega_mic_miss_gate_entry_enabled`).
    // JIT round 19 proposal wave 4 (lane megaosr4), opt-in: MEGA2-3, a
    // single-pass profile-seeded megamorphic site keeps its PIC arm, gate
    // first, so SP stage-once's register probe serves it
    // (`runtime_lowering::sp_mega_seeded_keep_probe_enabled`).
    // JIT round 19 proposal wave 5 (lane mega5), opt-in: MEGAOSR4-1, an
    // `invokeinterface` inside an IR-spliced body takes the nested guarded /
    // bimorphic splice its callee's own receiver profile names (`nest-profiled`,
    // until now `invokevirtual` only) (`cratonvm_jit::ir::ir_nested_guarded_interface_enabled`).
    // JIT round 19 proposal wave 5 (lane mega5), opt-in: the MEGAMORPHIC half
    // of MEGAOSR19-2's spliced-site seed, split off after the i7 regression
    // (own-site-wide-call 27 -> 150 ns); without it the seed sets the MIC only
    // (`cratonvm_jit::ir_splice_callee_profile_seed_mega_enabled`).
    // JIT round 16 wave 13 (MG16-3): a package-private override the receiver's JVMS selection picks is spliced
    // (vm `jit_bridge::receiver_resolution_is_dispatch_faithful`). Default ON since JIT round 20, `0` kills.
    // JIT round 23 wave 10, lane L2 (CF-W10-2): DEFAULT ON since R23-W11-FLIP (perf; mixed alone on n23-w10a, wins with the CF family on n23-w11b). The cold-nested
    // rule keeps spliced a nested call on the enclosing callee's profiled dominant path (jit
    // `splice_hot_path_pcs`).
    // JIT round 23 wave 10, lane L2 (CF-W10-1): DEFAULT ON since R23-W11-FLIP (perf; mixed alone on n23-w10a, wins with the CF family on n23-w11b). A spliced
    // callee's never-taken (or rare) arm prunes at 256 observations, not 2000 (jit
    // `spliced_branch_edge_prunable`).
    // JIT round 16 wave 8, lane sync: both default OFF (opt-in), pending a
    // loaded-run battery; see `jit_bridge::door_wrapped_body_attempt_admitted`
    // and `helpers::special_bytecode_callee_sync_enabled`.
    // JIT round 21, lane dispatch (page jr21-dispatch-five-class-site-goes-megamorphic-20261014): the IR planner marks its inline-cache slots MIC-first, so the MIC's class takes no inline way and the MIC plus the four ways hold five classes (the overflow and the profile seed count five). Default ON, "0" keeps the MIC's class in way 0 too.
    // JIT round 20 wave 18, lane xSplitIf (LOOPOPT20-1,
    // `docs/feature-designs/jit-ir-optimizations.md`): split-if for
    // an `If` on a phi of int constants at its own merge (a spliced boolean
    // getter's `iconst_1` / `iconst_0` join under `while (buf.hasRemaining())`):
    // each predecessor goes straight to the successor its constant selects
    // (`jit/src/ir_optimize/split_if.rs`). Default ON, `=0` keeps the join;
    // probe `R20XSplitIfLoops`.
    // JIT round 25 wave 2, lane branch-fold
    // (`docs/feature-designs/jit-ir-optimizations.md`),
    // default ON since JIT round 26 (R26-FLIP, `=0` kills each): a boolean phi a dominating `If` decides becomes that `If`'s
    // condition and a compare of a known boolean against a constant becomes the
    // boolean, its negated compare or a constant (`ir-bool-phi-fold`); an `If`
    // whose two arms are empty and agree at their join is removed
    // (`ir-empty-diamond-fold`); an `If` an edge of a dominating `If` decides
    // gets a constant condition (`ir-branch-implied-ifs`); the split-if also
    // through a memory phi at the join and with non-empty arms
    // (`ir-split-if-memory-phi`). Round 26: the split-if also over a partly
    // constant phi and through a compare / mask chain, a predecessor that is
    // not constant getting its own copy of the `If`, and across a join whose
    // predecessors decide a condition made above it (`ir-split-if-partial-phi`,
    // RB25-2 / RB25-4), and through value phis, rejoins and floating readers
    // (`ir-split-if-value-phi`, RB25-3). `jit/src/ir_optimize/bool_branch.rs`,
    // `split_if.rs`; `CRATONVM_DBG_JITC` prints `ir-branch-fold` per body.
    // JIT round 20 wave 19, lane yLoopPred (LOOPOPT20-3,
    // `docs/feature-designs/jit-ir-optimizations.md`): on the
    // optimizing OSR route, a loop an opaque OSR arm enters through its
    // pre-header gets its direct static reads moved there and its constant-mask
    // loop-entry guard (`ir_optimize::osr_route_arm_predicates_enabled`).
    // Default ON, `=0` restores the old route; probe `R20YLoopPredRows`.
    // JIT round 20 wave 19, lane yLoopPred (LOOPOPT20-2 / L5W6-2): a full
    // pure-body unroll clones a masked `ArrayLoad` whose value feeds the loop,
    // each copy pc-less and licensed by the loop-entry mask guard as its guard
    // token (`ir_optimize::unroll_clone_masked_load_enabled`). Default ON, `=0`
    // keeps such a loop rolled; probe `R20YLoopPredRows`.
    // JIT round 20 wave 19, lane yLoopPred (LOOPOPT20-6): the early mask plant
    // sees javac loops without the builder's single-input merge wrappers, and
    // the pure-body escape no longer counts the pc of a node that never reads
    // the flat snapshot at it (a named-frame loop-entry predicate, a pure data
    // node, a direct `getstatic`); the lowering refuses a by-bci transfer at a
    // stranded bci (`ir_optimize::unroll_named_frame_pcs_enabled`). Default ON,
    // `=0` restores both; probe `R20YLoopPredRows`.
    // JIT round 21, lane loopopt (TYPECHECK21-6, page jr21-typecheck-lane-perf-rows-20261014): after a loop promotion (field, static or fresh-object cells) leaves a register-only loop, the graph is offered to the unroller once more when the loop rounds unrolled nothing. Opt-in ("1") since 2026-10-26: ON kept R20PLoopStaticCells static-counter-long at ~0.9 ns per iteration where OFF removes the loop (0.0, as HotSpot), and changed no other measured body.
    // JIT round 21, lane loopopt (LOOP21-1, page jr21-typecheck-lane-perf-rows-20261014): the static and fresh-object loop promotions admit an in-loop field read of the receiver (or another base known non-null at the entry) instead of refusing it as a node that can deopt. Opt-in ("1") since 2026-10-26: R20PLoopStaticBarrier.run promotes without it, and no measured row's steady-state body changed with it (in-process instruction counts equal to the instruction).
    // JIT round 21, lane tcshape (page jr21-typecheck-lane-perf-rows-20261014, row late-r20-shape): a body compiled with a deferred field site (its owner not loaded at compile time) is recompiled once the site has linked, and a single-pass OSR body that reads a not-yet-initialized class's statics through the helper is rebuilt when that class finishes initializing (vm `jit_bridge::deferred_field_recompile_enabled`). OPT-IN ("1" arms it; the reader is `runtime_flag_on`, `off_word` is `None`; this comment said "Default ON" until tcm21): it lost R21TcshapeLateInit `unloaded` in the w3o A/B, and the same switch gates both OSR static-init waiters (`note_osr_init_waiters`, `note_osr_optimizing_static_init_waiters`), so their `loaded-uninit` fix is inert by default; the decision tree is on tcm21-typecheck-miss-and-shape-gates-20261102.md.
    // JIT round 21, lane promote (TYPECHECK21-9, page docs/feature-designs/real-frame-deopt.md): the static and fresh-object loop promotions admit a loop holding a deopt point (guard, primitive array access / length, integer division); its cell stores stay silent and every deopt pad in the loop writes the cells back first (jit `ir_lower::ir_loop_promote_deopt_pads_enabled`). Default ON since the w3o A/B, "0" restores the refusal.
    // JIT round 21 lane promote residual (PROMOTE21-1, page docs/feature-designs/real-frame-deopt.md): with the deopt pads on, the static and fresh-object loop promotions also admit a cell store under a branch (a body memory phi); the pads read each join's value phi (jit `ir_lower::ir_loop_promote_deopt_pads_branch_enabled`). Opt-in: flat on its rows.
    // Found retiring TYPECHECK21-9 (page docs/feature-designs/real-frame-deopt.md): the fresh-object loop promotion takes only an allocation the method returns; one never returned is left to escape analysis, whose deletion of it otherwise left the pre-header frame copy unclaimed and the method rejected by the IR verifier (jit `loop_field_forward::fresh_base_returned`). Default ON, "0" admits every unpublished allocation as before.
    E { group: Group::JIT, token: "field-array-bce", on_key: Some("CRATONVM_JIT_FIELD_ARRAY_BCE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "field-int-hoist", on_key: Some("CRATONVM_JIT_FIELD_INT_HOIST"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "sr-slots-past-licm", on_key: Some("CRATONVM_JIT_SR_SLOTS_PAST_LICM"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    // The chain arm (CHN15-7): the umbrella `ir-chain-arm` and two of the
    // three switches it stages (the third, `ir-splice-multi-return`, is above).
    // Default ON since round 16 (measured: CratonBench hashmap 645->564 ms,
    // strings 3448->2632 ms): `ir::CHAIN_ARM_DEFAULT_STAGE` is `Full`, so unset
    // means ON for all four and each is a KILL SWITCH whose parser reads `"0"`
    // as off (`ir_chain_arm_stage`, `chain_arm_switch_on`).
    E { group: Group::JIT, token: "ir-splice-frame-states", on_key: Some("CRATONVM_JIT_IR_SPLICE_FRAME_STATES"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::JIT, token: "ir-splice-chain-fences", on_key: Some("CRATONVM_JIT_IR_SPLICE_CHAIN_FENCES"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    // JIT round 17 wave 3, lane mega3 (MG16-7): default OFF (performance, opt-in). Raises the one
    // inline-cache stack-word cap (`cratonvm_jit::inline_cache_pic::ic_max_stack_words`) from 8 to 16.
    // JIT round 19 (lane selflock19): SYNCDOOR618-1, `static synchronized` with blocks (opt-in).
    // JIT round 19 wave 3 (lane syncdoor3): SYNCDOOR3-1, the handlers body check admits mode exits that hand the hold over (opt-in).
    // JIT round 19 wave 4 (lane syncdoor4): SYNCDOOR4-1, a statically bound site of a synchronized instance callee with no published body compiles its self-locking body in place and binds it (opt-in).
    // JIT round 21, lane strintr2 (STRINTR21-1, page `jr21-strintr-string-pin-residual-20261015.md`, probe
    // `R21StrintrRows` charat-oob): a String length/isEmpty/charAt site the IR accessor expander left an `Op::Call`
    // takes the declining StrLength/StrIsEmpty/StrCharAt prefix (jit `ir_string_accessor_call_prefix_enabled`; inside
    // `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`; moves no pin / splice decision). Default ON, kill "0".
    // JIT round 21, lane strintr2 (STRINTR21-1, same page and row): a String accessor site whose deopting guard the
    // VM's de-spec registry withdrew is built as its call instead of being re-expanded (jit
    // `ir_string_access_trap_withdraw_enabled`). Default ON, kill "0".
    // JIT round 21 wave 20, lane sbfuse (STRINTR21-4, page `jr21-strintr2-sb-chunk-builder-chain-is-not-fused-FIXED-20261027.md`,
    // probe `R21SbfuseRows`): an explicit `new StringBuilder().append(..)...toString()` chain that does not escape
    // (String/char/int/long operands of pure code) is one inline concat in the optimizing tier, the bridge its decline,
    // and its builder sites leave the String-intrinsic pin (jit `sb_chain_fuse.rs`, `IrBuilder::sb_chain_fuse_step`,
    // vm `make_jit_string_concat_site_from_recipe`). Default ON, kill "0".
    // JIT round 19 wave 5 (lane sync5): SYNC5-1, a self-locking run that only overflowed its code-buffer estimate runs again at the measured size instead of memoizing a self-lock refusal (nested-block bodies; opt-in).
    // JIT round 19 wave 5 (lane sync5): SYNCDOOR4-2, a self-locking body's statically bound recursive self-call CALLs its own locking entry (opt-in).
    // JIT round 19 wave 5 (lane sync5): SYNC5-2, the SYNCDOOR4-1 eager bind also compiles a static synchronized callee whose class mirror slot exists or can be minted on this thread (opt-in).
    // JIT round 19 wave 5 (lane sync5): SYNC5-3, the OSR door's statically bound sites to a synchronized callee with no self-locking body yet: compile it in place (with the eager bind) or wait for it (late bind, method-entry recompile) (opt-in).
    // `gpu-wait-latch` is DEFAULT-ON with a "0" off-word, same argument as
    // `gpu-device-pool`: skipping a `cuStreamWaitEvent` on an event that has
    // already fired has no observable semantics, so the only honest way to
    // JIT round 21, lane charat (STRINTR21-5, page `jr21-strintr2-charat-loop-per-char-node-cost-FIXED-20261101.md`, probe
    // `R21CharatRows`): the IR `charAt` expansion reads the char through one `Op::StrCharDecode` (`TEST coder; MOVZX`)
    // behind its bounds guard instead of two checked byte loads and a six-node combine (jit
    // `ir_string_char_decode_enabled`). Default ON, kill "0".
    // JIT round 21, lane charat (STRINTR21-5, same page and probe): an IR `Guard` whose condition a dominating fact
    // proves (`x != null` past a dereference or null guard, `a < b` under a loop test or an earlier guard, `x >= 0`
    // for a unit-stride induction) emits nothing (jit `ir_check_elim::guard_fact_elide_enabled`). Default ON, kill "0".
    // price it is one binary both ways.
    // Measurement lever: root every heap-backed LinkedHashMap overlay entry
    // again, restoring the unbounded young-gen pinning the skip-set removed.
    E { group: Group::GC, token: "max-inflated-bytes", on_key: Some("CRATONVM_MAX_INFLATED_BYTES"), off_key: None, off_word: None, since: "2026-06-17" },
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
    // Resolving the innermost JIT frame's own method from the direct CALL that
    // built it, instead of giving up whenever that method is not the chain
    // entry's. Presence-parsed opt-out, so `=0` still disables it and
    // `off_word` must stay `None`.
    E { group: Group::GC, token: "oldgen-coalesce", on_key: None, off_key: Some("CRATONVM_NO_OLDGEN_COALESCE"), off_word: None, since: "2026-08-01" },
    E { group: Group::GC, token: "oldgen-compact", on_key: Some("CRATONVM_OLDGEN_COMPACT"), off_key: None, off_word: Some("0"), since: "2026-08-03" },
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
    // Experimental (off by default): heap-specializing tier (page-protect
    // static-final graphs and demanded receiver pages read-only, folding
    // stable reads in JIT; `frozen_pages::enabled` reads `runtime_flag_on`).
    E { group: Group::GC, token: "frozen-pages", on_key: Some("CRATONVM_FROZEN_PAGES"), off_key: None, off_word: None, since: "2026-11-16" },
    // gen r4w5/concmark5: opt-in concurrent-cycle SERVICE thread for the
    // generational heap (the cycle runs on it, not on the mutator whose
    // young-collection epilogue found it due). `non_empty_non_zero`.
    E { group: Group::GC, token: "gen-conc-service-thread", on_key: Some("CRATONVM_GEN_CONC_SERVICE_THREAD"), off_key: None, off_word: None, since: "2026-09-24" },
    // gcg g2/r: opt-in -- the concurrent sweep prunes reference rows once per pause boundary /
    // cap / sweep end, not once per slice (`ReferenceProcessor::flush_deferred_sweep`).
    // gcg g3/k (opt-in): blocked transitions no longer maintain the anonymous
    // blocked counter (`vm/src/threading/gc_barrier.rs`, read per `GcBarrier`).
    // gen r4w5/concmark5: opt-in no-op remark-time reference-processing hook
    // (the call site only; counts itself, clears nothing). `non_empty_non_zero`.
    E { group: Group::GC, token: "gen-conc-remark-refproc-hook", on_key: Some("CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // gci i42/mk: default ON (Generational remark flushes every thread SATB bucket before its verdicts), =0 kills.
    E { group: Group::GC, token: "gen-conc-remark-thread-satb-flush", on_key: Some("CRATONVM_GEN_CONC_REMARK_THREAD_SATB_FLUSH"), off_key: None, off_word: Some("0"), since: "2026-10-16" },
    // gen r5w1/refs5: opt-in generational SATB young-value filter (the barrier
    // logs old-gen old values only). `non_empty_non_zero`.
    E { group: Group::GC, token: "gen-satb-old-only", on_key: Some("CRATONVM_GEN_SATB_OLD_ONLY"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    // gen r5w1/refs5: the allocation-failure collection door asks the
    // generational concurrent start trigger (and runs a due cycle inline), as
    // `maybe_gc`'s epilogue does. DEFAULT-ON since gen r5w2/conc6
    // (`on_unless_zero`), so a KILL SWITCH and `off_word` is `"0"`.
    // gcg g4/oo (opt-in): with no service thread, the allocation-failure door
    // owes a due concurrent cycle to the next safepoint poll instead of running
    // it inside the failing allocation (`gc_and_alloc::gen_owed_concurrent_cycle_at_poll`).
    // gen r5w3/unload7: opt-in concurrent class unloading on the generational
    // concurrent cycle (initial mark and remark under the class-unload root
    // licence, side-table edges in the marker, unload at remark). Needs
    // `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`. `non_empty_non_zero`.
    E { group: Group::GC, token: "gen-conc-class-unload", on_key: Some("CRATONVM_GEN_CONC_CLASS_UNLOAD"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    // gen r5w1/refs5: HotSpot `LRUMaxHeapPolicy` inputs for the SoftReference
    // policy (soft clock and free heap as of the previous collection). Every
    // backend. gen r5w4/defaults8: PER-BACKEND default
    // (`flags::alloc_policy_defaults::SOFTREF_HOTSPOT_LRU`; the VM's effective
    // default is ON on all three heaps: Generational, G1 since gci i5g/g1o via
    // `vm_init::soft_lru_takes_hotspot_inputs_by_default`, ZGC since zr z1/rf
    // via `SOFTREF_HOTSPOT_LRU_ZGC`), parsed by `backend_policy_switch`,
    // so the minus token must write an explicit `0` — unsetting the key would
    // leave the default on. Hence `off_word: Some("0")`.
    E { group: Group::GC, token: "softref-hotspot-lru", on_key: Some("CRATONVM_SOFTREF_HOTSPOT_LRU"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    // gch h2/jr (gch-h1ak-soft-lru-free-figure-is-young-bounded-not-hotspots): the soft LRU
    // takes HotSpot's free figure, `MaxHeapSize - used` (`ref_discovery::soft_lru_free_mb`); Generational
    // since gch h2/jr, G1 and ZGC since gci i48/zl2. Default ON (user triage 2026-10-09, gch h4/wr), `=0` restores
    // each collector's previous figure (Gen: young-bounded; G1: soft capacity - allocated; ZGC: arena - allocated).
    E { group: Group::GC, token: "softref-hotspot-free", on_key: Some("CRATONVM_SOFTREF_HOTSPOT_FREE"), off_key: None, off_word: Some("0"), since: "2026-10-08" },
    // gen r4w3/oldgen3: opt-in give-back of large free old-gen granules after
    // an in-place sweep (RSS vs re-fault cost; A/B pending). `non_empty_non_zero`.
    E { group: Group::GC, token: "old-give-back", on_key: Some("CRATONVM_GC_OLD_GIVE_BACK"), off_key: None, off_word: None, since: "2026-09-23" },
    // gen r4w4/oldgen4: default-ON decommit of the old generation's free tail
    // after a stop-the-world old-gen collection (HotSpot's MaxHeapFreeRatio);
    // `=0` is the kill switch. `on_unless_zero`.
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
    // gcf f1/c: a moving cycle's Phase 5 major roots this pause's promotion
    // destinations (default on; `=0` restores the old seed).
    // gch h2/og; default ON since jr20 GC wave 9 lane A (gcd-d4j, host w9c: witness SAME, cost rule C2 passed), `=0` kills
    // -- P-C: a moving cycle's Phase 5 major seeds young from the true roots.
    // gcki-ngr: the last-ditch recovery pause skips the staged full compaction once the failed
    // humongous request fits (`vm/src/runtime/interpreter/gc_and_alloc.rs`). Default ON, `=0` kills.
    // gci i3/og (default ON since vi7-flip, `=0` kills; with P-C): the moving true-root major judges the watched young copies its trace left out dead, as WATCHED_DEAD does for the non-moving one.
    // gch h4/pc; default ON since jr20 wave 9 lane G (vh4-pc A + D on w9c) and gci i1/og, `=0` kills -- the
    // true-root young rounds folded into the old mark's BFS: each popped
    // object's young targets are seeded by the scan it already gets, so `round` only drains
    // (`gch-h3mj-proposal-true-root-rounds-rescan-every-marked-old-object-DONE-20261015`).
    // `runtime_flag_default_on` (`gc::gen_heap::true_root_rounds_in_bfs_enabled`).
    // gch h4/pc; default ON since jr20 wave 9 lane G and gci i1/og, `=0` kills -- a true-root major skips the
    // post-finalizer `close_live_set` after an
    // idle finalizer round, and the finalizer seed's own one after the pre-close.
    // `runtime_flag_default_on` (`gc::gen_heap::true_root_skip_idle_post_close_enabled`).
    // gci i3/og (default ON, `=0` kills): the default non-moving true-root major takes P-C's batched dead-promotion verdict and grid-index reached set (exact).
    // gcf f1/c: the concurrent start's buffer covers the recent growth step (opt-in; conc8).
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
    // gen r4w4/oldgen4: default-ON walk-gap recovery for the in-place old-gen
    // sweep (conservative scan of the unwalked bytes); `=0` restores the
    // fail-closed sweep. `on_unless_zero`.
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
    E { group: Group::GC, token: "old-shrink-after-concurrent", on_key: Some("CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
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
    // gen r5w3/oldgen7: default ON since gcg g6/yg (=0 kills) — the old generation
    // may grow past its default
    // share to HotSpot's `MaxHeapSize - MaxNewSize` after a refused
    // allocation, the young trigger ceding the budget. `non_empty_non_zero_default_true`.
    E { group: Group::GC, token: "old-borrow-young", on_key: Some("CRATONVM_GC_OLD_BORROW_YOUNG"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    // gen r5w3/oldgen7, default-ON since the GC defects round: humongous arrays
    // are carved from the TOP of the old generation, committing only their own
    // pages. `on_unless_off_word_cased`.
    E { group: Group::GC, token: "old-humongous-top", on_key: Some("CRATONVM_GC_OLD_HUMONGOUS_TOP"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    // gen r5w5/old9: DEBUG — `=<n>` plants one unsizable header in a fresh,
    // unreferenced old-gen block at the end of the first stop-the-world
    // old-gen collection at or past the n-th, so the walk-gap recoveries can
    // be exercised on demand. Deliberately makes a heap anomaly; the token
    // form (`CRATONVM_DBG=old-plant-walk-break`) plants at the first.
    // gen r5w6/conc10: opt-in — the generational concurrent cycle's two pauses
    // take their young→old seeds from the LIVE young set (the young
    // collector's own, widened to every old holder), and, when the cycle
    // processes references at remark, not through the referent slot of an
    // active young `Reference`. Any doubt falls back to the all-young walk.
    // `runtime_flag_on` (`gen_heap::y2o_live_seed_enabled`).
    E { group: Group::GC, token: "gen-y2o-live-seed", on_key: Some("CRATONVM_GEN_Y2O_LIVE_SEED"), off_key: None, off_word: Some("0"), since: "2026-09-27" },
    // gen r5w6/conc10: default ON (`=0` kills) — a Generational young-collection root scan under
    // the loader-conditional licence defers a YOUNG user-loader mirror as well
    // (reached through its loader's `mirror_pin` row), and forces the young
    // cycle in place when it did, so the mirror is no longer a pinned root
    // value and can be promoted. `runtime_flag_default_on`
    // (`vm::memory::roots::young_mirror_defer_licensed`).
    E { group: Group::GC, token: "gen-young-mirror-defer", on_key: Some("CRATONVM_GEN_YOUNG_MIRROR_DEFER"), off_key: None, off_word: Some("0"), since: "2026-09-27" },
    // gch h3/mj; opt-in, diagnostic -- g4og step 0: at a concurrent initial mark, count the
    // seeded old objects only old holders' young children name (`[GC] conc_y2o_holders:`).
    // `runtime_flag_on` (`gen_heap::y2o_old_holder_census_enabled`).
    E { group: Group::GC, token: "gen-conc-y2o-old-holder-census", on_key: Some("CRATONVM_GEN_CONC_Y2O_OLD_HOLDER_CENSUS"), off_key: None, off_word: None, since: "2026-10-08" },
    // gcd d2/g: opt-in — the non-moving young sweep's selective promotion no
    // longer pins a young root value whose every occurrence in the root list
    // came from a precise table the post-collection fix-up rewrites (statics,
    // interned strings, class mirrors, JNI globals), so such an object can be
    // promoted under a live JIT. `runtime_flag_on`
    // (`gc::gen_heap::precise_root_promote_enabled`).
    E { group: Group::GC, token: "gen-precise-root-promote", on_key: Some("CRATONVM_GEN_PRECISE_ROOT_PROMOTE"), off_key: None, off_word: Some("0"), since: "2026-09-27" },
    E { group: Group::GC, token: "gen-conc-pause-no-precise-root-table", on_key: Some("CRATONVM_GEN_CONC_PAUSE_NO_PRECISE_ROOT_TABLE"), off_key: None, off_word: Some("0"), since: "2026-10-13" },
    // gcd d9/e; default ON since gce (2026-09-29), `=0` is the kill switch —
    // an open Generational concurrent cycle takes precedence
    // over the STW old-gen collection until an old-gen allocation actually
    // fails (no 90 % ceiling), and a measured start prediction is no longer
    // held back by the start's growth hysteresis. `runtime_flag_on`
    // (`gc::concurrent_mark::gen_conc_precedence_enabled`).
    E { group: Group::GC, token: "gen-conc-precedence", on_key: Some("CRATONVM_GEN_CONC_PRECEDENCE"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::GC, token: "gen-conc-preempt-growth-net-of-sweep", on_key: Some("CRATONVM_GEN_CONC_PREEMPT_GROWTH_NET_OF_SWEEP"), off_key: None, off_word: Some("0"), since: "2026-10-13" },
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
    E { group: Group::GC, token: "gen-conc-inline-start", on_key: Some("CRATONVM_GEN_CONC_INLINE_START"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    // gen r4w5/thrash5: default-ON kill switch over the mutator-progress half
    // of the GC-overhead limit and the Generational "full collection, then the
    // error" exit of the interpreter's allocation ladder; `=0` restores both
    // (`runtime::interpreter::gc_overhead_progress_enabled`).
    E { group: Group::GC, token: "overhead-progress", on_key: Some("CRATONVM_GC_OVERHEAD_PROGRESS"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // gcd d2/j: default-ON liveness fix, `=0` restores the forced young cycle
    // per object-door entry; Generational only
    // (`runtime::interpreter::gc_and_alloc::futile_young_backoff_skips`).
    E { group: Group::GC, token: "futile-young-backoff", on_key: Some("CRATONVM_GC_FUTILE_YOUNG_BACKOFF"), off_key: None, off_word: Some("0"), since: "2026-09-27" },
    // gcf f1/o: opt-in -- a refused JNI allocation runs the shared native ladder
    // and retries once when no native copy can go stale
    // (`native::jni::jni_alloc_or_oom`). `runtime_flag_on`.
    E { group: Group::GC, token: "jni-in-native-repeat", on_key: Some("CRATONVM_JNI_IN_NATIVE_REPEAT"), off_key: None, off_word: Some("0"), since: "2026-10-01" },
    E { group: Group::GC, token: "jni-bulk-array-copy", on_key: Some("CRATONVM_JNI_BULK_ARRAY_COPY"), off_key: None, off_word: Some("0"), since: "2026-10-01" },
    E { group: Group::DBG, token: "jni-repeat-audit", on_key: Some("CRATONVM_DBG_JNI_REPEAT_AUDIT"), off_key: None, off_word: None, since: "2026-10-01" },
    // gcg g2/w: one stderr line per weak-table sweep.
    // gcg g6/jm: default ON (vg5 jm5_aa/cs); =0 restores the check-only inline
    // barrier on both tiers.
    // gcg g4/oo: default ON, `=0` restores g2a. Under the HotSpot overhead
    // default the limit is a crawl backstop: eight forced Generational cycles
    // in a row that starved the program (old wedged, < 2 % of the heap
    // allocated since the last forced cycle, live set flat) end the crawl with
    // `Java heap space` after a full collection (`gc_and_alloc::gc_cycle_is_progress_starved`).
    // gcd d9/a item 1; default ON since gch h3/mj, `=0` kills (a HotSpot-parity fix: under concurrent-first the
    // window's unrequested STW major left weak refs to a dead holder chain set, vh3b-mj B2) -- every non-moving STW major seeds young from the true roots.
    // gch h4/wr (gch-h3jx-gen-true-root-major-keeps-unstarted-compat-threads-one-cycle): default ON since jr20 wave 9 and gci i1/og, `=0` kills -- the non-moving
    // true-root major does not seed the watched set; a watched young survivor its trace left out is judged dead
    // (identity entry removed, `GenerationalHeap::true_root_watched_left_out`), so its weak refs clear as HotSpot's.
    // gci i10/lp: default ON since gcj yg3-residuals (2026-10-26), `=0` kills -- a moving young copy answers the
    // instance->loader edge from one registry snapshot instead of the registry's read lock per copied object.
    // gcf f4/o: default ON since gcg g6, `=0` kills -- natives inside a VM-raised
    // OOME's construction skip the funnel's post-OOME debt.
    // gcf f4/o; default ON since the gcg triage (tg1), `=0` restores the shared path -- capturing-lambda proxies try the thread's TLAB first (legacy shape).
    // gcg g2/e; default ON since gcg vg2 (g4/og F2), `=0` restores the binary search -- the walked closure admits through a start bitmap.
    E { group: Group::GC, token: "old-close-bitmap", on_key: Some("CRATONVM_GC_OLD_CLOSE_BITMAP"), off_key: None, off_word: Some("0"), since: "2026-10-01" },
    // gci i41/ev: default ON (old-gen free-list search starts at the class containing the request), =0 kills.
    E { group: Group::GC, token: "old-exact-start-class", on_key: Some("CRATONVM_GC_OLD_EXACT_START_CLASS"), off_key: None, off_word: Some("0"), since: "2026-10-16" },
    // gcj j1/og: opt-in since the j1c rows (the compactors' Phase 2 resolves survivors through a sorted block-indexed table, not a hash map).
    E { group: Group::GC, token: "old-compact-forward-index", on_key: Some("CRATONVM_GC_OLD_COMPACT_FORWARD_INDEX"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // gcj j1/og: default ON since gcj j2/og2 B4 (2026-10-27), `=0` kills (HotSpot's MinHeapFreeRatio expansion of the old generation after a STW collection, up to 2/3 -Xmx).
    E { group: Group::GC, token: "old-free-ratio-growth", on_key: Some("CRATONVM_GC_OLD_FREE_RATIO_GROWTH"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // gcj j1/og: opt-in since the j1c rows (the full-GC mark answers the instance -> loader edge from one loader_pin snapshot per collection, not a registry lock per marked object).
    // gcj j2/og2 -- begin
    // gcj j2/og2: default ON (a Generational concurrent cycle's mark bitmap spans the old generation's growth limit, so a growth before the initial-mark pause no longer wastes the cycle), =0 kills.
    // gcj j2/og2: opt-in (the old generation's post-collection shrink is damped by HotSpot's _shrink_factor 0/10/40/100 % of the excess, not the all-or-nothing committed-end-advanced rule).
    E { group: Group::GC, token: "old-shrink-factor", on_key: Some("CRATONVM_GC_OLD_SHRINK_FACTOR"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // gcj j2/og2: opt-in (the stop-the-world old-gen mark pops through an 8-entry ring that prefetches each header as it enters, instead of reading it cold off the LIFO worklist).
    E { group: Group::GC, token: "old-mark-prefetch", on_key: Some("CRATONVM_GC_OLD_MARK_PREFETCH"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // gcj j2/og2: opt-in (the full-GC slide's Phase 2, the reference rewrite, runs on scoped worker threads in contiguous survivor chunks; 1 = min(cpus, 4) workers, n >= 2 = n, at least 65536 survivors).
    E { group: Group::GC, token: "old-par-adjust", on_key: Some("CRATONVM_GC_OLD_PAR_ADJUST"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // t1 wave 8 lane J1OG (gcj-j1og, the in-mark closures / Phase 0): opt-in (the old-gen live-set closure first asks its read-only question -- does any marked object reach an unmarked walked base? -- on scoped workers over contiguous chunks; the serial closure runs only when one does; 1 = min(cpus, 4) workers, n >= 2 = n; serial below 65,536 walked objects).
    E { group: Group::GC, token: "old-par-close", on_key: Some("CRATONVM_GC_OLD_PAR_CLOSE"), off_key: None, off_word: None, since: "2026-10-26" },
    // t1 wave 4 lane G2 (step 4 of gcj-j1og-proposal-parallel-full-gc-adjust-and-mark): opt-in (the stop-the-world old-gen mark drains a wide worklist on scoped worker threads with a CAS mark claim and a shared overflow stack; 1 = min(cpus, 4) workers, n >= 2 = n; never inside the finalizer resurrection round).
    E { group: Group::GC, token: "old-par-mark", on_key: Some("CRATONVM_GC_OLD_PAR_MARK"), off_key: None, off_word: Some("0"), since: "2026-10-26" },
    // gcj j3/og3: opt-in (the full-GC slide skips Phase 0 when the old mark recorded no unfollowed reference and nothing marked without a push).
    // gcj j3/og3: debug opt-in (a compaction handed a closed-mark verdict runs Phase 0 anyway and counts each disagreement).
    E { group: Group::GC, token: "old-compact-trust-mark-verify", on_key: Some("CRATONVM_GC_OLD_COMPACT_TRUST_MARK_VERIFY"), off_key: None, off_word: None, since: "2026-10-19" },
    // gcj j3/og3: opt-in (an unrequested major runs when the padded average promotion no longer fits the old generation's free space, HotSpot Serial's promotion_attempt_is_safe, instead of at 75 % occupancy).
    E { group: Group::GC, token: "old-trigger-serial", on_key: Some("CRATONVM_GC_OLD_TRIGGER_SERIAL"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // gcj j3/og3: default ON since 2026-10-25, `=0` kills, WARNING gcj-j2-residuals-flip-20261025 (a grown old-gen capacity shrinks back toward the construction capacity after a STW collection, and the young trigger gets the ceded budget back).
    E { group: Group::GC, token: "old-capacity-shrink", on_key: Some("CRATONVM_GC_OLD_CAPACITY_SHRINK"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // gcj j3/og3: default ON since 2026-10-25, `=0` kills (the Tenured Gen pool max is the old generation's fixed growth limit, HotSpot Serial's tenured maximum, not its current capacity).
    // gcj j3/og3 -- end
    // gcg g3/e; default ON since gcg g6/og (vg5 og5_ss_*), =0 off -- one walked start
    // set per collection for the mark and every closure.
    // gcg g4/rs (opt-in): a `java.lang.ref` reference is registered at its
    // allocation and bound at the next collection (`vm/src/memory/ref_discovery.rs`).
    E { group: Group::GC, token: "ref-discover-at-allocation", on_key: Some("CRATONVM_REF_DISCOVER_AT_ALLOCATION"), off_key: None, off_word: None, since: "2026-10-01" },
    // Interpreter round i7 wave 143 (lane L6; proposal i77-L5, performance,
    // default ON since 2026-10-28, `=0` restores the locked push; WARNING
    // i77-ref-notes-flip-20261028): a `Reference`'s allocation note goes to a
    // striped per-VM buffer (`cratonvm_gc::reference::AllocationNoteStripes`),
    // not under the reference processor's mutex; every processor reader
    // absorbs it first.
    E { group: Group::GC, token: "ref-notes-per-thread", on_key: Some("CRATONVM_REF_NOTES_PER_THREAD"), off_key: None, off_word: Some("0"), since: "2027-01-09" },
    // t1 wave 8, lane PIN10: age the pinned and kept survivors a pinned
    // in-place young cycle leaves where they are, and count them in the age
    // table, as the sweep and HotSpot age every survivor. Default ON since gcki
    // (2026-10-28), `=0` kills.
    // gcf f3/q (2026-10-01): the young-allocation-premium cuts of the
    // generational non-moving sweep (r15w5/gcpage patches). Default ON, `=0`
    // kill: the per-walk size memo, the batched selective-promotion census,
    // fix-up 3a's replay of the mark set. (The opt-in listed-slot 3c and the
    // `evac_map` reserve were deleted by gcg g4/yg.)
    // gch h1/yy (opt-in): the non-moving sweep's selective-promotion walk and its dirty-card
    // seed on the evacuation pool (the large-block-sparing TLAB refill was deleted by gch h3: vh2-ob B / vh1-yy).
    // gch h3/yc: the selective-promotion walk on the pool is default ON (`=0` restores the serial walk).
    // gci i1/yg (orchestrator vi1-orch T3) and JIT r20 wave 9, lane gcI: the dirty-card seed on the pool is
    // default ON too (`=0` restores the serial seed; gm-I.sh on w9c, 4 of 5 interleaved pairs).
    // gcj j1/pn (2026-10-19), one block:
    // - opt-in: the pinned in-place young cycle seeds only what a pin word NAMES by the watched arm's rule, for every object (gci-i30wy).
    // - default ON (`=0` kills): a pinned cycle never reclaims an unvisited SEED (a seed whose forward was refused before the pinned arm).
    // - default ON (`=0` kills): a licensing young-pin deposit whose sweep cannot reach the stack top is marked incomplete.
    // - opt-in, Generational only: the conservative interpreter-locals probe skips `long` / `double`-kind slots (minted handles still root).
    // gcj j1yg-pause: default ON since 2026-10-27, `=0` kills -- the named rule roots only what a pin word names; on JIT-warm javac the page rule doubled the copied objects of a pinned cycle.
    // gcj j1yg-pause round 3 (2026-10-27): default ON, `=0` kills -- under the named rule, a pinned cycle's seeds come only from the pin words the pause's root slice also holds (plus walk-gap and blocked-peer words); a ledger-only word still pins its page.
    // gcj close (2026-10-23): default ON (`=0` kills), was opt-in -- the conservative probes honour the long/double kind mark.
    // gcj j2/pn2 (2026-10-19), one block:
    // - default ON (`=0` kills): a pinned in-place cycle keeps a pinned object whose forward was refused (it was reached).
    // - opt-in: the pinned in-place cycle reserves each pin word's window and the objects overlapping it, not whole 4 KiB pages.
    // - opt-in, Generational only: the moving-young coverage verifier's dead-spill excuse is bounded to operand-spill words.
    // - default ON (`=0` kills), Generational only: an unmounted virtual thread claims no carrier OS tid in the blocked-peer capture fold.
    // - opt-in, Generational only: interpreter frames' reference-typed locals (the GenerateOopMap mask) gate the conservative probe and the lost-tag remap.
    // gcj close (2026-10-23): default ON (`=0` kills), was opt-in -- the interpreter type map gates the conservative locals probe and the lost-tag remap.
    // gcj j1pn retirement (2026-10-25), one block, every switch default ON (`=0` kills):
    // - the blocked-peer native-stack write-back refuses words below the wake path's entry (every backend).
    // - Generational: the young pin ledger's read screens words to from-space `[base - slack, top]` (not to-space, not past the top).
    // - Generational: the collection initiator's ledger deposit does not sweep the collector's own frames below the pause door.
    // - the safepoint root publish (`update_root_snapshot`) inserts nothing into the never-read thread-local movable / unrewritable sets.
    // gcf f5/f, opt-in (the f6 default flip was reverted by its battery): an
    // optimizing-tier frame-deopt register block whose RBP image is not the
    // frame's `rbp` was never written by the activation and stops being a
    // root, OSR bodies included (`vm/src/jit/conservative_roots.rs`,
    // `deopt_residue_word_may_be_a_save`).
    // gci i23/rf (opt-in, proposal P1/rf): a registered-chain optimizing-tier `frame_holds_no_references` frame is no band root below `callee_saved_lo`.
    // gcg g6/jm: opt-in (vg6: no effect on Gcd1ArgPinSpecialProbe). A dispatch door does not pin a call's arguments across a callee body that owns them.
    // gch h1/fr: opt-in. The by-name dispatch's bytecode tail releases its argument pins before `interpreter::execute` (which roots its own copy), so a parameter the callee nulls can be collected (d9d P1).
    // gch h2/fj: opt-in. The call-site service pins a taken callee stash's object words across the frame rebuild and restashes a frame refused after a collection instead of dropping it (gcd-d10f restash proposal).
    E { group: Group::GC, token: "interp-type-map-locals", on_key: Some("CRATONVM_GC_INTERP_TYPE_MAP_LOCALS"), off_key: None, off_word: Some("0"), since: "2026-10-13" },
    // gch h2/fj: default ON since JIT r20 GC wave 9 (host w9c, gcd-d9d cost A/B passed; `=0` kills). The interpreter's compiled-callee doors hold an owning body's reference arguments as weak JNI global handles across the run, so a parameter the callee nulled can be collected (d9d P3).
    // gch h3/fd: default ON since JIT r20 GC wave 9 (host w9c, gcd-d9d cost A/B passed; `=0` kills). interpreter::execute's first-call door holds an owning body's reference arguments as weak JNI global handles across the compiled run, so a parameter the callee nulled can be collected (d9d P2).
    // gce e2/o: default ON, `=0` restores the gen r4w5 exit. The latched
    // GC-overhead exit tries the allocation once more after its deciding major,
    // at most 8 times per latched episode (`gc_and_alloc::latched_overhead_grace`).
    // gci i2/oo (opt-in): the GC-overhead verdict reads counters sealed inside the pause; two threads' verdicts cannot interleave.
    // gcd d2/j: opt-in — a preallocated `OutOfMemoryError` per VM-raised
    // message (`vm::realms::heap_realm::PreallocatedOome`).
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
    // gcj j1yg-pause: default ON since 2026-10-27, `=0` kills -- the parallel young copy forwards the dirty-card roots on its workers.
    E { group: Group::GC, token: "par-evac-card-seed", on_key: Some("CRATONVM_GC_PAR_EVAC_CARD_SEED"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::GC, token: "gen-pinned-young-copy-parallel", on_key: Some("CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    // gcd d3/m: the pinned in-place young copy (which needs
    // `gen-pinned-young-copy` too) also takes a young cycle whose only divert
    // is the take-over licence: every unrewritable peer a helper window the
    // pass pinned whole, each window's band read raw into the young pin
    // ledger. Opt-in (default OFF).
    // gcj j1yg-close: default ON since 2026-10-27, `=0` kills -- the pinned copy takes a helper-window take-over pause.
    E { group: Group::GC, token: "system-gc-moving-young", on_key: Some("CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG"), off_key: None, off_word: None, since: "2026-09-23" },
    // gci i47w/sg47: default ON since the WARNING flip `a63f13062d` (user decision), `=0` kills. The System.gc() door's collection tenures every movable live young object (Generational in-place sweep or moving copy, G1 evacuation; since gci i48/zl3 also ZGC's generational model heap, `CRATONVM_ZGC_GENERATIONAL=1`), as HotSpot 25's full collection leaves young empty.
    // gen r4w6/young6 (2026-09-24): opt-in HotSpot-style adaptive tenuring for
    // the generational young collector (presence-parsed, default OFF). See
    // `gc/src/gen_heap_tenuring.rs`.
    E { group: Group::GC, token: "adaptive-tenuring", on_key: Some("CRATONVM_GC_ADAPTIVE_TENURING"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    E { group: Group::GC, token: "par-threads", on_key: Some("CRATONVM_GC_PAR_THREADS"), off_key: None, off_word: None, since: "2026-07-25" },
    E { group: Group::GC, token: "sync-young-wipe", on_key: Some("CRATONVM_GC_SYNC_YOUNG_WIPE"), off_key: None, off_word: None, since: "2026-09-02" },
    // gen r4w3/cards3 (2026-09-23): three default-ON A/B levers for the
    // generational remembered set, each `on_unless_zero`, so
    // `CRATONVM_GC=-token` writes `0`. `card-summary`: the card table's
    // one-byte-per-64-cards summary map. `old-bot`: the old generation's
    // block-offset table behind the dirty-card object walk. `precise-array-
    // cards`: `set_array_element` dirties the written element's card, not the
    // array header's. G1 and ZGC are unaffected by all three.
    // gcd d5/s: opt-IN. On an element-precise table, the young pause's
    // dirty-card scan trusts ELEMENT cards for a reference array whose header
    // card is dirty too: it reads only the dirty ranges the array overlaps,
    // not the whole array (cards3 item 3's consumer). `runtime_flag_on`
    // (`gc::gen_heap::scan_dirty_cards_inner`).
    // gci i2/st (opt-in): the parallel young mark shares work with a parked peer from 8 local entries, not 64.
    // Opt-IN: the interpreter's TLAB fast path plans the COMPACT body shape,
    // the one the JIT's inline `new` and the TLAB-miss path already use. Off by
    // default because the single previous attempt at this unification
    // miscompiled `probes/FjpProbe.java`; see `compact_tlab_alloc_enabled`.
    // Default-ON opt-out since 2026-09-03: `compact_tlab_alloc_enabled` reads
    // `0`. It shipped opt-in the same day and earned the default with a
    // 228-program differential soak per collector, 89/89 on the
    // HotSpot-differential regression suite with the shape enabled, and a real
    // application allocating 87.5 MB less.
    // Bisection levers for the shape above: which sites may plan compact, and
    // which classes actually did. Both exist because the first miscompile it
    // exposed cost a rebuild per hypothesis until they did not.
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
    E { group: Group::GC, token: "gen-uncommit", on_key: Some("CRATONVM_GEN_UNCOMMIT"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    // gce e2/o: `gen-pristine-chunks` (subsumed by zero-once), `tlab-size-retired`
    // and `tlab-waste-shrink` (superseded by the share sizer) are REMOVED
    // (`e1-p-triage.md` table 2B).
    // Opt-in (gen r4w3/alloc3, 2026-09-23), two TLAB arms, each default OFF
    // until its A/B (protocols in
    // `docs/feature-designs/generative-gc-architecture.md`):
    // * `tlab-filler-skip-zero` -- a Java thread's retire writes the tail
    //   filler header but not the memset of its (already zero) data area;
    // * `tlab-gate-bump-floor` -- the JIT refill gate probes the young bump
    //   tail for the fragmentation floor instead of the full request.
    E { group: Group::GC, token: "tlab-filler-skip-zero", on_key: Some("CRATONVM_TLAB_FILLER_SKIP_ZERO"), off_key: None, off_word: None, since: "2026-09-23" },
    E { group: Group::GC, token: "tlab-gate-bump-floor", on_key: Some("CRATONVM_TLAB_GATE_BUMP_FLOOR"), off_key: None, off_word: Some("0"), since: "2026-09-23" },
    // gen r4w4/alloc4 (2026-09-24). Every allocation-policy arm (the rows
    // above and these three) now parses against its default in
    // `flags::alloc_policy_defaults`; a default flip is that constant plus
    // this table's `off_word` (`Some("0")` for a default-ON arm).
    // * `tlab-share-sizer` -- the HotSpot-shaped per-thread TLAB sizer
    //   (share of young allocation per collection, exponentially averaged,
    //   1/50 of it per refill) and the refill-waste limit. gen r5w4/defaults8
    //   made its default PER-BACKEND (`alloc_policy_defaults::TLAB_SHARE_SIZER`);
    //   the Generational flip was reverted at the r5w4 merge; ON again on the
    //   Generational heap since gcg g6/jn and on G1 since gci i48/zl3
    //   (`TLAB_SHARE_SIZER_G1`), OFF on ZGC; `off_word` `"0"` is the kill;
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
    E { group: Group::GC, token: "gen-zero-once", on_key: Some("CRATONVM_GEN_ZERO_ONCE"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // Opt-in (gce e1/y, 2026-09-29): the zero-once skip also covers a
    // free-list hand-out inside a span the last sweep zeroed
    // (`GcFlags::gen_zero_once_free_list`).
    // Opt-in (gcf f7/c, 2026-10-01; gch h4/fl flipped it and reverted it: vh4-fl B failed its cost
    // rule): the young trigger and the tenuring survivor target follow HotSpot Serial's eden
    // (`gen_heap::serial_eden_young_trigger`).
    // gci i1/yg (default ON by the user's flip, WARNING gci-user-flip-20261013; `=0` kills): survivor ages count HotSpot Serial's young collections (one per HotSpot eden of
    // young allocation, `gen_heap::age_clock_advance`); the young trigger is unchanged.
    // gci i3/yg (default ON, `=0` kills; inert without serial-eden-age-clock): the HotSpot age clock also charges
    // old-direct allocations below its eden (`gen_heap::GenerationalHeap::age_clock_charge_old_direct`).
    // gci i22/ed (opt-in): the Serial-shaped young trigger starts at HotSpot Serial's eden for the initial heap and
    // is resized after full collections (`gen_heap::GenerationalHeap::follow_initial_heap_eden`).
    // gci i3/tk (opt-in): a refusing helper window whose stack was read whole leaves Generational promotion on.
    // gci i3/tk (opt-in): a pause whose take-over froze compiled peers keeps promotion when every frozen stack was read whole.
    // Default ON (gc wave 9, lane gcC, 2026-10-12; `gcf-f5c-...`; w9c gate C1/C2/C3 passed), =0 kills: a
    // young survivor ages once per HotSpot Serial eden of young allocation instead of once per young
    // cycle; the trigger is unchanged (`gen_heap::serial_eden_age_epoch`).
    // zs s9/la: on ZGC (with preallocated-oome-immutable on) the first four VM-raised "Java heap space" OutOfMemoryErrors are preallocated at boot and handed out with a fresh stack trace (HotSpot's preallocated traced pool) instead of being constructed on the heap that just refused an allocation; the constructed ones became garbage the ZGC free list reused, which let the L4 launcher probes enter main where HotSpot fails the receiver. Default ON, kill "0".
    // zs s10/sp: after the final refusal of a SMALL request of S bytes (an OutOfMemoryError is built), the ZGC low free list serves no request from a block smaller than T = max(S, floor) until a collection changes the picture by max(64 KiB, capacity/64) (the arena's bumped bytes go down by that much, or what it can serve grows by that much; zt t1/sp); blocks stay listed, only the fit predicate changes (HotSpot's retired-TLAB analogue). Value: the floor -- "1"/on = 2048 (MinTLABSize), a number >= 16 = that floor ("16" = the literal rule: blocks smaller than the refused request). Opt-in (unset/"0" = off).
    // gcg g2/w, default on since gcg g6/rs (vg5 rs5_wst_conc_*): the interned-String pool is a weak
    // root on Generational; =0 restores the strong pool (`vm::memory::string_table`).
    // gci i47w/ws47: after a pause that reclaimed old storage, an OLD weak-pool entry is kept only if its
    // address is still an old-generation object base (a slide can leave it inside a survivor). Default on;
    // `=0` kills.
    // gci i25/st2 (opt-in): the String intern table's single insert funnel (no pool lock across the allocation,
    // insert-if-absent) plus, on Generational under the weak table, a young-entry key list a young pause judges,
    // removes and remaps instead of the whole pool -- steps 1-2 of gcg-g2w-proposal-string-table-keyed-by-the-string.
    // gcj j1/st (opt-in): on Generational under the weak String table, a String a pooled constructor
    // (`vm_object.rs` create_java_string* / try_create_java_string*) pooled since the last pause stays rooted
    // through the next pause (gcg-g4rs blanket mitigation); `vm::memory::string_table::note_pooled_by_constructor`.
    // zr z1/rf: the weak interned-String pool also on a ZGC heap that only collects
    // stop-the-world (no concurrent mark configured, generational off); `vm::memory::string_table`.
    // Default ON since zt t10 (rows I1-I6 SAME both modes, 40/40 soundness runs, CPU A/B; flip a725152c39), kill "0".
    // zt t13/x10 (2026-10-19): once the ZGC arm of the weak interned-String table answered "strong" (the heap was not stop-the-world only), it answers "strong" for the VM's life: an embedder turning a running heap's generational mode off no longer moves the VM to the weak pool, where a literal resolved while strong (no ldc record) could clear. A launcher run never moves. Default ON, kill "0".
    // gcj j1/jn (opt-in, Generational only): a foreign-attached (`AttachCurrentThread`) thread's refused JNI
    // allocation runs the native reclaim ladder and retries once, as HotSpot's does, when the thread is a counted
    // mutator inside its JNIEnv function and holds only indirect locals (`native::jni::foreign_attached_alloc_may_collect`).
    // gcj j1/jn, default ON (`=0` kills), Generational only: a JNI `NewObjectArray` / `New<Type>Array` longer than
    // HotSpot's limit (`Integer.MAX_VALUE - 2`) is refused at once with `Requested array size exceeds VM limit`,
    // without the native ladder's collections (`native::jni::jni_array_refused_for_its_length`).
    // gcj j2/jn2 (cut 1 of the JNI transition ledger; default ON since WARNING t1-user-flip-20261026, `=0` kills): a JNI native dispatch resolves the VM's JNI switches
    // once and passes them down instead of about seven TLS resolutions per call (`native::jni::switches_once_per_call_enabled`).
    // gcj j2/jn2 (opt-in, Generational only, cut 2 of the JNI transition ledger): a JNI call going into native keeps
    // its TLAB; a collecting pause retires it on the thread's behalf (`native::jni::in_native_keeps_tlab`).
    // gcj j2/jn2 (opt-in, Generational only): a JNI `Call<Type>Method*` up-call SELECTS its target against the
    // method ID (JVMS 5.4.6, HotSpot's `jni_invoke_nonstatic`) instead of running the receiver chain's first
    // same-signature declaration (`native::jni::jni_virtual_upcall_selection`).
    // gcj j2/jn2, default ON (`=0` kills), Generational only: JNI `NewString(p, 0)` answers `""` whatever `p` (HotSpot
    // reads no unit), and a negative length leaves `NegativeArraySizeException` pending (`native::jni::jni_new_string`).
    // gci i22/g1rk (opt-in): after a trusted G1 remark + cleanup, the address-keyed side tables are swept on the
    // completed mark (`gc_and_alloc::g1_remark_sweep_address_keyed_mark_exact`).
    // zr z1/rf, default on (=0 kill): a ZGC heap publishes the watched-referent set empty (no
    // ZGC code reads it); `vm::runtime::interpreter::watched_set_unread`.
    // gcg g3/rw (default ON since gcg g5/rs, `=0` kills; common-w16c): close the fds the weak-row sweep parked at
    // the post-collection Cleaner drain (`gc_and_alloc::run_cleaner_actions`).
    // gcg g3/x, default ON since g6/yg (=0 kill), launcher only: single-dash `-verbose:gc` becomes
    // `-Xlog:gc`, HotSpot's meaning (`vm-cli/src/main.rs`,
    // `normalize_java_launcher_argv`); `--verbose:gc` is unchanged.
    // gcg g4/yg, default ON since g5/yg (=0 kill): `-Xlog:gc+phases` prints Serial's four `Phase 1..4`
    // pairs on a Generational full collection (`gc_events.rs`; Phase 1 carries
    // the pause, 2-4 print 0.000ms).
    // gci i27/ob (gcg-g3x item 11), default ON (`=0` kills): the launcher refuses an `-Xlog` option HotSpot 25 refuses (unknown level or tag) with HotSpot's lines and exit 1.
    // gci i45/sl, default ON (`=0` kills): `-Xlog:safepoint` prints HotSpot 25's `Safepoint "<op>", Time since last ... Total` info line per pause.
    // gci i46/lg, default ON (`=0` kills): `-Xlog` file outputs follow HotSpot 25's `LogFileOutput`: an existing file is archived to `<name>.<n>` at VM start (truncated under `filecount=0`) and rotated at `filesize`.
    // gci i46/lg, default ON (`=0` kills): a frame-trace pause's `-Xlog:safepoint` operation is its initiator's scope's (a single thread's `getStackTrace` is a handshake: no line, no count; `jcmd Thread.print` is `PrintThreads`).
    // gci i46/lg, default ON (`=0` kills): a G1 young pause whose report waits for its mark-cycle drive prints its `safepoint` line after that report's `gc` lines.
    // gci i46/hb, default ON (`=0` kills): a reclaim-guard reclamation-RING hit with no free-list verdict and no corrupt-proof caller prints at WARN, not ERROR (a legitimate failing cast of a re-served address printed "RECLAIMED while it was still referenced").
    // gci cs48 (gci-i19ob CS1, gci-i15rb vi28-dg E4), default ON (`=0` kills): a G1 cycle
    // opens only after a young-only pause, never right after a `Mixed` one; a user-requested
    // concurrent cycle (System.gc() / GC.run under -XX:+ExplicitGCInvokesConcurrent) takes its
    // door's young pause as the forced `Concurrent Start` and runs no compaction pause.
    // gci nd48 (gci-i48-g1-native-door-oome-after-ladder), default ON (`=0` kills): a small
    // fallible G1 allocation at the TLAB reserve may claim a Free region a collection gave back
    // since the reserve last refused one (half a region of bytes per claim, never the last one).
    // gch h1/ob (default ON since gch h2, vh1-ob `ob1_split_*` 8/8 SAME; `=0` restores the one
    // `Pause Full`): an escalated young pause (an unrequested major after a completed young
    // half) is reported as HotSpot Serial's `Pause Young` + `Pause Full` with two GC ids, two JFR
    // collections and split Copy / MarkSweepCompact notifications (`gc_events.rs`).
    // gcg g1/o: default ON, `=0` restores the lazy first use. `ensure_singleton_oom`
    // prepares `StackTraceElement` and creates the `StackTraceElement[]` /
    // `Throwable[]` mirrors at boot (`exceptions::warm_oome_path_classes`).
    // gcg g1b/o: default ON, `=0` restores the error. A class preparation refused
    // for a ConstantValue String collects once and retries on the init path
    // (`vm_util::prep_refusal_may_reclaim`).
    // gcg g3/o: default ON, `=0` restores the infallible G1 arm. A class mirror a
    // native asks for on a G1 heap with no Free region left raises the catchable
    // OutOfMemoryError instead of aborting (`vm_object::native_mirror_refusal`).
    // gci i22/oo, default ON (`=0` kills): `Object.getClass()` whose mirror must be minted on a full Generational
    // or G1 heap declines, collects and retries (`vm_exec::NativeContextImpl::try_get_class_mirror`).
    // gci i31/om: default ON, =0 kills.
    // gcg g3/o, default ON since gcg g6, `=0` kills: a class's mirror is created on
    // its initialization path,
    // where a refusal may collect (`vm_util::create_mirror_on_init_path`).
    // gcg g3/o; default ON since gch h3/os (`=0` kills, vh2-ob D: majors 34 -> 18 in 3/3 pairs): the native factory door tries after its young collection
    // (`gc_and_alloc::native_reclaim_young_stage`).
    // gcg g3/o, default ON since gcg g6, `=0` kills: the JIT helpers' latched
    // GC-overhead exit decides through
    // the interpreter's `overhead_limit_exit` (`gc_and_alloc::jit_overhead_limit_throws_unified`).
    E { group: Group::GC, token: "gen-humongous-zero-unlocked", on_key: Some("CRATONVM_GEN_HUMONGOUS_ZERO_UNLOCKED"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // gen r4w6/tlab6 (2026-09-24), opt-in, an allocation-policy arm (default in
    // `flags::alloc_policy_defaults::GEN_TLAB_TAIL_SINK`): a retiring TLAB's
    // unused tail goes back to the generational young from-space (cursor
    // retraction or the young free list) instead of under an `int[]` filler.
    E { group: Group::GC, token: "gen-tlab-tail-sink", on_key: Some("CRATONVM_GEN_TLAB_TAIL_SINK"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // gen r5w1/oldgen5 (2026-09-26), default ON since gcg g5 (=0 off), an allocation-policy arm (default
    // in `flags::alloc_policy_defaults::GEN_HUMONGOUS_REF_ZERO_UNLOCKED`): a
    // humongous REFERENCE array's body is zeroed after the old-generation lock
    // drops, behind a same-footprint `long[]` header until it is zero.
    E { group: Group::GC, token: "gen-humongous-ref-zero-unlocked", on_key: Some("CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    // gen r4w5/pinned5 (2026-09-24): opt-in, presence-parsed, default OFF. A
    // young cycle the conservative-JIT-root term would divert copies instead,
    // leaving the pages its unrewritable conservative words land in in place.
    E { group: Group::GC, token: "gen-pinned-young-copy", on_key: Some("CRATONVM_GEN_PINNED_YOUNG_COPY"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    // gcj-j1pn-pinned-cycle-review-findings item 1 (opt-in; default ON failed WeakClearYoungProbe 4/4): a
    // layout-free-band / register / peer-band pin word at a watched object's
    // start counts as rooted under the start-word rule above.
    // gci i38/ds: opt-in (flip rule vi38-ds VERDICT).
    // WARNING gci-user-flip-20261016: default ON (vi35-fz S twins 6/6 equal on i35a), =0 kills.
    // gci i40/gn: default ON (correctness: the pinned in-place cycle records its resurrection drain), =0 kills.
    // Default-ON opt-out: the young-GC trigger predicate reads `used`,
    // `free_list_bytes` and `capacity` from a triple republished by the
    // `young_from` mutex guard's `Drop`, instead of taking that mutex on the
    // ALLOCATION path. `=0` takes the lock, so the two are one binary apart.
    E { group: Group::GC, token: "promotion-guard", on_key: None, off_key: Some("CRATONVM_NO_GC_PROMOTION_GUARD"), off_word: None, since: "2026-06-21" },
    E { group: Group::GC, token: "promotion-oom-guard-broad", on_key: Some("CRATONVM_PROMOTION_OOM_GUARD_BROAD"), off_key: None, off_word: None, since: "2026-06-23" },
    E { group: Group::GC, token: "selective-promote", on_key: None, off_key: Some("CRATONVM_NO_SELECTIVE_PROMOTE"), off_word: None, since: "2026-06-05" },
    E { group: Group::GC, token: "stress", on_key: Some("CRATONVM_GC_STRESS"), off_key: None, off_word: None, since: "2026-06-18" },
    E { group: Group::GC, token: "sweep-anchor-stride", on_key: Some("CRATONVM_GC_SWEEP_ANCHOR_STRIDE"), off_key: None, off_word: None, since: "2026-07-25" },
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
    // zr z3/mt (2026-10-12): DEFAULT-ON again -- unset now means `auto`
    // (`Z_PARMARK_DEFAULT_AUTO`, the z2b host rows), so the row gets its
    // `off_word` back: `CRATONVM_GC=-zgc-parmark` must expand to "0" (serial),
    // not to unsetting the key (which is `auto`).
    // zr z2/r2 (relocation backlog), all default ON with `=0` the kill switch.
    E { group: Group::GC, token: "zgc-relocations-prune-amortized", on_key: Some("CRATONVM_ZGC_RELOCATIONS_PRUNE_AMORTIZED"), off_key: None, off_word: Some("0"), since: "2026-10-12" },
    E { group: Group::GC, token: "zgc-forward-chain-by-epoch", on_key: Some("CRATONVM_ZGC_FORWARD_CHAIN_BY_EPOCH"), off_key: None, off_word: Some("0"), since: "2026-10-12" },
    // Declared 2026-08-21 with the per-cycle relocation proof. Default-ON,
    // so a KILL SWITCH, with the same `off_word: Some("0")` correction as
    // its neighbours: ZGC may compact while a compiled frame is live
    // whenever the collection PROVED that frame's oops rewritable, and `=0`
    // restores the older refusal that fired on the mere existence of a
    // compiled frame -- which is why the default configuration never
    // defragmented. See `gc/src/zgc.rs::zgc_relocate_under_proven_jit`.
    // Declared 2026-08-29 with the LARGE-OBJECT end's compactor. Default-ON,
    // so a KILL SWITCH with the same `off_word: Some("0")` as its neighbours:
    // `=0` leaves the arena's high end exactly as it was before, which is the
    // same-binary A/B for a repair whose whole claim is that the requests
    // failing at 97 % free came from an end nothing could relocate. See
    // `gc/src/zgc.rs::zgc_high_compaction_enabled`.
    // Declared 2026-08-29 with the starved TLAB-refill floor. Default-ON, so a
    // KILL SWITCH: `=0` restores the unconditional `want / 8` floor, which is
    // what let a starved bump spend the large-object reserve on TLAB churn.
    // See `gc/src/zgc.rs::starved_recycle_enabled`.
    // Declared 2026-08-29 with the vacated-span publication. Default-ON, so a
    // KILL SWITCH: `=0` restores reclaim-is-the-cursor-drop-and-nothing-else,
    // which lost every byte a slide emptied below a cursor it could not move.
    // See `gc/src/arena.rs::compact_low_to`.
    // Declared 2026-08-23 with the cross-thread JIT coverage handshake.
    // Default-ON, so a KILL SWITCH, with the same `off_word: Some("0")` as its
    // neighbours: `=0` restores the blanket "any peer inside compiled code
    // makes this cycle unprovable" refusal, which is what left a many-threaded
    // workload with no defragmentation at all. See
    // `vm/src/jit/conservative_roots.rs::xt_jit_coverage_handshake_enabled`.
    // A measurement instrument, default-OFF and read with `is_some()`, so any
    // value turns it on and there is no off word to spell.
    // Declared 2026-08-23 with the OSR coverage-question correction.
    // Default-ON, so a KILL SWITCH: `=0` makes the OSR fallback read
    // `fully_oop_covered` (the frame-slot subset) again instead of
    // `fully_shadow_covered`, which is what refused relocation on 725 of 759
    // collections. See
    // `vm/src/jit/conservative_roots.rs::osr_coverage_uses_shadow_aggregate`.
    // Declared 2026-08-19 with the ZGC read-bounds publish. An OPT-OUT, not an
    // opt-in: ZGC publishing its arena envelope into `JIT_READ_BOUNDS` is the
    // default, and this key is the kill switch that restores helper-only
    // reference reads. See `gc/src/zgc.rs::zgc_jit_read_bounds_enabled`.
    // Default-ON, same presence-means-revert shape as the bounds row above:
    // `helpers::zgc_jit_load_barrier_suppressed` treats the key's PRESENCE as
    // "stop panicking on a colored word that reached a JIT read helper; count
    // it and degrade it to null instead". Degrading re-arms the silent heap
    // corruption the tripwire exists to catch, so this is a barrier bring-up
    // lever -- it buys a census from a run that would otherwise die on the
    // first colored word -- and never a production setting.
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
    // Default ON: a ZGC concurrent mark hides the VM's active Weak / Phantom referent
    // slots (G1's initial-mark skip set), so a cycle ending in a concurrent mark clears them.
    // zr z2/cc: with zgc-conc-ref-skip on, the concurrent phase also hides the active SOFT rows and
    // the mark end re-roots the referents the LRU policy kept (the pre-collection pass has nulled
    // the condemned slots by then). Default ON, kill "0" (weak / phantom only).
    // Phase G. `zgc-generational` is a boolean with a real `off` word; the other
    // two are numeric tunables with no negative spelling, like `zgc-parmark`.
    // zr z1/gn (2026-10-12): generational-mode young-cycle cost and soundness.
    // `old-bitmap` is opt-in (it changes which objects a young cycle reclaims);
    // `young-aged-sweep`, `young-relocate`, `slide-old-live`, `remset-summary`
    // and `nursery-census` are default-on with a `0` kill word.
    E { group: Group::GC, token: "zgc-gen-old-bitmap", on_key: Some("CRATONVM_ZGC_GEN_OLD_BITMAP"), off_key: None, off_word: Some("0"), since: "2026-10-12" },
    // zr z3/rv3: with the generational old-start bitmap on but not usable (invalidated, or side mark bits off), a would-be young cycle runs whole-heap instead of the header-walk + floor-bounded fallback (the arm that corrupts weak referents). Default ON, kill "0".
    E { group: Group::GC, token: "zgc-gen-bitmap-fallback-major", on_key: Some("CRATONVM_ZGC_GEN_BITMAP_FALLBACK_MAJOR"), off_key: None, off_word: Some("0"), since: "2026-10-12" },
    E { group: Group::GC, token: "zgc-gen-old-bitmap-verify", on_key: Some("CRATONVM_ZGC_GEN_OLD_BITMAP_VERIFY"), off_key: None, off_word: Some("0"), since: "2026-10-12" },
    // zr z4/rv4: a FAILED parallel stop-the-world mark seeds its serial fallback with the out-edges of every marked base (a claimed-but-unscanned object, roots first, was skipped by every serial arm and its children swept). Default ON, kill "0".
    E { group: Group::GC, token: "zgc-parmark-fallback-rescan", on_key: Some("CRATONVM_ZGC_PARMARK_FALLBACK_RESCAN"), off_key: None, off_word: Some("0"), since: "2026-10-13" },
    E { group: Group::GC, token: "zgc-gen-young-aged-sweep", on_key: Some("CRATONVM_ZGC_GEN_YOUNG_AGED_SWEEP"), off_key: None, off_word: Some("0"), since: "2026-10-12" },
    E { group: Group::GC, token: "zgc-gen-young-relocate", on_key: Some("CRATONVM_ZGC_GEN_YOUNG_RELOCATE"), off_key: None, off_word: Some("0"), since: "2026-10-12" },
    // t1 wave 6 lane Y4 (zt3gn S2.4 slice 1): a young cycle runs a NURSERY-SCOPED slide -- only young objects on pages at or above the young floor that hold no old object move, and the rewrite visits only the scope, the young survivors elsewhere and the carded objects (O(young live)). Default OFF, opt-in "1".
    E { group: Group::GC, token: "zgc-gen-young-evacuate", on_key: Some("CRATONVM_ZGC_GEN_YOUNG_EVACUATE"), off_key: None, off_word: None, since: "2026-10-26" },
    // t1 wave 8 lane ZT3GN (zt3gn S2.4 slice 2 step 1): a nursery-scoped young slide selects every admitted page (no occupancy cutoff, no budget); pins still withhold pages. Default ON inside the opt-in CRATONVM_ZGC_GEN_YOUNG_EVACUATE, kill "0".
    // t1 wave 8 lane ZT3GN: a nursery-scoped young slide does not advance the young floor (a pinned top page keeps the cursor up; its vacated holes stay above the floor). Default ON inside the opt-in CRATONVM_ZGC_GEN_YOUNG_EVACUATE, kill "0".
    // t1 wave 10 lane ZT3GN: a nursery-scoped young slide's pin screen skips pin words below the scope floor and resolves interior words against the scope's live list (binary search) instead of the registry's backward bitmap walk. Default ON inside the opt-in CRATONVM_ZGC_GEN_YOUNG_EVACUATE, kill "0".
    // t1 wave 10 lane ZT3GN: a nursery-scoped young slide installs its cursor and vacated spans (merged) by editing only the free blocks reaching the scope floor (Arena::compact_nursery_to) instead of rebuilding the whole low free list. Default ON inside the opt-in CRATONVM_ZGC_GEN_YOUNG_EVACUATE, kill "0".
    // t1 wave 10 lane ZT3GN: with the local install, a nursery-scoped young slide free-lists its vacated spans without the memset (the hand-out zeroes them), where the sweep's dead-header skip is on. Default ON inside the opt-in CRATONVM_ZGC_GEN_YOUNG_EVACUATE, kill "0".
    // t1 wave 8 lane ZT3GN: the young complement sweep frees dead young objects below the floor as complement windows (no header read per dead object). Default OFF, opt-in "1".
    E { group: Group::GC, token: "zgc-gen-slide-old-live", on_key: Some("CRATONVM_ZGC_GEN_SLIDE_OLD_LIVE"), off_key: None, off_word: Some("0"), since: "2026-10-12" },
    E { group: Group::GC, token: "zgc-nursery-census", on_key: Some("CRATONVM_ZGC_NURSERY_CENSUS"), off_key: None, off_word: Some("0"), since: "2026-10-12" },
    E { group: Group::GC, token: "zgc-nursery-census-age", on_key: Some("CRATONVM_ZGC_NURSERY_CENSUS_AGE"), off_key: None, off_word: Some("0"), since: "2026-10-12" },
    // zr z2/g2 (2026-10-12): generational defaults. `old-bitmap` is now
    // default-on inside generational mode; `young-relocate` and
    // `nursery-census` are now default-off (`1` restores). `inline-announce`
    // is default-on with a `0` kill word; `young-complement` is default-on
    // since zt t14/gn5 (t12 S2.1 rows: CPU median 0.965x, check SAME; kill "0");
    // `major-promoted-percent` is a numeric tunable (0 = the fixed ceiling).
    E { group: Group::GC, token: "zgc-gen-inline-announce", on_key: Some("CRATONVM_ZGC_GEN_INLINE_ANNOUNCE"), off_key: None, off_word: Some("0"), since: "2026-10-12" },
    E { group: Group::GC, token: "zgc-gen-element-cards", on_key: Some("CRATONVM_ZGC_GEN_ELEMENT_CARDS"), off_key: None, off_word: None, since: "2026-10-12" },
    E { group: Group::GC, token: "zgc-gen-young-complement", on_key: Some("CRATONVM_ZGC_GEN_YOUNG_COMPLEMENT"), off_key: None, off_word: Some("0"), since: "2026-10-12" },
    // zt t3/gn (2026-10-19): on a generational ZGC heap a full request (System.gc(), jcmd GC.run) runs a whole-heap (major) cycle, as HotSpot 25's generational ZGC does; it ran a young cycle that kept dead old objects and their weak references. Default ON inside the generational mode, kill "0".
    E { group: Group::GC, token: "zgc-gen-full-request-major", on_key: Some("CRATONVM_ZGC_GEN_FULL_REQUEST_MAJOR"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zt t7/cc (2026-10-19): on a generational ZGC heap a collection predicted young treats references as strong, as HotSpot 25's minor does: the pre-collection reference pass nulls and condemns nothing (young referents survive, nothing is cleared or enqueued) and a dead young finalizable object is kept for the next major instead of being resurrected for finalize(). Default ON since zt t8 (HotSpot 25.0.3 rows), kill "0".
    E { group: Group::GC, token: "zgc-gen-young-skip-references", on_key: Some("CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zt t9/gn (2026-10-19): the young-cycle prediction behind zgc-gen-young-skip-references evaluates the opt-in major rule (CRATONVM_ZGC_GEN_MAJOR_PROMOTED_PERCENT; the headroom rule was retired in zt t14/gn5) instead of predicting a major whenever it is set, so its young cycles keep young referents too. Changes nothing unless that rule is set. Default ON inside the generational mode, kill "0".
    E { group: Group::GC, token: "zgc-gen-young-predict-exact", on_key: Some("CRATONVM_ZGC_GEN_YOUNG_PREDICT_EXACT"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zt t11/gn3 (2026-10-19): on a generational ZGC heap with an old generation, a collection whose HotSpot cause is Warmup or Proactive (HotSpot 25's major-only director rules) runs whole-heap; it ran young and printed "Minor Collection (Warmup)". Default ON inside the generational mode, kill "0".
    E { group: Group::GC, token: "zgc-gen-major-rule-causes", on_key: Some("CRATONVM_ZGC_GEN_MAJOR_RULE_CAUSES"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zt t6/tc (2026-10-19): generational ZGC young-aged sweep sharded over n threads (GN-17). Numeric tunable, default 1 = serial (opt-in); "0" or "1" restore the serial walk. Also shards the young remembered-set scan (a big reference array by element range), 2026-11-07.
    // zt t6/tc (2026-10-19): on a generational ZGC heap whose young cycles take the floor-bounded sweep (old-start bitmap or young-aged sweep off), a whole-heap cycle leaves its OLD large-object survivors out of the old live figure too, because those young cycles sweep and count them (GN-18). Default ON inside the generational mode, kill "0".
    E { group: Group::GC, token: "zgc-gen-floor-arm-high-old-live", on_key: Some("CRATONVM_ZGC_GEN_FLOOR_ARM_HIGH_OLD_LIVE"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zt t6/tc (2026-10-19): on a generational ZGC heap a System.gc() major (the door's tenure request, CRATONVM_GC_SYSTEM_GC_TENURES_ALL) ages every survivor straight to the promotion age, as HotSpot's full collection tenures young data (GN-5). Default ON inside the generational mode, kill "0".
    E { group: Group::GC, token: "zgc-gen-system-gc-tenures", on_key: Some("CRATONVM_ZGC_GEN_SYSTEM_GC_TENURES"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zt t6/tc (2026-10-19): on a generational ZGC heap the JIT reference-store plan's young floor is the promotion age (compiled stores into any young receiver skip the barrier helper), not age 0 (GN-7). Default ON inside the generational mode, kill "0".
    E { group: Group::GC, token: "zgc-gen-jit-promotion-floor", on_key: Some("CRATONVM_ZGC_GEN_JIT_PROMOTION_FLOOR"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zt t6/tc (2026-10-19): on a generational ZGC heap with an old generation the JMX young pool keeps the young survivors of the last collection (live figure minus the old generation), as HotSpot's ZGC Young Generation pool does after a minor (GN-11). Default ON inside the generational mode, kill "0".
    E { group: Group::GC, token: "zgc-gen-young-pool-live", on_key: Some("CRATONVM_ZGC_GEN_YOUNG_POOL_LIVE"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zt t6/tc (2026-10-19): on ZGC the JMX Flag surface lists SoftMaxHeapSize as a writeable Long and setVMOption("SoftMaxHeapSize", ..) applies it to the heap at run time, as on HotSpot (zt1tp item 2). Default ON since the zt t7 orchestrator flip (row SM1 SAME as HotSpot 25.0.3 in both modes), kill "0"; it was opt-in ("1") pending the origin / error-message rows.
    // zt t6/tc (2026-10-19): a ZGC collection run for a System.gc() / jcmd GC.run request is not a warm-up step even when a poll raised the warm-up bit, as HotSpot counts only Warmup-cause cycles (zt1tp item 7). Default ON, kill "0".
    E { group: Group::GC, token: "zgc-warmup-explicit-door", on_key: Some("CRATONVM_ZGC_WARMUP_EXPLICIT_DOOR"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zt t7/tg (2026-10-19, GN-12): on a generational ZGC heap a young cycle that freed under an eighth of the nursery budget forces the next cycle to a major (the default escalates only on a minor that freed nothing). Opt-in ("1"); the weak-minor census on [GC] zgc-gen-cost: (weak_minors, weak_minor_max_run) is the deciding row's input.
    E { group: Group::GC, token: "zgc-gen-escalate-weak-minor", on_key: Some("CRATONVM_ZGC_GEN_ESCALATE_WEAK_MINOR"), off_key: None, off_word: None, since: "2026-10-19" },
    // zt t14/gn5 (2026-10-20): on a generational ZGC heap a young cycle that freed nothing escalates the next collection to a major only when something was promoted since the last major (or the cycle saw no live nursery, or took the floor-bounded sweep); HotSpot 25's director has no "a minor freed nothing" rule and runs minors while a program fills the heap with live data after a System.gc() (t12 defect 3, Zt12Gn4SoftFree). Default ON inside the generational mode, kill "0".
    E { group: Group::GC, token: "zgc-gen-empty-minor-needs-promotion", on_key: Some("CRATONVM_ZGC_GEN_EMPTY_MINOR_NEEDS_PROMOTION"), off_key: None, off_word: Some("0"), since: "2026-10-20" },
    // zt t20/gn7 (2026-10-20, D7): on a generational ZGC heap the nursery clause waits until the three Warmup cycles have run, as HotSpot 25's director runs its warm-up majors (10/20/30 % of the soft max) before any allocation-rate cycle; the nursery budget fired before the 20 % step and, with nothing old yet, ran whole-heap Major Collection (Allocation Rate) cycles HotSpot does not (Zt16Gn6JmxYoungPool). Default ON inside the generational mode, kill "0".
    E { group: Group::GC, token: "zgc-gen-nursery-after-warmup", on_key: Some("CRATONVM_ZGC_GEN_NURSERY_AFTER_WARMUP"), off_key: None, off_word: Some("0"), since: "2026-10-20" },
    // zt t16/gn6 (2026-10-20, item 11): on a generational ZGC heap, after a young cycle on the young-aged sweep, the JMX ZGC Old Generation pool leaves out the young survivors below the floor (they read as ZGC Young Generation, as after HotSpot's minor); a major lowers it. zt t26/rt2: default ON in the mode, kill "0" (rows gn7_JY_1j / _1jc, pl10_JY_1jc; HotSpot's own answer varies with its tenuring).
    E { group: Group::GC, token: "zgc-gen-jmx-young-below-floor", on_key: Some("CRATONVM_ZGC_GEN_JMX_YOUNG_BELOW_FLOOR"), off_key: None, off_word: Some("0"), since: "2026-10-20" },
    // zt t21/pl10 (2026-10-20, item 11's second residual): on a generational ZGC heap, after a whole-heap cycle that is not a System.gc() (Warmup, Proactive, a rule major), the JMX ZGC Young Generation pool keeps the survivors that major left young by age (HotSpot 25's major tenures by age; only its System.gc() pre-clean promotes all); the old pool held them and the young pool read ~0. A young cycle resets it. Default ON inside the generational mode, kill "0".
    E { group: Group::GC, token: "zgc-gen-jmx-major-young", on_key: Some("CRATONVM_ZGC_GEN_JMX_MAJOR_YOUNG"), off_key: None, off_word: Some("0"), since: "2026-10-20" },
    // zt t16/gn6 (2026-10-20, item 12): under zgc-xlog-generation-statistics a ZGC major that began with an allocation failure latched prints the two young blocks of a pre-cleaning major whatever its cause (HotSpot 25 should_preclean_young's is_alloc_stalling_for_old clause). Opt-in ("1") until a probe reaches the state on both VMs.
    // zt t7/x4b (2026-10-19): on ZGC -XX:SoftMaxHeapSize=0 (and a management setVMOption of 0) is a soft maximum of 0, as HotSpot 25.0.3 keeps it ("Soft Max Capacity: 0M"); it was warned about and dropped (launcher) or read as the capacity (heap). Default ON, kill "0".
    // zt t9/tg2 (2026-10-19): a ZGC cycle the director tick's Timer rule asked for (-XX:ZCollectionInterval) clears the warm-up bit a poll may have raised, so it is not a warm-up step; HotSpot counts only Warmup-cause cycles (zt7tg item 3). Default ON, kill "0".
    E { group: Group::GC, token: "zgc-timer-not-warmup", on_key: Some("CRATONVM_ZGC_TIMER_NOT_WARMUP"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zt t9/tg2 (2026-10-19): a ZGC director tick request whose pause was lost withdraws its cause bit only while the bit is its own (it charged it and no collection completed since), so it no longer clears a Proactive bit an allocation poll charged for the next collection (zt8x5 item 8; reachable only with the opt-in idle tick). Default ON, kill "0".
    E { group: Group::GC, token: "zgc-tick-withdraw-own-bit", on_key: Some("CRATONVM_ZGC_TICK_WITHDRAW_OWN_BIT"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zt t10/tg3 (2026-10-19): a ZGC director tick collection reads the director stop (taken before the gc,exit summary) before each attempt and between its request and its pause; it stands down and takes back its own cause bit, so no Timer cycle starts after the stop and no stale Timer / Proactive bit labels a later collection (t10-x7-report R1). Default ON, kill "0".
    E { group: Group::GC, token: "zgc-tick-stop-aborts", on_key: Some("CRATONVM_ZGC_TICK_STOP_ABORTS"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zt t10/tg3 (2026-10-19): -XX:+ZCollectionIntervalOnly is honoured on a ZGC heap: the allocation-polled collection clauses stand down and only the Timer tick and allocation stalls start cycles, as HotSpot 25. Opt-in "1" (default: parsed and ignored).
    E { group: Group::GC, token: "zgc-collection-interval-only", on_key: Some("CRATONVM_ZGC_COLLECTION_INTERVAL_ONLY"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zt t11/tg4 (2026-10-19): the thread a ZGC director tick collection runs on is kept out of the JMM's started / live / peak thread counters, as HotSpot 25's NonJavaThread director (Timer cycles on an idle heap: started-delta 0). Reachable only with -XX:ZCollectionInterval or the opt-in idle tick. Default ON, kill "0".
    // zt t13/x10 (2026-10-19): on ZGC a GC-notification drain point refused by another thread's drain in progress leaves a mark; that drainer, having let go, drains again when the mark is set and anything is queued, so a notification queued after its last pop (a lock-free --compatible door racing the delivery thread's batch, or System.gc()'s forced drain) is not left for a drain point that may never come. G1 and Generational unchanged. Default ON, kill "0".
    // zt t12/tg5 (2026-10-19): a ZGC allocation stall's collection does not restart the director's Timer clock (-XX:ZCollectionInterval), as HotSpot 25 runs a stall's collection as a minor that leaves the old generation's clock alone; with -XX:ZCollectionInterval=1 a stalling churn gets its Timer cycles (row TG3-O4). Default ON, kill "0".
    E { group: Group::GC, token: "zgc-timer-skips-stall-cycles", on_key: Some("CRATONVM_ZGC_TIMER_SKIPS_STALL_CYCLES"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    E { group: Group::GC, token: "zgc-stall-mark-reaches-joined-pause", on_key: Some("CRATONVM_ZGC_STALL_MARK_REACHES_JOINED_PAUSE"), off_key: None, off_word: Some("0"), since: "2026-10-24" },
    // zt t12/tg5 (2026-10-19): the exit natives (System.exit, Runtime.exit, Shutdown.halt0) stop the ZGC director tick and wait, GC-safe on the exiting thread and bounded, for a tick pause already past its last stop check, before the pre-exit hook prints the gc,exit summary, as the launcher's main-thread arms do since t11. Default ON, kill "0".
    // zt t14/x11 (2026-10-20): the launcher reads JDK_JAVA_OPTIONS as the JDK 9+ java launcher does: "NOTE: Picked up JDK_JAVA_OPTIONS: <value>" on stderr, its tokens (white space split, quoted runs kept, @argfiles expanded) inserted after argv[0] before every launcher stage, and libjli's refusals (unmatched quote, -jar / -m / --module / help / version options, a main class) with exit 1; every collector. Read from the process environment before the flag snapshot exists, so only the raw variable kills it. Default ON, kill "0".
    // zt t16/tg7 (2026-10-20): -jar is libjli's launch mode, not an option with a value: the options after it are still options and the jar is the first non-option token (-jar -version prints the banner, -jar -Xmx64m app.jar runs app.jar with -Xmx64m); a later class path or module option replaces it. The token after -jar was taken as the jar. Every collector. Read from the raw environment (main's early argv pass). Default ON, kill "0".
    // zt t16/tg7 (2026-10-20): --enable-native-access <modules> takes its separate value as libjli's IsModuleOption does (--enable-native-access ALL-UNNAMED Main runs Main with the grant); only the = form took a value and the next token became the main class. Every collector. Read from the raw environment (main's early argv pass). Default ON, kill "0".
    // zt t18/tg8 (2026-10-20): -Xmx / -XX:MaxHeapSize=, -Xms and -Xss take HotSpot's size syntax (k/m/g/t, 0x hex; -Xmx1t is 1 TiB, -Xms0 / -Xss0 are not set), and a malformed value joins the ordered first-refusal scan with HotSpot's "Invalid maximum heap size" / "Invalid initial heap size" / "Invalid thread stack size" lines. Every collector. Default ON, kill "0".
    // zt t15/x12 (2026-10-20): on ZGC a lock-free System.gc() waits, after its inline finalizer drain, only while the delivery thread runs finalize() methods it dequeued, not for its whole batch: the batch ends with the GC-notification step, which another thread's collections keep feeding, so every System.gc() waited out the 2-s finalizer timeout under an allocating peer (Zt13X10MissedDrain). Default ON, kill "0".
    // zt t15/x12 (2026-10-20): a forced ZGC GC-notification drain point (System.gc(), the JVMTI and VM-internal full doors) delivers at most the records queued when it began and hands the rest to the delivery thread, so a --compatible System.gc() no longer delivers, unbounded, what an allocating peer keeps queuing. Default ON, kill "0".
    // zt t18/x14 (2026-10-20): on ZGC a collection door (System.gc(), the JVMTI and VM-internal full doors) waits for no finalizer, as HotSpot 25's JVM_GC: a lock-holder's finalizers go to the delivery thread without a wait, and a lock-free caller's inline drain runs at most what was queued when it began and does not wait for the delivery thread's finalizer step (a peer making finalizable garbage kept every System.gc() waiting the 2-s timeout). Runtime.runFinalization() keeps its drain and wait. Default ON, kill "0".
    // zt t20/tg9 (2026-10-20, Defect D2): under launcher-jar-option-scan a -cp / -classpath / --class-path after -jar leaves the jar launch in place, as HotSpot 25 (-jar -Xmx64m -cp dir nosuch.jar: "Error: Unable to access jarfile nosuch.jar"); the launcher read the jar as a main class. Every collector. Read from the raw environment (main's early argv pass). Default ON, kill "0".
    // zt t20/tg9 (2026-10-20, Defect D3): under -XX:+ZCollectionIntervalOnly a ZGC collection at one of the VM's occupancy-classified forced doors (the native-boundary pressure door, the refill gate's wedge breaker) is an allocation stall: cause Allocation Stall, Timer clock untouched; it was labelled Allocation Rate (row tg6_i1_c rule-cause=true, HotSpot false). Inert without the flag. Default ON, kill "0".
    E { group: Group::GC, token: "zgc-interval-only-occupancy-doors-stall", on_key: Some("CRATONVM_ZGC_INTERVAL_ONLY_OCCUPANCY_DOORS_STALL"), off_key: None, off_word: Some("0"), since: "2026-10-20" },
    // zt t21/tg10 (2026-10-20, page zgc-zt12tg5 item 7): an allocation poll whose ZGC director asks for Proactive while the director tick's request already charged the bit marks a co-claim, and the tick's withdrawals (at the director stop, after a lost pause) leave that bit: it is the poll's cause too. It dropped the poll's Proactive. Default ON, kill "0".
    E { group: Group::GC, token: "zgc-tick-withdraw-keeps-poll-proactive", on_key: Some("CRATONVM_ZGC_TICK_WITHDRAW_KEEPS_POLL_PROACTIVE"), off_key: None, off_word: Some("0"), since: "2026-10-20" },
    // zt t21/tg10 (2026-10-20, page zgc-zt12tg5 item 5): with an -Xlog selection the concise usage (no program named) and a help option's text are printed from a booted VM, as the -Xlog version boot prints its banner: HotSpot's JavaMain creates the VM before PrintUsage, so its -Xlog:gc startup lines come first (rows tg7_u3, tg8_h15). Every collector. Default ON, kill "0".
    // zt t21/tg10 (2026-10-20): -verbose:gc (HotSpot's -Xlog:gc) is an -Xlog selection for the version boot (and the usage boot), so java -verbose:gc -version prints [info][gc] Using The Z Garbage Collector before the banner, as HotSpot 25; it kept the no-VM fast path. Every collector. Default ON, kill "0".
    // zt t31/x10 (page zgc-zt13x10-review-residuals item 28f): the bare single-dash -Xmx / -Xms / -Xmn are refused with HotSpot 25's "Invalid maximum heap size: -Xmx" (initial heap size / initial young generation size) and exit 1, as HotSpot parses them (it has no separate-value spelling); the double-dash --Xmx <size> keeps its value. Every collector, both modes. Default ON, "0" = the next argument is the value again.
    // zt t23/hunt4 (2026-10-20, page zgc-zt22hunt3-zgenerational-obsolete-warning-missing): an obsolete HotSpot flag (-XX:+ZGenerational / -XX:-ZGenerational / =value / bare) prints HotSpot 25's "OpenJDK 64-Bit Server VM warning: Ignoring option ZGenerational; support was removed in 24.0" on stderr, in command-line order, before the first refused option (launcher and libcratonvm; it was dropped silently). Every collector, both modes. Default ON, "0" = silent again.
    // zt t23/hunt4 (2026-10-20, page zgc-zt23hunt4-fragmentation-and-young-compaction-limits-ignored): -XX:ZFragmentationLimit / -XX:ZYoungCompactionLimit take HotSpot 25's double syntax in the ordered first-refusal scan ("Improperly specified VM option 'ZFragmentationLimit=abc'", also the bare name); the -XX: catch-all booted on them. Values still ignored. Default ON, "0" = dropped silently again.
    // zt t7/x4b (2026-10-19): with zgc xlog pause phases on, a ZGC collection's gc,heap block is HotSpot 25's per generation: the capacity triple and the "Heap Statistics:" table (Capacity / Free / Used at Mark Start, Mark End, Relocate Start, Relocate End, High, Low), under Y: then O: on a major and y: on a minor. Default ON, kill "0" (t6/ob4's single capacity triple).
    // zt t7/x4b (2026-10-19): the ZGC gc,init Min / Initial / Max / Soft Max Capacity lines print whole MiB ("%zuM") as HotSpot 25.0.3 does (-Xmx1g: "Max Capacity: 1024M"); they printed the exact size ("1G", "100K"). Default ON, kill "0".
    // zt t9/x6: under zgc-xlog-heap-statistics, each ZGC gc,heap generation block carries HotSpot 25.0.3's Young / Old Generation Statistics table, the O: block reads the occupancy after the young half, and a pre-cleaning major (System.gc() and the other should_preclean_young causes) prints two young blocks. Default ON, kill "0".
    // zt t31/x10 (page zgc-zt13x10-review-residuals item 1): the ZGC -Xlog generation tables print the collection's booked edges: the Mark Start column at the occupancy when a concurrent mark started (and the bytes allocated since as the young table's Allocated), the slide's copy volume as Compacted. Stop-the-world collections keep Mark Start = Mark End = Relocate Start (nothing is allocated or freed inside the pause). Default ON, "0" = the pre-pause figures and zero Compacted again.
    // zt t31/x10 (page zgc-zt13x10-review-residuals item 2): a ZGC collection's -Xlog block carries HotSpot 25's per-generation gc,load line (the one-, five- and fifteen-minute load averages and their share of the CPUs) and gc,mmu line (the minimum mutator utilisation over 2..100 ms windows of this VM's pause history, one pause per stop-the-world collection), before each generation's Metaspace line; gc+load / gc+mmu / gc+reloc / gc+task / gc+nmethod / gc+stats are selectable without the "No tag set matches" warning. Default ON, "0" = no load / MMU lines.
    // zt t10/gn2: under zgc-xlog-generation-statistics, a minor of ZGC's generational mode (CRATONVM_ZGC_GENERATIONAL=1) fills its y: Young Generation Statistics table with the young generation only (the old generation and the minor's promotions split out), as HotSpot 25's table counts it. Inert off the mode. Default ON, kill "0".
    // zt t9/x6: a ZGC major prints its -Xlog gc,phases / gc,metaspace / gc,heap lines generation by generation (Y: pauses, a Y: metaspace line and block per young collection, then the O: ones), as HotSpot 25.0.3. Default ON, kill "0".
    // zt t31/zc11: a ZGC collection prints HotSpot 25's framed gc,phases block (Y/O generation frames, every concurrent phase line); =0 the pause lines alone
    // zt t5/gn (2026-10-19): a whole-heap generational ZGC cycle leaves the young survivors of the large-object region out of the old live figure, which the young cycles after it count themselves; they were counted twice (a heap of large arrays read up to twice its live set). Default ON inside the generational mode, kill "0".
    E { group: Group::GC, token: "zgc-gen-high-young-live", on_key: Some("CRATONVM_ZGC_GEN_HIGH_YOUNG_LIVE"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zr z1/tr (2026-10-12). `zgc-rate-trigger`: HotSpot ZGC's director rules
    // (allocation rate, proactive, high usage) decide, and the 75%-of-`-Xmx`
    // live-bytes clause stands down -- default OFF (opt-in, `off_word: None`).
    // `zgc-hotspot-gc-cause`: HotSpot 25 ZGC's GCCause spellings on
    // `-Xlog:gc`, GC notifications and JFR (`Allocation Rate`, `Warmup`,
    // `Allocation Stall`, ...) instead of `Allocation Failure` -- default ON,
    // `0` is the kill switch.
    E { group: Group::GC, token: "zgc-census", on_key: Some("CRATONVM_ZGC_CENSUS"), off_key: None, off_word: None, since: "2026-09-20" },
    E { group: Group::GC, token: "zgc-census-access", on_key: Some("CRATONVM_ZGC_CENSUS_ACCESS"), off_key: None, off_word: None, since: "2026-09-20" },
    // Wave 4, C0 stage 0. `page.rs` builds a SURVEY over the arena's existing
    // reservation -- an allocator over a window it does not own, which indexes
    // and reports but refuses to hand out memory (`ZPageError::SurveyOnly`).
    // It exists so the page-based allocator becomes reachable and measurable
    // before it becomes load-bearing; stage 1 routes the TLAB chunk source
    // through it. Default OFF, hence `off_word: None`.
    // Wave 4. One extra streaming pass over the mark and object-start bitmaps
    // (no header reads, no writes) reporting the WHOLLY-DEAD-PAGE FRACTION on
    // `[GC] zgc-sweep-pages:`. That number is the one proposal B's O(pages)
    // sweep rests on and it has never been measured -- the sweep is the
    // dominant pause component, so whether page-granular reclamation can help
    // is currently an assumption. Default OFF, hence `off_word: None`.
    // Wave 4. Arms the extra-root bloom ACROSS the concurrent phase instead of
    // only across the stop-the-world mark. The filter's `false` is a PROOF that
    // an object owns no extra roots, so arming it while roots can still appear
    // would drop live edges -- which is why one load gates all four halves
    // (arm, harvest, repair, refusal) and you cannot set one without the
    // others. The per-drain revalidation (stage 3c) is deliberately absent: it
    // is a bound on the repair set, not a proof, and wiring it needs a
    // REQUIRED `ZMarkContext` method rather than a defaulted one -- a defaulted
    // one on a delegating wrapper answers "still valid" forever, which here is
    // a dropped live edge. zr z7/cm (2026-10-13): default ON (inert unless a
    // concurrent cycle opens), kill "0" -- disarmed, the concurrent pool paid
    // three global RwLock reads + a shared counter per marked object.
    // zr z7/cm (2026-10-13): the ZGC SATB pre-barrier drops a record whose overwritten value is
    // already marked (claimed by the marker or born black). Inert unless a concurrent cycle arms
    // the barrier. Default ON, kill "0". z7b: default "2" filters at the thread-buffer flush; "1" per
    // store (the z7 arm).
    // zr z8/rv7 (2026-10-13): the concurrent mark-end repair of the extra-root filter judges a
    // pin-table row REMOVED during the phase by this heap's own rows only (owner inside the
    // arena); another VM's removals no longer refuse to certify (no stop-the-world re-mark).
    // Inert unless a concurrent cycle runs with the filter armed. Default ON, kill "0".
    // zgc round zr z1/mk (2026-10-12): the serial stop-the-world marker claims a child when it is
    // pushed (HotSpot's mark-on-push) and scans a reference array longer than the mark chunk in
    // continuations, children first, so its work stack is bounded by the chunk rather than by the
    // array length. Same mark set. Default ON, kill "0" (the pop-time loop).
    // zr z1/mk: a ONE-worker mark pool (`CRATONVM_ZGC_PARMARK=1`, `CRATONVM_ZGC_CONC_WORKERS=1`)
    // keeps its work stack local instead of publishing half of it to its own stripe every 256
    // pushes. Inert at two or more workers. Default ON, kill "0".
    // zr z1/mk: the parallel stop-the-world mark (`CRATONVM_ZGC_PARMARK`) runs its zgc_concurrent
    // driver loop on the collecting thread instead of spawning and joining a driver thread per
    // cycle. Inert when the serial marker runs. Default ON, kill "0".
    // zr z1/mk: a concurrent ZGC cycle records the native collection-overlay rows mutators write
    // during the phase and re-reads them at mark end (an overlay row is not a heap slot, so the
    // SATB barrier never sees a reference move between two overlays). Inert unless
    // CRATONVM_ZGC_CONC_START opens a cycle. Default ON, kill "0".
    // zr z2/cc: a mark worker whose stack is below both sharing valves gives half of it to a
    // PARKED peer (a deep narrow graph's DFS never reaches either valve, so one worker marked a
    // whole binary tree while the others slept). Inert on one worker. Default ON, kill "0".
    // zr z3/mt: ZGC honours -XX:ParallelGCThreads (every STW pool that is on: parallel mark,
    // compaction passes, parallel sweep), -XX:ConcGCThreads (concurrent mark pool) and
    // -XX:+/-UseDynamicNumberOfGCThreads (concurrent default share), and sizes the concurrent
    // pool by HotSpot's nconcurrent_workers when ConcGCThreads is not given. Default ON, kill "0"
    // (both flags inert on ZGC, concurrent pool cores/4 clamped to 1..4).
    // zr z3/mt (after zr z4/rv4): mark workers parked BETWEEN cycles wait on the condvar with no
    // timeout (the 5 ms poll woke every idle worker ~200 times a second on every ZGC VM once
    // PARMARK=auto became the default). Default ON, kill "0" (the poll).
    // zr z3/mt: STW mark loop per-object costs. REF_DISPS: walk a compact object's oop-map
    // (`ref_disps`) instead of every field. LAYOUT_MEMO: the serial marker keeps the last class's
    // oop-map. TEST_FIRST: an edge whose target is already marked is dropped before the registry
    // screen (one bitmap word instead of two). All three default ON, kill "0"; same mark set.
    E { group: Group::GC, token: "zgc-mark-ref-disps", on_key: Some("CRATONVM_ZGC_MARK_REF_DISPS"), off_key: None, off_word: Some("0"), since: "2026-10-12" },
    E { group: Group::GC, token: "zgc-mark-layout-memo", on_key: Some("CRATONVM_ZGC_MARK_LAYOUT_MEMO"), off_key: None, off_word: Some("0"), since: "2026-10-12" },
    E { group: Group::GC, token: "zgc-mark-test-first", on_key: Some("CRATONVM_ZGC_MARK_TEST_FIRST"), off_key: None, off_word: Some("0"), since: "2026-10-12" },
    // zr zs s1/mc: every ZGC marker (serial, STW and concurrent pools) decodes a header once and sends a shape with no reference slots
    // (primitive array, humongous filler) straight to its pin / overlay edges, skipping the
    // referent-hole lookup and the empty strong walk. Default ON, kill "0"; same edges, same order.
    E { group: Group::GC, token: "zgc-mark-one-decode", on_key: Some("CRATONVM_ZGC_MARK_ONE_DECODE"), off_key: None, off_word: Some("0"), since: "2026-10-14" },
    // zr zs s1/mc: mark workers (STW and concurrent pools) and the serial marker read their thread
    // CPU clock around busy stretches; `cpu_us=` / `worker_cpu_us=` on the mark lines. Telemetry.
    // zs s3/ac: the same switch gates the rest of a collection's CPU accounting (sweep pool,
    // slide-pass pullers, the collecting thread's whole pause): `[GC] zgc-pause-cpu:` per cycle and
    // `[GC] zgc-cpu:` at shutdown. Default ON, kill "0".
    // zs s2/mc2: the mark pool's drain hands each object to the context's `scan_and_claim`
    // (one header decode, a per-worker oop-map memo, the claim inlined into the slot loop) instead
    // of a `dyn` closure calling a `dyn` claim per edge. Same edges, same order. Default ON, kill "0".
    // zs s2/mc2: (opt-in, value = distance 1..16) a mark-pool worker prefetches the entry
    // <distance> below the one it pops, only inside a block one scan pushed (>= 8 children: a
    // reference-array chunk, a wide object). A tree node never opens one.
    // zs s2/mc2: entries (1..8, default 8) in each mark worker's direct-mapped oop-map memo
    // (the direct scan's); "1" is the one-entry memo, the A/B arm for interleaved classes.
    // zs s2/mc2: (opt-in "1") a mark pool's root batch is split one chunk per worker into the
    // stripes the workers own, not over all 4 x W stripes (3/4 of which no worker owns).
    // zs s5/mc3: a reference array longer than one mark chunk is split on its first visit into
    // stealable chunk tasks (tagged queue words, halved like HotSpot's partial-array entries), so
    // one huge Object[] is no longer one mark worker's serial slot loop. Every element walked
    // exactly once. Inert on a one-worker pool. Default ON, kill "0" (the private in-progress slot).
    // zt t1/mk: (opt-in, numeric) the unit of a split reference array's stealable chunk tasks, in
    // elements, apart from the yield chunk (CRATONVM_ZGC_MARK_REF_CHUNK). Unset / 0 = the yield
    // chunk (the behaviour before the key). Read once per mark pool.
    // zr zs s1/mc: (opt-in "1") the ZGC SATB barrier's per-store registry screen moves to the
    // thread-buffer flush, beside the mark-bit filter (filter mode 2 + thread buffers only).
    // zr z2/cc: the concurrent mark's SATB pre-write barrier records into a per-thread buffer
    // registered with the heap, publishing to the ingress once per 256 stores (step 2.2 of
    // proposal-a-concurrent-mark-by-default). Inert unless a concurrent cycle arms the barrier.
    // Default ON, kill "0" (one ingress-bucket push per store).
    // zr z2/cc: the ZGC concurrent-start window ends at the nearest predictable collection point
    // (warm-up clause, allocation budget, threshold), keeping the trigger's proportion of the way
    // there; warm-up / budget collections used to reset `allocated` before a cycle could open.
    // Inert unless CRATONVM_ZGC_CONC_START is set. Default ON, kill "0" (threshold-only window).
    // zr z3/cs: (opt-in) every non-urgent ZGC trigger opens a concurrent mark when the heap can
    // finish one, and the collection follows the mark's fixed point (HotSpot ZGC's model);
    // configures concurrent marking by itself. (default ON, kill "0")
    // `-XX:MaxGCPauseMillis` no longer sizes ZGC's allocation budget (HotSpot's ZGC ignores it).
    // (zt t12/rt deleted the opt-in stop-the-world rate reserve, zgc-rate-stw-reserve.)
    // t1 lane zgc: the every-trigger seed opens no occupancy window on a generational heap.
    // t1 wave 2 lane Z: an explicit CRATONVM_ZGC_CONC_START opens no occupancy window on a
    // generational heap, so its young cycles stay minors (HotSpot 25). (default ON, kill "0")
    // t1 wave 2 lane Z: (default ON since WARNING t1w4-U-flip-20261026, kill "0") a certified concurrent
    // ZGC cycle parks its mark pool on the heap and the next mark start reuses it instead of spawning
    // ConcGCThreads threads.
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
    // Wave 5. The machine-readable half of `zgc::metrics`. `to_tsv_row` and
    // `tsv_header` (58 columns) existed from the module's first day and were
    // emitted by nothing, which is why the full-suite ZGC-versus-Generational
    // join `tsv_header`'s doc describes has never been run. `...METRICS_TSV` is
    // the path to append one row per VM to; `...METRICS_RUN` is the `run`
    // column, i.e. the join key. Both take values, so `off_word: None`.
    // Separate from `zgc-metrics`, which only controls the two HUMAN-readable
    // lines: a harness wants the row without 1975 logs' worth of per-cycle
    // stderr, and an operator reading a pause wants the lines without a file.
    // G2e/G2f (2026-08-17, widened to every cycle 2026-08-18). Default-ON kill
    // switches over the sweep's two per-dead-object costs, in the shape `zgc-relocate` established:
    // `0` restores the previous behaviour byte for byte, so the A/B is a re-run
    // and not a rebuild.
    // C5 (2026-08-18). Hand the mark coordinator the heap's own `Arc` instead of
    // a forwarding wrapper -- one fewer indirect call per marked object on the
    // parallel path. Default-on and `0` restores the wrapper, so the A/B is one
    // binary; the whole path is already opt-in behind `zgc-parmark`.
    E { group: Group::GC, token: "zgc-tlab", on_key: Some("CRATONVM_ZGC_TLAB"), off_key: None, off_word: Some("0"), since: "2026-08-07" },
    // zt t18/zb1 (zr1br A2a/A3): arm the ZGC read barrier at heap construction in plain-store mode (every heap-served reference read goes through load_barrier_slot; stores stay plain); "mark" also publishes loaded references to a running concurrent mark. Opt-in (default OFF).
    // zt t22/zb5 (stage 2e): under CRATONVM_ZGC_LOAD_BARRIER=colored a concurrent cycle flips the good color to its mark bit at mark start and back to Remapped at the closing pause; both JIT tiers test a loaded element against the heap's bad-mask cell and call jit_aaload (mark + self-heal) on a bad word. Opt-in (default OFF).
    E { group: Group::GC, token: "zgc-color-flip", on_key: Some("CRATONVM_ZGC_COLOR_FLIP"), off_key: None, off_word: None, since: "2026-10-20" },
    // nio21 wave 9 L3: a frozen peer's published VM-TLAB tail is lowered over the unregistered in-flight bytes below it so the sweep cannot free them. Default ON, kill "0".
    // zr z8/st: the census half of a ZGC start registry without a per-allocation bit inside a live VM-TLAB chunk (header walk at retire and at a published tail, oracle-hit census). Opt-in (default OFF), a diagnostic: every on value walks and counts only (zt t12/rt deleted the `materialize` arm that registered what the walk finds).
    // zr z9/bo: every inline ZGC start-bit store (both tiers, the box prefixes, the inline toString pair) emits the SHORT form: one `short_blocked` table word replaces the blocked/words/base/span/grid tests and a per-heap `granule_bias` replaces the base arithmetic (25 vs 38 instructions, same bit, same refusals). Opt-in (default OFF).
    // zs s3/bt: a new ZGC heap publishes its JIT post gate as the truth it mirrors (has_old_objects = false, so 0) instead of armed; the promoting sweep raises it first. Default ON, kill "0".
    // zr z9/rv8: with CRATONVM_ZGC_LAZY_TLAB_STARTS on, a collection drops the record of every live VM-TLAB buffer the pause did not publish (a retire walk would otherwise parse the swept corpses, whose headers survive since zr z8/zb). Default ON, kill "0".
    // nio21 wave 9 L3: a collection clears the summary bits of blocks the sweep left empty (declines in a cycle that took over a peer). Default ON, kill "0".
    // nio21 wave 9 L3: a VM-TLAB carve notes its chunk's block-summary bits before the JIT's inline start-bit store can write them (the pin probe and nearest_base_in skipped live objects otherwise). Default ON, kill "0".
    // zr z1/al: a VM-TLAB tail retired after a collection is not subtracted from `allocated` (the collection republished it as live bytes, which never held the tail). Default ON, kill "0".
    // jr21 gc: a per-heap worker keeps the committed arena bytes <MiB> (default 64) past the low bump cursor populated (MADV_POPULATE_WRITE, under uncommit_gate), so a fresh heap's first allocating code does not pay a page fault per 4 KiB (un_prefaulted= on [GC] zgc-uncommitter:). Default ON on Linux (off elsewhere unless a value is given), kill "0".
    // zr z1/al: the large-object sweep sizes a dead array over the 256 MiB alloc_size screen from its header after extent checks, instead of retaining it forever (premature OOME). Default ON, kill "0".
    // zr z1/al: the complement sweep leaves dead low-region headers unwritten and counts a word's dead by popcount when no dead list is wanted. Default ON since zr (`ZGC_SWEEP_SKIP_DEAD_HEADERS_DEFAULT_ON`, off while a stale-reference hunt is requested), kill "0".
    // zr z5/sw: the ZGC complement sweep's dead side stops costing per dead object: the monitor prune gets the dead KEYS (key probe) instead of a list of every dead base, and a 32 KiB block whose mark summary bit is clear is freed without reading its mark words (with the dead-header store off). Default ON, kill "0".
    // zr z2/cp: after a hard allocation failure the low slide's selector evacuates any page over 5 % garbage (cutoff 0.95 live, HotSpot's old-page figure) instead of only pages over 75 % garbage, and skips the cost gate; `2` (the default since 2026-10-13) also widens to 0.75 on a tight heap with no failure, `=1` keeps the after-failure arm only. Default ON, kill "0".
    // zr z2/cp: ZGC's infallible alloc_object / alloc_array arms offer the VM's NativeAllocOom unwind (a catchable OutOfMemoryError under a converting catch_unwind, native_oom::unwind_ok) before aborting. Default ON, kill "0".
    // zr z2/cp: the GC-overhead crawl backstop on ZGC (progress window + crawl rule + the latched exit throws after the soft rung). Opt-in since 2026-10-13 (HotSpot ZGC has no overhead limit; GcOverheadProbe went DIFF with it on), hence `off_word: None`.
    // zr z3/fp: release the retained capacity of the ZGC forwarding table and the sweep's dead-address scratch once mostly unused, and keep a granule's uncommit stamp when the -Xms floor refused its release. Default ON, kill "0".
    E { group: Group::GC, token: "zgc-footprint-trim", on_key: Some("CRATONVM_ZGC_FOOTPRINT_TRIM"), off_key: None, off_word: Some("0"), since: "2026-10-12" },
    // zr z3/fp: ZGC uncommits down to HotSpot's MinHeapSize (-Xms / -XX:MinHeapSize, else min(8M, initial)) instead of the startup commit, which the ergonomic initial heap made equal to -Xmx on large hosts. Default ON, kill "0".
    // zr z4/un: a per-heap ZUncommitter thread gives back ZGC memory unused for ZUncommitDelay (down to MinHeapSize) without a collection, printing HotSpot's [gc,heap] "Uncommitter (0) Uncommitted:" line. Readable (reset) give-back only. Default ON, kill "0".
    // zr z4/un: a granule the readable ZGC give-back reset counts as given back (net commitment: the -Xms floor and Runtime.totalMemory()) and is not reset again until reused. Default ON, kill "0".
    E { group: Group::GC, token: "zgc-reset-accounting", on_key: Some("CRATONVM_ZGC_RESET_ACCOUNTING"), off_key: None, off_word: Some("0"), since: "2026-10-12" },
    // zr z4/ab: when ZGC's huge-extent table has overflowed (more than 64 arrays past 256 MiB were live at once), each collection re-seeds the slots from the registry and leaves overflow mode once every survivor fits, instead of only when no huge array survives. Default ON, kill "0".
    // gci i46/ts: (feature gpu-offload) a cycle whose thread-local veto slot is gone vetoes relocation through the process flag instead of not at all (fail-safe). Default ON, kill "0".
    // zr z3/lt: ZGC's end-of-run metrics summary / TSV row is claimed once per HEAP (was once per process, so a second VM's row was never written); the Drop backstop emits only for a heap that collected. Default ON, kill "0".
    // zr z2/a2: on ZGC, `multianewarray` serves every level that fits a TLAB from the executing thread's VM TLAB (the interpreter's `newarray` arm) instead of one `try_alloc_array_full` per sub-array through the heap's own locked cell. Default ON, kill "0".
    // zr z3/ja (every collector since gci i48/zl2; the name is kept): a collection request stamped at its decision (the compiled refill trigger, the JIT helpers' headroom arms) is dropped when a collection completed in between, as HotSpot's VM_GC_Operation::skip_operation does. Default ON, kill "0".
    // zr s1/ca: the opt-in jit_newarray headroom arm's ZGC rate limit (one occupancy collection per 1/32nd of the heap allocated since the last one). Reached only with CRATONVM_ZGC_JIT_NEWARRAY_HEADROOM=1. Default ON, "0" removes the limit.
    // zr z5/cr: a ZGC allocation refusal whose heap was already collected for a refusal, with under 4 KiB allocated since, skips its forced collection and goes straight to the ladder's clear-soft rung (HotSpot's stall that fails after a GC which freed no page). Default ON, kill "0".
    // zr z5/cr: the native factory door's unlatched tail on ZGC skips the clear-soft rung when its forced collection was the only one since the arena refused the request and a single span now serves it (HotSpot's young, try, full, try). Default ON, kill "0".
    // zr z8/nr: that skip is tracked per thread; a caller whose retry still fails pays the skipped clear-soft rung (last-ditch reclaim + second major) and retries once more before raising OutOfMemoryError. Default ON, kill "0".
    // zr z8/nr (every collector since gci i48/zl2; the name is kept): the one-attempt native doors of Unsafe.allocateInstance (constructor method handles, Constructor.newInstance) and Thread.getThreads / dumpThreads take the fallible door and the reclaim ladder before OutOfMemoryError, as HotSpot's do. Default ON, kill "0".
    // zr z11/nd: on ZGC a native whose one-attempt allocation door refused (the NativeAllocOom unwind) and whose body is audited as restartable (native-builtins oom_restartable: reflection listings, Class.getName, Thread.getStackTrace0 / dumpThreads / getThreads, the --compatible primitive formatters) is re-run by the native funnel after each rung of the collecting ladder, before OutOfMemoryError. Default ON, kill "0".
    // zs s1/dg: the ZGC slide's reference-rewrite pass and reference-slot walk size a LIVE array past 1 << 28 elements (legal up to i32::MAX, refused by alloc_size's element cap) from independent evidence (registered base, extent inside the arena, no other base inside it) instead of skipping it as "not walkable" / omitting a reference array's elements. Default ON, kill "0".
    // zs s2/rf: the ZGC per-object sweep (young cycles, the fallback arm when the complement refuses, the spill set) frees a DEAD array past alloc_size's caps in either region after the extent checks (cycle snapshot, live registry, free list, published TLAB tails, true low cursor), instead of retaining it forever. Default ON, kill "0".
    // zs s2/rf: the ZGC over-long live-array screen memoises its answer per (base, length) for one stop-the-world collection (one registry scan per array per pause instead of ~7). Default ON, kill "0".
    // zs s2/nd2: on ZGC (with zgc-native-doors-collect on) a class definer (ClassLoader.defineClass0/1/2 and the --compatible defineClass / Lookup.defineClass / defineHiddenClass / Unsafe.defineClass bodies) mints the new class's mirror through the fallible door and the reclaim ladder (every reference it still uses is pinned) before the one-attempt door raises OutOfMemoryError. Default ON, kill "0".
    // zs s7/nd6: on ZGC (with zgc-define-mirror-collect on) ClassLoader.defineClass1/2 reserve the new class's java.lang.Class mirror object (and intern its name) through the fallible door and the reclaim ladder BEFORE the define, and adopt it after; a spent ladder is the define's OutOfMemoryError with nothing defined (HotSpot creates the mirror inside define_instance_class), so a retry defines the class instead of LinkageError: duplicate class definition. Default ON, kill "0".
    // zs s8/nd7: the definers other than ClassLoader.defineClass1/2 (defineClass0 = Lookup.defineClass / defineHiddenClass; the --compatible ClassLoader.defineClass, JavaLangAccess / defineClassN backend, Unsafe.defineClass / defineAnonymousClass, URLClassLoader local define, Lookup.defineClass / defineHiddenClass[WithClassData] bodies; the service loader's embedded-jar define) reserve the new class's mirror before the define too, re-reading the protection domain across the reservation; a spent ladder is OutOfMemoryError with nothing defined. Off also when zgc-define-mirror-reserved is off. Default ON, kill "0".
    E { group: Group::GC, token: "zgc-define-mirror-reserved-every-definer", on_key: Some("CRATONVM_ZGC_DEFINE_MIRROR_RESERVED_EVERY_DEFINER"), off_key: None, off_word: Some("0"), since: "2026-10-15" },
    // zs s8/nd7: a reserved mirror (zgc-define-mirror-reserved) parks its class name unpooled (or the String already pooled) and interns it at the adoption, after the define succeeded, so a define that fails (format, linkage, a spent ladder) leaves no string-pool entry for its name. Default ON, kill "0" (the name is pooled at the reservation, as zs s7/nd6 did).
    // zs s9/nd8: on ZGC a NAMELESS define (defineClass(null, bytes, ..), Unsafe.defineClass(null, ..)) parks the class file's own this_class name in the reserved mirror, so the adoption allocates no name String after the define (a refusal there left the class defined, and the retry met LinkageError). Default ON, kill "0".
    // zs s9/nd8: on ZGC a HIDDEN define parks its (mangled, unpooled) name in the reserved mirror and the adoption reuses that String when it reads the defined class's name, so nothing is allocated after the define. Needs zgc-define-mirror-name-pooled-at-adoption. Default ON, kill "0".
    // zs s7/nd6: zgc-boot-oome-init-error's "still initializing" also ends at a per-VM boot-finished latch (SharedVm::mark_boot_finished; cratonvm_embed::finish_boot / preallocate_out_of_memory_error), for an embedder that never raises the init level to 4: a later refused start-up object is no longer reported as a VM initialisation failure. Default ON, kill "0" (the level alone).
    // zt t6/nd4: on ZGC the --compatible ClassLoader.defineClass backend (define_class_via_full) grows the loader's `classes` list (ArrayList.add(null) under the list's monitor, size put back) BEFORE the mirror reservation and the define, when the list is full, so the ClassLoader.addClass it runs after the define allocates nothing; a refused growth is the define's OutOfMemoryError with nothing defined (HotSpot calls addClass before the dictionary update). Generational and G1 unchanged. Default ON, kill "0".
    // zt t9/df: on ZGC a define whose ClassLoader.addClass upcall failed after the publish (the class stays defined; HotSpot makes the upcall before the dictionary update, so its define leaves nothing) records the class as an orphan, and a retry of the SAME bytes by the same loader (ClassLoader.defineClass0/1/2) adopts it -- the upcall runs again and the define answers the class -- instead of LinkageError: duplicate class definition. Generational and G1 unchanged. Default ON, kill "0".
    // zt t9/df: on ZGC the ClassLoader.addClass upcall at the definers define-add-class left out: --compatible's Proxy.newProxyInstance / getProxyClass bridges (a freshly defined $ProxyN), --compatible's Lookup.defineClass door from a lookup class of the application or platform loader, and Unsafe.defineClass into the application loader. HotSpot records each in its loader's classes list. Generational and G1 unchanged. Default ON, kill "0".
    // zt t9/df: on ZGC defineClass0 with initialize=true and no class data runs <clinit> after its ClassLoader.addClass upcall, as HotSpot's JVM_LookupDefineClass initializes after define_instance_class; it ran inside the define, before the upcall. A failing initializer raises the VM's own ExceptionInInitializerError. Generational and G1 unchanged. Default ON, kill "0".
    // zt t9/df: on ZGC an OutOfMemoryError refusal of a define's addClass presize or mirror reservation (both run before the parse) yields to the define's ClassFormatError when the class bytes do not parse (the step is skipped); HotSpot parses first. Bytes that parse keep the refusal. Generational and G1 unchanged. Default ON, kill "0".
    // zt t12/df4: on ZGC ClassLoader.findLoadedClass answers null for an orphan of a define whose ClassLoader.addClass upcall failed (zgc-define-add-class-retry), until its retry adopts it: HotSpot makes the upcall before the dictionary update, so the failed define left the name undefined there. Generational and G1 unchanged. Default ON, kill "0".
    // zt t14/df5: on ZGC (with zgc-define-add-class-retry) the adoption of an orphan by a retry of the same bytes through defineClass0/1/2 writes the retry's ProtectionDomain into the mirror before the addClass upcall, as HotSpot's retry is a fresh define with that domain. Generational and G1 unchanged. Default ON, kill "0".
    // zt t31 (define page residual, row df6_a1_orphan): on ZGC the adoption of an orphan (a define whose ClassLoader.addClass upcall failed after the publish) makes its addClass upcall although the failed upcall had claimed the class's one built-in loader record, so the adopted class is listed in the application loader's classes, as HotSpot's retry (the class's only define) lists it. Generational and G1 unchanged. Default ON, kill "0".
    // zt t16/df6: on ZGC a ClassLoader.defineClass0/1/2 into a user-defined loader binds the class's name only after its ClassLoader.addClass upcall returned, as HotSpot's define_instance_class makes the upcall before update_dictionary: no lookup answers the class while the upcall runs, a failed upcall withdraws the name (no orphan; a retry with the same or OTHER bytes defines afresh), and a second define of the name waits (Thread.yield rounds) for the first to settle. Built-in loaders, hidden classes and initializing defines bind at the define as before. Generational and G1 unchanged. Default ON, kill "0".
    // zt t16/df6: on ZGC (with zgc-define-format-error-first) the OutOfMemoryError refusal of a define's addClass presize or mirror reservation yields to the define's ClassFormatError also for bytes that parse structurally but fail one of the class manager's pre-publish ClassFileParser checks (attribute decode, constant pool references and names, class and member modifiers such as ACC_INTERFACE without ACC_ABSTRACT, duplicate interfaces, class attributes), as HotSpot makes them all before allocating; "0" keeps the structural parse alone. Generational and G1 unchanged. Default ON, kill "0".
    // zt t9/cl5: on ZGC the class-unload transaction drops the Class.getName() memo rows (CRATONVM_CLASS_GET_NAME_MEMO: a JNI global root of an Object[2] holding the mirror's name String and the interned binary name) of the classes it removed, and releases their global roots; they were kept for the life of the VM (1 Object[] + 2 Strings per unloaded class, 143 B/define on Zt7ClClassNames). Not Java-visible. Generational and G1 unchanged. Default ON, kill "0".
    E { group: Group::GC, token: "zgc-unload-class-name-memo", on_key: Some("CRATONVM_ZGC_UNLOAD_CLASS_NAME_MEMO"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zs s5/nd5: Class.getModule() of a user class loader's generated proxy class answers the dynamic module that loader's ProxyBuilder defined (the JDK's own Module, recorded by Module.defineModule0), as HotSpot does; a module minted for one is filed per loader instead of in the VM's permanent by-name module table, whose strong root kept every such loader, its classes and their mirrors alive (~6.5 KB per Proxy.getProxyClass with a fresh loader). Every collector, both modes. Default ON, kill "0".
    // zs s5/nd5: on ZGC a user class loader's java.lang.Module recorded by Module.defineModule0 is held by a JNI weak handle and pinned to its loader (metadata_pin, the layer-modules root row) instead of a permanent JNI global root, so it dies with its loader as HotSpot's ModuleEntry does; the strong root kept every such loader (one per Proxy.getProxyClass with a fresh loader) and all its classes alive. Default ON, kill "0".
    // r23 G1 near-full proxy-define page: zs s5/nd5's loader-owned layer modules on G1 and Generational too -- a user class loader's java.lang.Module recorded by Module.defineModule0 is held by a JNI weak handle and pinned to its loader instead of a permanent JNI global root, so a fresh-loader Proxy.getProxyClass no longer keeps the loader, its jdk.proxyN module and its classes alive for good (3.9 KB a define on G1; HotSpot 0). Default ON, kill "0" (the strong root off ZGC).
    // zs s7/nd6: on ZGC the prune of dead user-loader modules (layer_module_pins) drops their package rows BEFORE it frees their JNI weak handles, and layer_module_of_package resolves a handle under the package map's read guard, so a freed (and reused) handle is never resolved as a Module. Default ON, kill "0" (the s5 prune order; the read-guard resolve is unconditional since s7, zs s9/rv6).
    // zt t4/ch: on ZGC a compiled TLAB refill the occupancy gate refused (a collection is due) takes that collection at once through the stamped occupancy door, rate-limited to one per 1/32 of the heap allocated since the last collection, instead of spilling every object to the locked per-object path until the wedge breaker's 16,384 refusals or an allocation stall. zt t12/st: default ON (it removes the allocation-stall collections of a compiled allocating thread, Zr1TrCauseProbe), kill "0"; engagement `gate_collects=` on `[GC] zgc-refusal:`.
    // zr z6/tp: near a full ZGC heap (post-collection live set at or above the 75% threshold) the occupancy clauses of needs_gc wait for max(1 MiB, <percent>% of the free space) of new allocation instead of max(free/4, 64 KiB). Value is the percent, default 75; kill "0" (the legacy floor).
    // zs s6/no: the ZGC exhausted verdict. After an allocation refusal's collection that left under <KiB> free (`on` = 2048, HotSpot's small page), the refusing thread's next ladder attempts are refused until the next collection, so the ladder runs its clear-soft rung and throws OutOfMemoryError instead of collecting once per sliver of free space down to the last object. Opt-in (default OFF); engagement `exhausted_armed=` / `exhausted_refused=` on `[GC] zgc-refusal:`.
    // zs s6/no: the compiled box door (`jit_box_cold_alloc`, Integer/Long.valueOf after a missed TLAB bump) runs its forced collection on ZGC even while the GC-overhead streak is latched, as the interpreter's ladder does; before, a latched streak skipped it and the box fell through to a native allocation that throws without collecting. Default ON, kill "0".
    // zs s9/no2: an allocation-driven ZGC collection (not System.gc()) whose previous cycle left under <permille> of the heap free asks every active soft reference cleared, standing in for HotSpot ZGC's stalled majors near full (which clear all soft references). Value permille (`on` = 60); DEFAULT ON since zt (2026-10-19; local HotSpot rows: 95 % SAME, 90/80 and the regression set SAME, A/B <= 1.01x), kill "0"; engagement `near_full_soft_clears=` on `[GC] zgc-refusal:`.
    // 2026-11-10 (default ON, "0" kills): the concurrent heap's HotSpot-shaped allocation-rate rule reserves the director's poll latency in bytes, not HotSpot's 100 ms director tick, for its reaction time; the cycle-duration term stays. At GB/s allocation rates the 100 ms term alone started a cycle at ~30 percent occupancy (3x HotSpot's cycle count) and cost the concurrent profile 1.5-2.4x CPU on the churn probes.
    // zt t1/tp: with -XX:SoftMaxHeapSize below -Xmx, the ZGC live-bytes trigger also fires at 75% of the soft maximum (HotSpot's director reads the soft max in every rule; the default arm read it in the warm-up clause alone). Default ON since zt t20/fl1, kill "0".
    // zr z7/pc: the ZGC slide reads each low-region survivor's header once (logical_pages) into a size table aligned with the live set; the partition reads sizes from it and the post-slide rewrite / screen serve primitive arrays without a header read. Default ON, kill "0".
    E { group: Group::GC, token: "zgc-relocate-size-table", on_key: Some("CRATONVM_ZGC_RELOCATE_SIZE_TABLE"), off_key: None, off_word: Some("0"), since: "2026-10-13" },
    // zr z7/rs: advise the ZGC arena reservation MADV_NOHUGEPAGE at heap construction (Linux; skipped when CRATONVM_HEAP_THP asks for huge pages), so a THP=always host does not fault 2 MiB per touched granule. Opt-in (default OFF); outcome on `[GC] zgc-footprint-spans:` as arena_no_thp=.
    // zr z7/rs: the launcher (vm-cli, the only crate that installs mimalloc) sets mimalloc's arena_eager_commit default to 0 from a pre-main constructor, so the 1 GiB first arena is neither committed up front nor madvise(MADV_HUGEPAGE)d (host: 233 -> 155 MB Hello maxrss). An explicit MIMALLOC_ARENA_EAGER_COMMIT wins. Environment only (read before main). Opt-in since zr z10/mt (`=1`, or `force`): the 4 KiB first-touch faults cost +20 % CPU on class loading (z10 host), over the 1.05x bar.
    E { group: Group::GC, token: "mimalloc-lazy-arena-commit", on_key: Some("CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT"), off_key: None, off_word: Some("0"), since: "2026-10-13" },
    // zr z10/mt: with the switch (`=1` / `zeroed`, or `all`) the Linux launcher's global allocator maps every alloc_zeroed (`all`: every alloc) of at least CRATONVM_MIMALLOC_SPARSE_BLOCK_MIN_KB (default 1024, floor 64) on its own anonymous mmap advised MADV_NOHUGEPAGE (realloc = mremap, dealloc = munmap), outside mimalloc's eager huge-page arena; it reaches every collector's and non-GC tables, so the wider modes stay opt-in. FP-SPARSE-BOOT-FLIP: the default is now mode `boot` (only the first CRATONVM_MIMALLOC_SPARSE_BLOCK_BOOT_COUNT big zeroed blocks), kill "0". Environment only (latched at the first allocator call). The `[cratonvm] mimalloc-sparse-blocks:` counters (CRATONVM_GC_STATS) run in every mode, off included.
    E { group: Group::GC, token: "mimalloc-sparse-block-mmap", on_key: Some("CRATONVM_MIMALLOC_SPARSE_BLOCK_MMAP"), off_key: None, off_word: Some("0"), since: "2026-10-13" },
    E { group: Group::GC, token: "mimalloc-sparse-block-min-kb", on_key: Some("CRATONVM_MIMALLOC_SPARSE_BLOCK_MIN_KB"), off_key: None, off_word: None, since: "2026-10-13" },
    // zs s9/rs2: CRATONVM_MIMALLOC_SPARSE_BLOCK_MMAP=boot maps only the first CRATONVM_MIMALLOC_SPARSE_BLOCK_BOOT_COUNT (default 64) big zeroed blocks of the process (the boot's long-lived tables) and leaves later, churning ones in mimalloc. Environment only (latched at the first allocator call).
    E { group: Group::GC, token: "mimalloc-sparse-block-boot-count", on_key: Some("CRATONVM_MIMALLOC_SPARSE_BLOCK_BOOT_COUNT"), off_key: None, off_word: None, since: "2026-10-16" },
    // boot-rss levers close (2026-10-26): the Linux launcher installs a default cap (KiB, default 64: only 64 KiB small pages) on the object size mimalloc places in its huge-page arena; a bigger page or block gets its own OS mapping (4 KiB pages, given back on free). `0` / off keeps mimalloc's own cap; an explicit MIMALLOC_ARENA_MAX_OBJECT_SIZE wins. Environment only (read before main).
    E { group: Group::GC, token: "mimalloc-arena-max-object-kb", on_key: Some("CRATONVM_MIMALLOC_ARENA_MAX_OBJECT_KB"), off_key: None, off_word: Some("0"), since: "2026-10-26" },
    // zs s6/rs: at the start of the shutdown reports (both exit arms) the launcher prints the resident set by mapping class from /proc/self/status + /proc/self/smaps on stderr (`[cratonvm] rss-rollup:` / `rss-class:` / `rss-top:`; Linux only, `unsupported=1` elsewhere), the classes of tools/rss-by-mapping.sh. Off unless set (`0`/`off` = off).
    // zs s6/rs: the Linux launcher's allocator wrapper counts the live REQUESTED bytes of every allocation by size bucket (<=64 B, <=512 B, <=8 KiB, <=64 KiB, <1 MiB, >=1 MiB), the live total and its peak (`[cratonvm] alloc-census:`, printed by CRATONVM_DBG_RSS_BY_MAPPING). Every call takes the wrapper's slow path while it is on (a debug census: atomics per call). Environment only, latched at the first allocator call; off unless set.
    // zs s3/fp3: a class store strips the verifier's per-pc type-map rows at publish and keeps each method's verdicts (fast-path proof, veto, max_locals/max_stack). No default-path consumer reads a row: the readers are the CRATONVM_DBG_VERIFY_OOP_MAPS shadow and oracle, and the rows are kept whenever that key is present. Read once per class store. Default ON, kill "0" (every row kept, as before).
    // zr z7/pc: under CRATONVM_ZGC_RELOCATE_SIZE_TABLE, the ZGC bitmap sweep records each low-region survivor's size cell and the slide's page census (logical_pages) takes it instead of reading the header again. Default ON, kill "0".
    E { group: Group::GC, token: "zgc-sweep-size-hints", on_key: Some("CRATONVM_ZGC_SWEEP_SIZE_HINTS"), off_key: None, off_word: Some("0"), since: "2026-10-13" },
    // zr z9/sl: the ZGC low slide's selector keeps a sparse page with survivors in place unless at least FRAG_PCT % (default 25) of its extent is free in holes shorter than HOLE_KIB (default 64 KiB); a selection with no survivor left declines the slide (no whole-live-set rewrite). Never after an allocation failure (the full-compaction fallback). Default ON since zs s1c as the per-cycle gate (unset = "2" / "cycle"); "1" or any other word selects the per-page arm; kill "0" (zgc_selective_relocate_config).
    E { group: Group::GC, token: "zgc-selective-relocate-hole-kib", on_key: Some("CRATONVM_ZGC_SELECTIVE_RELOCATE_HOLE_KIB"), off_key: None, off_word: None, since: "2026-10-13" },
    E { group: Group::GC, token: "zgc-selective-relocate-frag-pct", on_key: Some("CRATONVM_ZGC_SELECTIVE_RELOCATE_FRAG_PCT"), off_key: None, off_word: None, since: "2026-10-13" },
    // zs s1/sl: the low slide's loop sizes each selected survivor from the census size cell instead of re-reading its header. Default ON, kill "0".
    // zs s1/sl: an unsizable survivor on an unselected page is an obstacle up to low_end in the low slide's placement probe (nothing is copied over its unknown body). Default ON, kill "0".
    // zs s1/sl: the selective arm's fragmented-garbage census rides logical_pages' own pass over the live set (no second walk). Default ON, kill "0". Read only with CRATONVM_ZGC_SELECTIVE_RELOCATE on (whose value "2" / "cycle" selects the per-cycle gate).
    // zs s2/sl2: the ZGC low slide's pin screen also drops the page of the OBJECT a pin lies in (an interior pin on page k+1 into an object based on page k no longer lets it move). Default ON, kill "0".
    E { group: Group::GC, token: "zgc-slide-pin-by-base", on_key: Some("CRATONVM_ZGC_SLIDE_PIN_BY_BASE"), off_key: None, off_word: Some("0"), since: "2026-10-14" },
    // zs s2/sl2: the relocation-set telemetry (JFR jdk.ZRelocationSet, targeted_pages_selected, the pressure tier's live bytes) books the set the slide relocates, after the pin screen and the gates. Default ON, kill "0" (the selector's answer).
    E { group: Group::GC, token: "zgc-relocation-set-books-final", on_key: Some("CRATONVM_ZGC_RELOCATION_SET_BOOKS_FINAL"), off_key: None, off_word: Some("0"), since: "2026-10-14" },
    // zs s2/sl2: the cost gate and the selective empty-set decline exempt a targeted cycle only while a targeted page survived the pin screen. Default ON, kill "0".
    E { group: Group::GC, token: "zgc-target-exemption-after-pins", on_key: Some("CRATONVM_ZGC_TARGET_EXEMPTION_AFTER_PINS"), off_key: None, off_word: Some("0"), since: "2026-10-14" },
    // zs s2/sl2: the ZGC low slide EVACUATES the selected pages' survivors into the bump tail (to-space window) and frees each selected page whole, on cycles with tail headroom and no failure / pressure tier; else the slide runs. Opt-in "1".
    // zs s2/sl2: the slide's rewrite sizes a survivor with reference slots from its pre-slide size cell instead of re-reading its header (gives up the walkability screen's independent read). Opt-in "1".
    // zs s3/cr: the evacuating slide withholds the spans it vacated from the free list until the next collection's sweep (stage 2 of the concurrent-relocation plan). Opt-in "1"; inert without CRATONVM_ZGC_SLIDE_EVACUATE=1.
    // zt t1/rl: release_evac_quarantine first walks every registered object's reference slots and counts the words that still name a held span (qdead= on [GC] zgc-relocate:; stage 3's first measurement). Debug, O(registered slots) per release; inert without CRATONVM_ZGC_EVAC_QUARANTINE=1.
    // Boot image: a comma-separated list of addresses whose first allocations the capture arena traces (read by the launcher before the flag snapshot exists).
    E { group: Group::DBG, token: "boot-image-trace-addrs", on_key: Some("CRATONVM_BOOT_IMAGE_TRACE_ADDRS"), off_key: None, off_word: None, since: "2026-11-10" },
    // zt t31/zc11: after a concurrent cycle's closing drain, one [GC] zgc-soft-row-dbg: line per soft row the mark reached whose referent it did not (the swept-soft-referent hunt), and a summary per closing pause. Debug; reads only.
    // zt t32 (2f positive control): under CRATONVM_ZGC_LOAD_BARRIER_MARKING the load barrier's slow path marks nothing, so an object a mutator loaded and then deleted the only path to is lost (the rows' expected failure).
    // zt t33: the R6 plan also walks every survivor's reference slots as the pause did before t33 and counts (and rewrites) what the legacy pass left (crl_walk_missed= plain/color/legacy on [GC] zgc-concurrent-relocate:; must read 0). Debug, O(live slots) per plan.
    // zt t33 (the walk check's positive control): the R6 plan skips its legacy-holder pass, so legacy cells naming planned objects are left to CRATONVM_DBG_ZGC_CRL_WALK_CHECK. Unsound without the check.
    // zt t34: an R6 plan made from the mark's liveness also runs t33's sizing loop and header scan over the whole live set and counts the regions the mark under-estimated and the legacy holders its list missed (crl_liveness_check=plans/under/missed on [GC] zgc-concurrent-relocate:, both must read 0; [GC] zgc-crl-liveness-check: per plan under -Xlog:gc). Debug, O(live) per plan.
    // zt t1/rl: on Windows the ZGC uncommitter thread does not start (its reset is a decommit plus a re-commit beside running mutators); memory goes back at the collection-start give-back only, with the world stopped. Other platforms ignore it. Opt-in "1".
    // zt t1/rl: the ZGC forwarding table's clear ceiling follows the heap (one entry per 256 bytes, floor 256 Ki entries, never above the absolute 4 M). Only lowers the bound. zt t12: default ON, kill "0".
    // zt t8/rl2: the ZGC forwarding table's footprint trim is sized by its demand (the larger of what survived and the most one slide published since the last prune) and taken only after 8 consecutive over-provisioned prunes, walked or skipped; removes the trim-then-regrow oscillation fp_trim_regrows= counts. Opt-in "1".
    // zt t1/rl: ZGC refuses a slide while a thread of its VM holds raw JNI local references (CRATONVM_JNI_INDIRECT_LOCALS=0 or a raw escape), and the JNI dispatch keeps that count for it (gc_quiescence::raw_jni_locals_matter). Skip reason raw-jni-locals-open. Opt-in "1".
    // zs s3/cr: the =2 cycle gate prices the fragmented garbage of the pages the slide actually selected (budget prefix, after the pin screen) instead of every page under the occupancy cutoff. Default ON since zt t20/fl1, kill "0".
    // zs s3/cr: the ZGC relocation passes (census, partition, rewrite/screen) run on a persistent per-heap worker pool instead of per-pass scoped threads. Opt-in "1".
    // zr z4/jf: ZGC's JFR events while a recording runs -- GCPhasePauseLevel1 rows from the collector's laps, the mark-start GCPhasePause and the cycle's GCPhaseConcurrent, ZAllocationStall, ZRelocationSet[Group], ZUncommit, ZOldGarbageCollection (both JFR backends). Default ON, kill "0" (pre-z4 ZGC JFR shape).
    // zr z9/hp: a ZGC collection writes no jdk.GCPhasePauseLevel1 rows (HotSpot 25 ZGC writes none; both JFR backends). Default ON, kill "0" (the collector's laps as level-1 rows, the z4/jf shape).
    // zr z9/hp: `-version` / `--version` with an -Xlog option boots the VM before the banner, so the -Xlog startup lines (Using, gc+init) and the gc+exit summary print as on HotSpot. Default ON, kill "0" (the no-VM banner fast path always).
    // zs s2/ob: a ZGC `... Pauses` GcInfo / GC notification carries all-zero pool usages (HotSpot 25's ZServiceabilityPauseTracer records no usage). Default ON, kill "0" (the collection's real before/after usage, as on the cycle bean).
    E { group: Group::GC, token: "zgc-pause-gcinfo-no-usage", on_key: Some("CRATONVM_ZGC_PAUSE_GCINFO_NO_USAGE"), off_key: None, off_word: Some("0"), since: "2026-10-14" },
    // zs s2/ob: ZGC heap pools' MemoryPoolMXBean.getPeakUsage() is also sampled at both edges of every collection, as HotSpot's ZGC cycle tracer records it. Default ON, kill "0" (query-time peaks only).
    E { group: Group::GC, token: "zgc-pool-peak-at-gc", on_key: Some("CRATONVM_ZGC_POOL_PEAK_AT_GC"), off_key: None, off_word: Some("0"), since: "2026-10-14" },
    // zs s2/ob: `ZGC Young Generation`'s MemoryUsage init is MinHeapSize and `ZGC Old Generation`'s 0, as HotSpot 25's ZServiceability builds them. Default ON, kill "0" (young 0, old the startup commit).
    E { group: Group::GC, token: "zgc-hotspot-pool-init", on_key: Some("CRATONVM_ZGC_HOTSPOT_POOL_INIT"), off_key: None, off_word: Some("0"), since: "2026-10-14" },
    // zs s2/ob: on ZGC the real JFR backend's jdk.GCConfiguration / jdk.GCHeapConfiguration state the command line's figures (MinHeapSize / InitialHeapSize, explicit ParallelGCThreads / ConcGCThreads, UseDynamicNumberOfGCThreads, DisableExplicitGC, ExplicitGCInvokesConcurrent, narrow-oop mode 32-bit) as HotSpot 25 does. Default ON, kill "0" (the generic figures).
    // zs s3/ob2: every ZGC GcInfo carries a `Metaspace` key sampled at the collection's edges (cycle beans: the figure at each edge, pause beans: zeros), as HotSpot 25 does. zt t1/ob: the pool list itself is CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS's (HotSpot's five); only under that switch's kill does this one also list zs s3/ob2's `Metaspace`-only pool (init 0, committed = used) and its `Metaspace Manager`. Default ON, kill "0" (no edge samples: a cycle GcInfo's `Metaspace` entry is the pool's usage at delivery).
    E { group: Group::GC, token: "zgc-jmx-metaspace-pool", on_key: Some("CRATONVM_ZGC_JMX_METASPACE_POOL"), off_key: None, off_word: Some("0"), since: "2026-10-14" },
    // zt t1/ob (the zs/dev merge decision): ZGC lists HotSpot 25's five non-heap MemoryPoolMXBeans (CodeHeap 'non-nmethods' / 'profiled nmethods' / 'non-profiled nmethods', Metaspace, Compressed Class Space) and two plain managers (CodeCacheManager, Metaspace Manager) with gci i15/nh / i33/ms's figures, as every other backend does, so each pool bean and getNonHeapMemoryUsage (their sum) agree; a pause GcInfo carries a zero entry for each of the five and a cycle GcInfo's Compressed Class Space entry comes from the same edge walk as its Metaspace entry. Default ON, kill "0" (zs s3/ob2's Metaspace-only list and Metaspace Manager).
    E { group: Group::GC, token: "zgc-jmx-hotspot-non-heap-pools", on_key: Some("CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zt t3/ob: on ZGC (with CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS) each of the three CodeHeap MemoryPoolMXBeans reports HotSpot 25's init, 2555904 (InitialCodeCacheSize, committed per heap at start), so getNonHeapMemoryUsage().getInit() is HotSpot's 7667712. Default ON, kill "0" (gci i15/nh's 2 MiB on the non-profiled heap, 0 on the other two).
    E { group: Group::GC, token: "zgc-code-heap-init-hotspot", on_key: Some("CRATONVM_ZGC_CODE_HEAP_INIT_HOTSPOT"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zt t6/ob4: on ZGC (with CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS) a cycle GcInfo carries the three CodeHeap pools sampled at the collection's two edges, as HotSpot's gc_begin / gc_end sample every pool; =0: rendered from the usage at delivery (before == after).
    E { group: Group::GC, token: "zgc-gcinfo-code-heap-edges", on_key: Some("CRATONVM_ZGC_GCINFO_CODE_HEAP_EDGES"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // zt t6/ob4 (zs3ac item 4): opt-in; on Windows x86-64 the ZGC GC-thread CPU clock (`mark::thread_cpu_nanos`) is QueryThreadCycleTime scaled by the TSC rate (calibrated once against QueryPerformanceCounter) instead of the quantum-rounded GetThreadTimes. No effect elsewhere.
    // zt t6/ob4 (zs3ac item 2): the VM's side of a ZGC collection pause (safepoint request and rendezvous, the common epilogue) on the collecting thread is booked as the vm_pause share of [GC] zgc-cpu: (vm_pause_ms=, inside collection_ms); =0 kill.
    // zs s6/ob5: a ZGC cycle GcInfo recorded before the `Metaspace` pool was first handed out keeps the class-store census of its edges and gains the measured `Metaspace` key (the class metadata of exactly the classes defined by then) when a GcInfoBuilder first names the pool, instead of the JMM's undefined usage (init = max = -1). HotSpot 25 always reports the figure. Default ON, kill "0" (no key on such a record).
    E { group: Group::GC, token: "zgc-metaspace-first-cycles", on_key: Some("CRATONVM_ZGC_METASPACE_FIRST_CYCLES"), off_key: None, off_word: Some("0"), since: "2026-10-15" },
    // zs s6/ob5: a ZGC `Metaspace` MemoryPoolMXBean usage threshold is also checked at the end of every collection against the collection's edge sample, as HotSpot 25's LowMemoryDetector checks every pool at gc_end. zt t1/ob: since the dev merge gci i16/nt arms and checks every non-heap threshold on every backend (at the set, on the define cadence and at a collection's end when its walk is due), so the kill no longer silences the sensor. Default ON, kill "0" (only gci i16/nt's checks).
    E { group: Group::GC, token: "zgc-metaspace-threshold", on_key: Some("CRATONVM_ZGC_METASPACE_THRESHOLD"), off_key: None, off_word: Some("0"), since: "2026-10-15" },
    // zs s6/ob5: the ZGC `Metaspace` pool's collection-edge samples are its peaks too (getPeakUsage), as HotSpot records every pool's peak at gc_begin / gc_end; resetPeakUsage resets that peak. Default ON, kill "0" (a query-time peak; also off with CRATONVM_ZGC_POOL_PEAK_AT_GC=0).
    E { group: Group::GC, token: "zgc-metaspace-peak-at-gc", on_key: Some("CRATONVM_ZGC_METASPACE_PEAK_AT_GC"), off_key: None, off_word: Some("0"), since: "2026-10-15" },
    // zt t11/pl3: on ZGC every collection pause writes the perf-data safepoint and collector counters when it ends (before its own GC-notification wake-up), as HotSpot updates them at the pause. Default ON, kill "0" (the sampler's next 100 ms pass; explicit collections keep CRATONVM_PERF_EVENT_REFRESH).
    // zt t13/x10 (2026-10-19): on ZGC the in-process perf counters are also refreshed when a collection's GC notifications are about to be queued for a listener (after the beans' update, before the enqueue), so a delivery thread an earlier hint woke cannot run a listener that reads them one collection behind; the late pause refresh stays. Default ON, kill "0".
    // zt t11/pl3: on ZGC java.cls.loadedClasses (jstat -class Loaded) counts the instance classes ClassLoadingMXBean counts (no array classes, no compatibility stubs). Default ON, kill "0" (every live class of the store).
    // zt t11/pl3: default ON since zt t20/fl1, kill "0". The ZGC GcInfo Metaspace edge samples and the perf-data sampler's metadata pass on ZGC take HotSpot's model figures from the running arena tally (the classes defined since the last look) instead of a class-store walk (proposal zgc-zt1ob-proposal-incremental-metaspace-figures).
    // zs s6/ob5: on ZGC a usage-threshold crossing found by setUsageThreshold (any pool) raises the drain hint, so the next allocation door delivers its MEMORY_THRESHOLD_EXCEEDED notification without waiting for a collection, as HotSpot's Service Thread does. Default ON, kill "0" (delivered after the next collection).
    E { group: Group::GC, token: "zgc-threshold-set-drain", on_key: Some("CRATONVM_ZGC_THRESHOLD_SET_DRAIN"), off_key: None, off_word: Some("0"), since: "2026-10-15" },
    // gcj j1/ob: on Generational a usage-threshold crossing that setUsageThreshold finds (Serial's heap pools, or a non-heap pool armed on the generational heap's table) is handed to the delivery thread at once (zs s9/ob6's ZGC hand-off), so a setting thread that only sleeps still gets MEMORY_THRESHOLD_EXCEEDED, as from HotSpot's Service Thread. Read once per heap (GcNotificationQueue::new). Default ON, kill "0" (the drain hint alone: delivered at the setting thread's next allocation door or the next collection).
    // gcj j1/ob: on Generational, -Xlog:safepoint+stats (safepoint*, all) prints HotSpot 25's SafepointTracing statistics table (the header every 30 safepoints, one row per safepoint before its `Safepoint` line: operation, threads total / initial running, sync / vmop / total ns, page_trap_count) and at exit its summary (per-operation counts with the terminating Halt, Maximum sync time, Maximum vm operation time). G1 / ZGC log nothing there, as before. Default ON, kill "0" (nothing on safepoint,stats).
    // gcj j1/ob -- end
    // gcj j2/ob2 (2026-10-19) -- begin
    // gcj j2/ob2: on Generational, jcmd VM.stringtable [-verbose] (and the DiagnosticCommand MBean's vmStringtable) reads the live interned-String table: the entry and literal counts, the literal payload bytes, and under -verbose the texts (sorted). It answered a constant 0-entry text on every collector. G1 / ZGC unchanged. Default ON, kill "0" (the constant text).
    // gcj j1/st (2026-10-25): HotSpot's String table bookkeeping on every collector: a String the table drops stays one of HotSpot's uncleaned entries (counted by jcmd VM.stringtable and -XX:+PrintStringTableStatistics, no literal bytes) until the concurrent work a collection triggers cleans it, the table doubles past load factor 2 (up to 2^24 buckets), and -Xlog:stringtable* prints HotSpot's Uncleaned items / Concurrent work / grow / clean lines. Default ON, kill "0" (no uncleaned entries, no growth, no per-collection lines).
    // gcj j2/ob2: -XX:+PrintFlagsFinal prints [Global flags] and the flags this VM models in HotSpot's JVMFlag::print_on layout with command-line / ergonomic / default origins; exact two-mode gates cover selected shared-registry rows. `=0` is a diagnostic kill.
    // gcj j2/ob2 (rows on j2a): on Generational, -XX:StartFlightRecording takes name= and maxsize= (accepted, not modelled) where the launcher ended the run with "unrecognized option"; HotSpot 25.0.3 starts the recording. G1 / ZGC unchanged. Default ON, kill "0" (the refusal).
    // zs s10/jf3 (ported by zt t1/jf): the real backend writes HotSpot 25's four all-zero jdk.MetaspaceChunkFreeListSummary rows per ZGC collection (Before/After GC x Metadata/Class). Default ON, `0` = none.
    // zt t29/g1 (G1 carry page item 1): the four all-zero jdk.MetaspaceChunkFreeListSummary rows on every collector (G1, Generational, ZGC), as HotSpot 25's shared GCTracer. Default ON, `0` = none on any collector (ZGC keeps its own key too).
    // zs s10/jf3 (ported by zt t1/jf): a ZGC concurrent cycle's jdk.GarbageCollection / ZOld / ZYoung span from its mark-start pause to the closing collection's end, and the mark-start pause is the cycle's first `Pause Mark Start (Major)` row counted in sumOfPauses / longestPause. Default ON, `0` = the collection's pause only, the mark-start row on its own.
    // zt t1/jf: a ZGC major writes the jdk.ZYoungGarbageCollection of the young collection HotSpot 25's (generational-only) major runs, under the major's gcId (threshold 0 = promote all in the single-generation mode and on System.gc(); the promotion age in the generational mode). Default ON, `0` = none for a major.
    // zt t4/x3: a ZGC major's jdk.ZYoungGarbageCollection ends at the major's Pause Mark End row (zt t3/jf, zt2x1 item 4) instead of spanning the whole cycle. Default ON, kill "0" (the whole-cycle span, as before t3).
    // zt t1/jf: the real backend writes a ZGC collection's stop-the-world time as HotSpot 25 ZGC's VM operations: one SafepointBegin / ExecuteVMOperation / SafepointEnd per named pause (ZMarkStartYoungAndOld, ZMarkEndOld, ZRelocateStartOld; the Young ones for a minor), caller = the collector thread. Default ON, `0` = one safepoint per collection, operation `Collect` (ZGC's collections are not `old`).
    // zt t2/al: ZGC zeroes each TLAB chunk at the carve in pieces from the top down (`ZeroPlan::zero_descending`), so the bytes the bump hands out first are the last written and likeliest still cached; same bytes zeroed, no soft Tlab::end (the zr7ca chunk-zeroing-frontier cache argument without the retired LAZY_TLAB_ZERO design). Default ON (16 KiB pieces); "0"/off kills (whole-chunk ascending memset); a number = that many KiB (clamped 4..256).
    // zt t2/jf: on ZGC the real backend's jdk.CodeCacheStatistics is one row per HotSpot code heap (codeBlobType key, the non-profiled heap's unallocatedCapacity = cap - used) and jdk.CodeCacheConfiguration states the code-cache cap, the figures the CodeHeap pool beans report. Default ON, `0` = the generic one row (codeBlobType null) and zero sizes.
    // zt t3/jf: on ZGC jdk.SystemGC is the System.gc() door's per-request row (calling thread, request stack, the collection's span, as HotSpot's JVM_GC: local 25.0.3 Zt1JfSystemGcSpan ends_with_collection=true); the Runtime.gc() native records the caller's thread first. Wins over CRATONVM_ZGC_JFR_SYSTEM_GC_ON_CALLER. Default ON, `0` = the caller-side writer.
    // zt t5/jf (R9): a ZGC collection prints HotSpot 25's gc,phases pause lines (`GC(n) Y: Pause Mark Start (Major) 0.011ms`, `O: Pause Mark End`, `O: Pause Relocate Start`; `y:` for a minor) and its gc,metaspace line (`O: Metaspace: NM used, NM committed, NM reserved`) between its start and end gc lines. Default ON, `0` = neither.
    // zt t18/zc1 (opt-in): the concurrent ZGC profile, stage 1 of docs/feature-designs/zgc-architecture.md. Turns on every built stage-1 piece (every trigger opens a concurrent mark; System.gc() runs a concurrent cycle); each piece's own switch still wins.
    // zt t18/zc1 (opt-in; on under CRATONVM_ZGC_CONCURRENT, "0" kills it there): a ZGC System.gc() / jcmd GC.run opens a concurrent cycle after the call, waits for its fixed point and closes it with its own collection (HotSpot's ZDriverPort::send_sync of one whole cycle). Every-trigger mode only.
    // zt t32 (page zgc-concurrent-relocation-and-load-barrier-marking sections 1-2, R5 / R6; opt-in): a ZGC closing pause that would slide PLANS the relocation instead -- regions selected from the survivors, every destination allocated, only the eager set copied (roots and every address a remap looks up, words the pause cannot leave bad, pin / overlay owners); every other object is copied after the release by the load barrier, the next mark or the collecting thread's drain, and the regions return to the free list at the next mark end. Needs CRATONVM_ZGC_LOAD_BARRIER=colored, CRATONVM_ZGC_COLOR_FLIP=1, CRATONVM_ZGC_REMAP_IN_MARK=1, a concurrently marked single-generation cycle; every refusal keeps the slide.
    // zt t32 (page zgc-concurrent-relocation-and-load-barrier-marking section 5, 2f; opt-in): mark through the load barrier -- under the color flip (CRATONVM_ZGC_COLOR_FLIP=1, CRATONVM_ZGC_LOAD_BARRIER=colored) the store side's SATB pre-barrier logs only the deletion of a PLAIN word, read raw at the accessor, the JIT store helpers and the CAS family; a colored word's target is marked by the load that hands it out. Nothing without the flip.
    // zt t32 (page zgc-concurrent-relocation-and-load-barrier-marking section 8, steps 2b / 2d; opt-in, on under CRATONVM_ZGC_CONCURRENT, "0" kills it there): a ZGC cycle that parks its sweep clears its dead referent slots after the release, first thing in the VM's reference round (and before any runner's sweep walk), instead of in its pause; until then every mutator read of Reference.referent (getfield, the compiled helper, JNI, the lambda arm, the natives' heap access) answers null for a referent the mark did not reach, as HotSpot's resurrection-blocked load. Needs CRATONVM_ZGC_CONCURRENT_REFS.
    // zt t32 (page zgc-concurrent-relocation-and-load-barrier-marking section 10): a requested ZGC cycle's door (System.gc(), jcmd GC.run, JVMTI, the metadata door) whose mark start lost to another pause in progress -- a peer's poll serving the same request -- takes part in that pause and adopts the cycle it opened, as HotSpot's synchronous request owns the cycle it starts; every pause of the cycle carries the requester's cause. Default ON, "0" = one attempt, then the stop-the-world fallback.
    // zt t32 (page zgc-concurrent-relocation-and-load-barrier-marking section 10): the ZGC concurrent mark start publishes its initiator's own roots (update_root_snapshot) before it gathers them, as the collection door does at entry; collect_roots step 11 and the registry gather push the initiator's root_snapshot, which held the frames of the last PARK -- a dead finalizer-chain round's array, kept through one System.gc(). Default ON, "0" = the last deposit, as before.
    // zt t21/zc2 (opt-in; on under CRATONVM_ZGC_CONCURRENT, "0" kills it there): a qualifying whole-heap ZGC cycle ends its pause at the sweep (Pause Mark End) and the collecting thread runs the sweep after the release, with the mutators live; every consumer of the sweep's totals runs at its install. Stage 1d of docs/feature-designs/zgc-architecture.md.
    // zt t22/hunt3: under -Xlog:safepoint a ZGC collection prints HotSpot 25 ZGC's `Safepoint "<op>"` lines (ZMarkStartYoungAndOld, ZMarkEndOld, ZRelocateStartOld; the Young ones for a minor) cut at its pause rows, and a concurrent cycle's own mark start prints ZMarkStartYoungAndOld; it printed no line for any ZGC pause. Default ON, `0` = no ZGC line.
    // zt t22/zc3 (opt-in; on under CRATONVM_ZGC_CONCURRENT, "0" kills it there): a non-generational ZGC collection keeps every Weak / Phantom (and condemned Soft) referent in its slot: the markers hide the slot instead of the VM pre-collection pass nulling it, and the pause nulls it when the referent dies, before the sweep. Stage 1c of docs/feature-designs/zgc-architecture.md.
    // zt t31/zc11: on a ZGC cycle that defers its sweep, the VM reference round runs after the release, before the install (program page 1c step 2c); opt-in, on under the concurrent profile
    E { group: Group::GC, token: "zgc-remap-in-mark", on_key: Some("CRATONVM_ZGC_REMAP_IN_MARK"), off_key: None, off_word: None, since: "2026-10-23" },
    // zt t22/zc3b (default ON, "0" kills): a ZGC concurrent mark start binds the References registered at allocation before it builds the referent skip set, so a System.gc()-owned concurrent cycle clears the weak referents made since the last collection (zc1_S1_J alive_after_gc=80000 vs HotSpot 0).
    // zt t23/hunt5 (default ON, "0" kills): a ZGC concurrent mark start roots the objects queued for finalize() (the finalizer queue, as G1's initial mark does), so a finalize() that runs during the concurrent phase and stores its object into an already-scanned one does not have it freed at the mark end.
    // zt t23/hunt5 (default ON, "0" kills): with a ZGC concurrent cycle open, the last-ditch clear-all-soft collection marks from scratch (the open cycle is abandoned) instead of closing the cycle with its mark set, as HotSpot's Allocation Stall cycle starts after the stall; it also keeps the soft-precise probe, so a soft reference whose referent a static holds is not cleared because a stale compiled-frame word names it.
    // zt t23/zc5 (R2, opt-in "1"): the ZGC low slide's evacuating arm (as CRATONVM_ZGC_SLIDE_EVACUATE=1, which it implies) copies through a per-region relocation cycle (claim, copy-then-install, seal) instead of its own loop; same copies, same destinations; the rewrite reads the pairs the cycle's tables answer. `[GC] zgc-rreloc:` under -verbose:gc. Unset / "0" = the slide.
    E { group: Group::GC, token: "zgc-region-relocate", on_key: Some("CRATONVM_ZGC_REGION_RELOCATE"), off_key: None, off_word: None, since: "2026-10-20" },
    // zt t23/zc6 (default ON since zt t26/zc9, "0" kills; acts only where the every-trigger mode is armed -- CRATONVM_ZGC_CONCURRENT or CRATONVM_ZGC_CONC_EVERY_TRIGGER -- so the default collector is untouched): the ZGC director tick closes an open every-trigger concurrent cycle that has stayed at its fixed point across two looks (1 ms apart), on a thread attached for it, through the allocation-threshold door -- HotSpot's Pause Mark End at the end of marking -- so a program that stops allocating does not keep the SATB barrier armed. Stage 1f of docs/feature-designs/zgc-architecture.md.
    // zt t23/x19 (default ON, "0" kills; acts only with a deferred sweep pending, CRATONVM_ZGC_CONCURRENT_SWEEP): the VM's exit finishes a pending deferred sweep (or waits for its runner) before the -Xlog:gc+exit heap lines, the [GC] summary and the end-of-run metrics row read the heap (review page item 28d).
    // zt t23/x19 (default ON, "0" kills; every collector, both modes): JVMTI's com.sun.hotspot.functions.IsClassUnloadingEnabled answers -XX:-ClassUnloading / -Xnoclassgc in the OnLoad phase too (a startup agent's env has no VM yet; it answered JNI_TRUE there), as HotSpot reads the parsed ClassUnloading flag in any phase (review page item 28e).
    // zt t23/st1 (default ON, "0" kills): the weak interned-String table also engages on a ZGC heap that marks concurrently (CRATONVM_ZGC_CONCURRENT=1, every-trigger, CRATONVM_ZGC_CONC_START): the pool is no mark root, pool hits are kept alive through the SATB barrier while a cycle marks, the closing collection sweeps the dead entries (HotSpot 25: Zr1RfInternProbe weakclear prints dropped-intern-weak-cleared=true).
    // zt t23/zc7 (R3, opt-in "1"; "2" = no hand-over): with CRATONVM_ZGC_REGION_RELOCATE=1, the region relocation cycle and its quarantined from-space outlive the closing pause; the cycle answers stale-address lookups instead of the relocations table until the next mark end, where "1" hands its pairs to that table. `[GC] zgc-rquar:` under -verbose:gc. Unset / "0" = R2.
    // zt t23/zb8 (default ON, "0" kills; acts only while the ZGC read barrier is armed, CRATONVM_ZGC_LOAD_BARRIER=colored): Reference.refersTo0 / PhantomReference.refersTo0 (natives, JIT direct helpers, the leaf arm) read the referent with ZGC's non-marking Weak load (NativeHeapAccess::get_field_no_keepalive), as HotSpot loads it AS_NO_KEEPALIVE; the strong read marked it during a mark.
    // zt t24/hunt7 (page zgc-zt23hunt4-fragmentation-and-young-compaction-limits-ignored item 2): with launcher-zgc-limit-flag-syntax, -XX:ZFragmentationLimit / -XX:ZYoungCompactionLimit outside HotSpot 25's range(0, 100) print "double ZFragmentationLimit=200.000000 is outside the allowed range [ 0.000000 ... 100.000000 ]" and "Improperly specified VM option 'ZFragmentationLimit=200'" in the ordered first-refusal scan, exit 1. Default ON, "0" = an out-of-range value boots again.
    // zt t24/hunt7 (page zgc-zt23hunt5-oom-ladder-can-rest-on-a-mark-that-started-before-the-stall): when the ZGC allocation ladder reaches its clear-soft rung with no active soft reference and either a concurrent cycle is open or the last collection closed one with its mark set, the rung runs one collection that marks from scratch (the collecting thread's full request) before OutOfMemoryError, as HotSpot fails a stalled allocation only after a cycle that started after the stall. Never reached on the default stop-the-world ZGC. `oom_fresh_marks=` on [GC] zgc-refusal:. Default ON, "0" = no collection there, as before.
    // zt t24/hunt7 (page zgc-zt24hunt7-out-of-memory-xlog-line-missing): a heap OutOfMemoryError ("Java heap space") raised on ZGC prints HotSpot 25 ZGC's "Out Of Memory (<thread>)" line under -Xlog:gc / -verbose:gc (ZHeap::out_of_memory; measured on 25.0.3 after the last "Allocation Stall (main)" line). Default ON, "0" = no line, as before.
    // zt t24/x20 (page zgc-zt13x10-review-residuals item 30b): the modelled manageable HotSpot rows (MinHeapFreeRatio, MaxHeapFreeRatio) are writeable and external on the com.sun.management.internal.Flag surface, setVMOption writes them per VM (HotSpot's range and constraint messages; the collector's sizing policy gets the value; origin MANAGEMENT), and ZGC's SoftMaxHeapSize row is external, so getDiagnosticOptions lists them as on HotSpot. Default ON, "0" = every modelled row read-only and non-external again.
    // zt t24/x20 (page zgc-zt13x10-review-residuals item 30c): the modelled UseZGC / UseG1GC row of the collector the VM runs without its flag given reads {ergonomic} (HotSpot's GCConfig::select_gc_ergonomically, FLAG_SET_ERGO_IF_DEFAULT), so jcmd VM.flags names the collector and JMX reports origin ERGONOMIC; it read {default} and VM.flags named none. Default ON, "0" = {default} again.
    // zt t26/x21 (pages zgc-zt22hunt3-always-pre-touch-ignored item 5, zgc-zt13x10-review-residuals item 30h): the modelled HotSpot flag table carries AlwaysPreTouch, CrashOnOutOfMemoryError, DisableExplicitGC, ExitOnOutOfMemoryError, ExplicitGCInvokesConcurrent, StackTraceInThrowable (product, HotSpot 25's defaults) and SoftMaxHeapSize (manageable on every collector, MaxHeapSize {ergonomic} when not given; setVMOption writes it off ZGC), so getVMOption / PrintFlagsFinal / jcmd VM.flags answer them (getVMOption("AlwaysPreTouch") threw IllegalArgumentException; getDiagnosticOptions lacked SoftMaxHeapSize off ZGC). Default ON, "0" = the rows absent again.
    // zt t29/sm: OPT-IN. The ergonomic soft maximum also paces the ZGC director (off: reported only -- gc,init / gc,heap lines and the JMX row; m8 rt2_o8_tail ran ~2500 back-to-back Allocation Rate collections above it).
    // zt t30/fz (page g1-and-generational-items-carried-from-the-zgc-zt-round item 10b; fin1 page items 1a / 3 / 3a): on G1 and Generational, under the default finalizer policy, the allocation doors hand finalize() to the delivery thread whether or not the thread holds a lock, a lock-free System.gc() hands the queue over and waits for its finalizer steps (leaving the rest to the thread, never draining on the caller), runFinalization() does the same (caller drain only as the fallback), and the delivery thread runs finalizers in slices; HotSpot 25 runs none on a mutator (rows x21_F1g, rf28_g1 printed ran-on-caller true). ZGC keeps its own keys. Default ON, "0" = the inline drains on G1 / Generational again.
    // zt t26/zc5b (default ON, "0" kills; ZGC, matters on --compatible): a stop-the-world ZGC collection pins the values of the weak-keyed JUL per-object rows (a Logger's handlers / parent / filter, a Handler's ErrorManager) and the SSLContext-keyed TrustManager / KeyManager rows to their key (metadata_pin) instead of rooting them, the Generational ephemeron rule (CRATONVM_GEN_SIDE_ROW_EPHEMERON) on the ZGC inherited-ThreadLocal gate; a dropped webapp loader whose Logger holds its Handler now unloads in one System.gc() as on HotSpot. "0": rooted as before.
    // zt t26/zc5b (diagnostic, opt-in "1", reads only; every collector): one "[sysgc-door] outcome=collected|coalesced|lost ..." stderr line per System.gc() door, after its attempts loop.
    // zt t27/pc2 (default ON, "0" kills; pause CPU only, same accounting): the ZGC bitmap sweep sizes a survivor from the previous sweep's size cell for the same base (bases the slide moved dropped; valid for one gc_count and one compact-layout epoch) instead of reading its header.
    E { group: Group::GC, token: "zgc-sweep-size-memo", on_key: Some("CRATONVM_ZGC_SWEEP_SIZE_MEMO"), off_key: None, off_word: Some("0"), since: "2026-10-21" },
    // zt t24/zc8 (default ON since zt t26/zc9, "0" kills; acts only where the every-trigger mode is armed -- CRATONVM_ZGC_CONCURRENT or CRATONVM_ZGC_CONC_EVERY_TRIGGER -- so the default collector is untouched): Concurrent Mark Continue -- a ZGC allocation-door collection (the occupancy poll, the mark-end service) that closes an every-trigger cycle first runs a Pause Mark End attempt; when the SATB buffers or the re-pushed roots hand the pool new mark work it releases the world without a collection, the pool keeps marking and the next fixed point tries again (at most 4 failed attempts per cycle, never at the urgent reserve), as HotSpot 25's Pause Mark End fails and resumes Concurrent Mark Continue. Stage 1f of docs/feature-designs/zgc-architecture.md.
    // zt t24/zc8 (default ON, "0" kills; acts only on a ZGC cycle whose sweep ran after the release, CRATONVM_ZGC_CONCURRENT_SWEEP): a cycle marked concurrently does not feed the pause-target allocation budget -- its pause walked neither the mark's nor the sweep's span -- and drops the budget a stop-the-world cycle sized; the profile's closing pauses (roots, SATB replay, reference round) had sized it down to 11-20 MB and multiplied the cycle count (t20 zc1_A2_cm_Z: 66 cycles, 61 budget-triggered). Stage 0 CPU of docs/feature-designs/zgc-architecture.md.
    E { group: Group::GC, token: "zgc-conc-close-forgets-pause-budget", on_key: Some("CRATONVM_ZGC_CONC_CLOSE_FORGETS_PAUSE_BUDGET"), off_key: None, off_word: Some("0"), since: "2026-10-20" },
    // zt t26/zc9 (opt-in; on under CRATONVM_ZGC_CONCURRENT, "0" kills it there): with ZGC's every-trigger mode acting, a poll with no cycle open or requested asks for the concurrent cycle at HotSpot's allocation-rate point -- free space at or below the urgent reserve plus twice (ZAllocationSpikeTolerance) the allocation expected during a mark, while the heap can still finish one, after the warm-up cycles and a grace of allocation since the last collection -- instead of at a clause that fires too late to finish a mark (a stop-the-world cycle, ct_no_headroom); a refused request of the rule is dropped, not collected. Stage 1a of docs/feature-designs/zgc-architecture.md; census [GC] zgc-rate-start: crs_*.
    // zt t27/fin1 (page zgc-zt27fin1-finalizable-churn-and-signals item 1): on ZGC, under the default finalizer policy, the delivery thread runs its finalizer step in slices (the queue length at the slice start, 20 ms) and delivers the GC notifications and ReferenceQueue wake-ups queued meanwhile between slices; the batch still ends with the queue empty. A finalizable churn kept the one unbounded step running and held every notification back (HotSpot: Finalizer and Notification Thread are separate). Default ON, "0" = one unbounded step again.
    // zt t27/fin1 (diagnostic, opt-in "1", reads only; every collector): one "[fin-delivery] ran=N left=M step_us=T sliced=<bool>" stderr line per delivery-thread finalizer step that ran anything.
    // t1 wave 12, lane I24J (diagnostic, opt-in "1", reads only): "[fin-retry] #N <event>" stderr lines for the finalizer
    // withdraw / re-register protocol (unconstructed mark, creator keep / withdraw, walk-entry withdraw / note, interpreted
    // `<init>` return on a finalizable receiver, compile-bar answers); first 40 per kind, then every 250th (vm
    // `interpreter::exception_dispatch::dbg_finalizer_retry_on`).
    // zt t27/fin1 (page zgc-zt27fin1-finalizable-churn-and-signals item 2): a backstop thread per caught SIGTERM / SIGINT / SIGHUP waits 5 s for the signal job to start on its attached thread; a job that never started (a VM wedged in a pause or on a lock the attach needs) ends the process with 128 + signal (OS default action for SIG_DFL, nothing for SIG_IGN) without the VM. A hung --compatible VM ignored SIGTERM until SIGKILL; HotSpot exits 143. Default ON, "0" = no backstop.
    // zt t27/fin1 (diagnostic, a number of ms, default 0): the launcher's signal job sleeps this long before it counts as started, the positive control of the exit backstop.
    // zt t27/gn5 (default ON, "0" kills; only in ZGC's generational mode, CRATONVM_ZGC_GENERATIONAL=1; gn page item 13): the weak interned-String table (CRATONVM_ZGC_WEAK_STRING_TABLE) also engages in the mode -- every collection, minor or major, leaves the pool out of its roots and drops the entries whose String it freed (a minor frees only young Strings: an old one stays until a major proves it dead). HotSpot 25: Zr1RfInternProbe weakclear prints dropped-intern-weak-cleared=true.
    // zt t27/gn5 (default ON, "0" kills; only in ZGC's generational mode with -XX:ZCollectionIntervalMinor; gn page item 14): HotSpot 25's rule_minor_timer -- the director tick runs a young collection with cause Timer (-Xlog:gc "Minor Collection (Timer)") whenever that many seconds passed since the last collection ended. "0": the flag is parsed and ignored, as before.
    E { group: Group::GC, token: "zgc-gen-minor-timer", on_key: Some("CRATONVM_ZGC_GEN_MINOR_TIMER"), off_key: None, off_word: Some("0"), since: "2026-10-21" },
    // zt t27/zc10 (default ON, "0" kills; mark CPU only, the same answers): every ZGC mark (the parallel stop-the-world pool, the concurrent pool) asks a per-cycle table of the skip set's member CLASSES before the referent-skip Bloom filter, so a heap with many live References (which saturates the filter: CRATONVM_ZGC_CONCURRENT_REFS, CRATONVM_ZGC_CONC_REF_SKIP) no longer sends every marked object to the skip set's lock. Census [GC] zgc-skip-class-filter: skc_*.
    // zt t31/zc11: the referent skip set as a member bitmap (no hash set, Bloom filter or class table built per collection); =0 the hash-set path
    // zt t31/zc11: a ZGC concurrent mark start waits this long (us, default 5000, cap 10 ms) for cooperative arrivals before it freezes a peer; 0 = the global take-over grace
    // zt t27/zc10 (default ON, "0" kills; ZGC, matters on --compatible, acts only in a concurrent cycle): the inherited-ThreadLocal values of an unstarted child Thread are pinned to the child (metadata_pin) by a ZGC concurrent cycle's opening scan and by the scans while its mark is in flight too, and a bucket taken mid-cycle leaves a handle-free shadow until the closing pause; a never-started Thread dropped before a concurrent System.gc() loses its values in that cycle (UnstartedThreadItlProbe gcCalls=1, was 2). "0": those scans root the values, as before.
    // zt t26/zc5b item 2 residual: correctness, default ON, kill "0". The ZGC side-row values (JUL / TLS rows) defer in
    // a concurrent cycle's scans too, and every scan of the cycle re-files the pins the cycle filed since its opening
    // scan (vm `memory::native_roots::side_row_cycle_pins`), so a webapp loader whose `Logger` holds its `Handler`
    // unloads under `CRATONVM_ZGC_CONCURRENT=1` as it does stop-the-world.
    // zt t27/lg1 (pages zgc-zt12tg5 item 5, zgc-zt13x10-review-residuals item 23i; rows tg8_s2, x15_T2): on Linux a ZGC maximum heap needing more mappings than /proc/sys/vm/max_map_count prints HotSpot 25's five [warning][gc] lines at heap initialisation, -version included. Default ON, "0" = nothing again.
    // zt t27/lg1 (page zgc-zt12tg5 item 5): a bare -jar with an -Xlog selection prints libjli's error line, then the full usage from a booted VM (its -Xlog startup lines first), exit 1. Every collector. Default ON, "0" = no VM again.
    // zt t27/lg1 (page zgc-zt12pl4-perf-counters-what-is-left; row pl8_census_J): java.vm.name is OpenJDK 64-Bit Server VM, java.vm.vendor / java.vm.version the image's release IMPLEMENTOR / JAVA_RUNTIME_VERSION and sun.management.compiler HotSpot 64-Bit Tiered Compilers, in every view (properties, JVMTI, perf counters, JMX, jcmd, JDWP, JFR). Both modes. Default ON, "0" = cratonvm / cratonvm / 25.0.1+8 / CratonVM JIT again.
    // zt t31/x10 (page zgc-zt13x10-review-residuals item 30h): G1 and Serial write HotSpot's full-collection heap dumps (HeapDumpBeforeFullGC / HeapDumpAfterFullGC, FullGCHeapDumpLimit, the java_pid<pid>.hprof[.N] sequence the OOM dump shares, the GC(n) Heap Dump (before full gc) trace lines), G1 runs its periodic collection (G1PeriodicGCInterval / G1PeriodicGCSystemLoadThreshold on a G1 Service thread, G1PeriodicGCInvokesConcurrent, gc,init "Periodic GC: Enabled"), and the five manageable rows are listed on every collector. Default ON, "0" = ZGC-only rows and no dumps / periodic collection again.
    // zt t29/pt (page zgc-zt13x10-review-residuals item 30h): the ParallelGCThreads / ConcGCThreads flag rows (getVMOption, PrintFlagsFinal, jcmd VM.flags) answer HotSpot 25's ergonomic counts: G1 WorkerPolicy count {default} + scaled ConcGCThreads {ergonomic}, ZGC ZHeuristics 60 % / 25 % heap-capped counts {default}, Serial 0; an explicit value {command line}. Default ON, "0" = ParallelGCThreads 0 {default} and no ConcGCThreads row, as before.
    // Declared 2026-08-06 with the DBG/JIT block: a millisecond goal that
    // `adapt_young_trigger_to_pause` reads. Default 200 since 2026-08-11
    // (`gen_heap::DEFAULT_YOUNG_PAUSE_GOAL_MS`), so it is a default-ON knob
    // whose parser reads `0` as false — which is exactly what `off_word` is
    // for. Without it the generated inventory row says `opt-in | off`, which
    // has been untrue since the default flipped, and
    // `CRATONVM_GC=-young-pause-goal-ms` has no way to turn it off.
    // gci i2/yg (opt-in): an in-band young pause (goal/4 <= pause <= goal) judges a pending halving
    // trial of the pause-goal loop (`gen_heap::next_young_trigger`) instead of dropping it.
    // gci i3/yg (opt-in): a young cycle the VM's allocation-failure door forced is not fed to the
    // pause-goal loop (`gen_heap::GenerationalHeap::feed_pause_goal_loop`); census `adapt_skipped_forced=`.
    // gci i4/yg (opt-in): a JIT-guarded TLAB refill young cannot serve collects young once per young cycle
    // (after a quarter of the semi was carved) instead of tenuring the object
    // (`gen_heap::GenerationalHeap::young_exhausted_refill_wants_collection`); `gen_refill_exhausted_*=`.
    // gci i22/dr (opt-in): a young TLAB refill takes the smallest whole free block that makes a useful TLAB
    // (`gen_heap::GenerationalHeap::refill_smallest_whole_block`); `yp_refill_whole_below_request=`.
    // gci i23/ep (Generational only; default ON since the 2026-10-17 WARNING flip gci-i48-flipB, `=0` kills -- the
    // "opt-in" this comment said predates the flip, corrected by gcj j1/rf): the per-object JUL rows and the
    // `SSLContext`-keyed manager rows report their values as `metadata_pin` rows of their key on a licensed STW scan
    // (ephemeron), not as roots.
    // gcj j1/rf: default ON (`=0` kills). The watched-referent set is published per heap (keyed by the
    // Generational heap's old-generation base; `gc_quiescence::set_watched_referents_for`), so a second VM's
    // publication on the same thread cannot replace or be read for the first VM's (`=0`: the one-slot set).
    // gcj j1/rf: default ON (`=0` kills). Generational: an Unsafe / JNI GetObjectField / reflective / handle read of slot 0
    // of a java.lang.ref.Reference subclass logs the value through the SATB barrier while a concurrent old mark is open
    // (HotSpot's ON_UNKNOWN_OOP_REF / is_referent_field keep-alive; `vm_exec::gen_referent_read_keep_alive`).
    // gcj j1/rf (opt-in, Generational only): a rooting native-roots `defer_or_root` of a user-loader row drops an OLD
    // value whose block is free (gch h4/wr's rule for the loader-licensed deferrals; `native_roots::defer_or_root`).
    // gcj j1/rf: default ON (`=0` kills). Generational: a clear-all-soft (last-ditch) collection records its clock for
    // the HotSpot soft LRU, as HotSpot's update_soft_ref_master_clock does after every round (`interpreter.rs`).
    // gcj j2/rf2: default ON (`=0` kills). Generational (ec47): a major marks its finalizer seeds (the true-root
    // seed's candidates, the moving cycle's promoted resurrected finalizables) in its resurrection round, not as
    // roots, so the old children of a dead young finalizable are the depth-2 set (`gen_heap.rs` `old_gen_gc_inner`).
    // gcj j2/rf2 (opt-in, needs weak-string-table-insert-grace): a pool HIT is rooted through the next pause too
    // (the g4rs grace residual; `string_table::keep_alive_on_hit`).
    // gcj j3/rf3 -- begin
    // gcj j3/rf3: default ON (`=0` kills; engaged only with gen-major-defers-finalizer-seeds). Generational (ec47,
    // the legacy seed): a major on the legacy young seed moves the old referents of the young phase's resurrection
    // set to its resurrection round (`gen_heap.rs` `mark_young_to_old_refs_deferring`), so they are the depth-2 set.
    // gcj j4/rf -- begin (the two j1/rf review-residual pages)
    // gcj j4/rf: default ON (`=0` kills). All collectors: the `-Xlog:gc+ref` census of a collection round counts
    // the weak / soft rows its restore pass cleared after the round (`ReferenceProcessor::take_late_cleared`).
    // gcj j4/rf: default ON (`=0` kills). Generational true-root major: the finalizer round's depth-2 set records the
    // young objects it reaches and a promotion destination as its source (`gen_heap.rs` `old_gen_gc_inner`).
    // gcj j3/rf3 (opt-in diagnostic, Windows): an unhandled-exception filter prints one stderr line with the code of
    // ANY exception that ends the process (the crash handler's VEH reports only its fatal list).
    // gcj j3/rf3 -- end
    // gci i25/jl: opt-in. Named loggers minted by the native getLogger/addLogger are held by weak global handles (root and global strong), as HotSpot's LogManager -- gci-i24lk name-keyed-rows item 2.
    // gci i23/hs (opt-in, proposal P1/hs steps 1-2): a cross-thread frame trace of a running peer is asked by a
    // per-thread handshake served at its next `safepoint_check`; the `door=frame-trace` pause only on a miss (1 ms).
    // JIT round 23 wave 6 (proxy-define page): flipped default ON, kill "0" -- served by compiled code at its
    // helper-entry door (next entry), so the pause is only the 1 ms fallback.
    // R23-W6-FLIP (JIT round 23 wave 6, proxy lane): default ON, kill "0" (frame trace served at helper entry; pause remains the 1 ms fallback).
    // JIT round 23 wave 6 (proxy-define page): perf, default ON, kill "0". A JIT helper's entry door
    // (`jit_safepoint_flush_satb`: allocation and dispatch helpers) runs this thread's queued handshakes, so a
    // compiled peer serves a cross-thread frame trace without a pause; one TLS load and one Acquire load otherwise.
    E { group: Group::GC, token: "handshake-at-helper-entry", on_key: Some("CRATONVM_GC_HANDSHAKE_AT_HELPER_ENTRY"), off_key: None, off_word: Some("0"), since: "2026-10-21" },
    // gcj j1/cm -- end
    // gcj j2/cm2 -- begin
    // gcj j2/cm2 (default ON on the Generational backend, `=0` kills; G1 / ZGC unchanged): a registered
    // native-builtins thread (XNIO I/O thread, Vert.x event loop) retires itself through the blocked-region
    // protocol when its loop ends, so a stop-the-world census between its loop's end and its OS thread's finish
    // no longer counts a thread that never arrives (the pause hung on it); a never-spawned one is unregistered.
    // gcj j2/cm2 (opt-in; reached only under the opt-in `handshake-frame-trace` with `handshake-loop-kick` on, and
    // only on the Generational backend): each interpreter loop entry publishes the poll word it runs on, and
    // the frame-trace handshake kicks only its target's published word instead of every loop of the VM.
    // t1 lane E (opt-in; every backend; step 3 of `common-a-proposal-per-thread-handshakes`): each interpreter
    // loop entry publishes the poll word it runs on, and the frame-trace handshake raises a HANDSHAKE bit in the
    // byte of its target's word that loops and -- under `jit-poll-thread-word` -- compiled polls test, so a
    // JIT-compiled peer with no helper call and no allocation serves the handshake at its next poll.
    // gcj j3/rt3 (default ON, `=0` kills; every collector -- thread identity): an unmounted (parked virtual)
    // thread never answers "which registered thread runs on this OS thread" (`thread_id_for_current_os_tid`), so
    // a carrier asking after an unmount gets itself, not the parked continuation. Read once per registry.
    // gcj j3/rt3 (opt-in; Generational mask plan with an inline card view only): the optimizing tier's gated
    // reference putfield / aastore tests its SATB pre gate once (no re-test in the card arm), as one RIP-relative
    // compare when in reach. Refused under CRATONVM_GEN_CONC_NO_FROZEN_EAGER_SCAN=1. `ir_lower::ir_satb_gate_once_enabled`.
    // gcj j3/rt3 -- end
    // Default-ON: the key's PRESENCE makes the trusted `*_validated` header
    // accessors re-validate, which is the pre-"validate once" behaviour.
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
    // `refproc-prepass` is the kill switch for the 2026-09-21 reordering that
    // runs `remove_collected` BEFORE reference processing on the generational
    // path. Default ON; `CRATONVM_GC=-refproc-prepass` restores the old order,
    // in which a `SoftReference` whose own instance died in the cycle again
    // roots the weak-clearing closure. A bisect lever, not a tuning knob --
    // and scoped to the generational path, with G1 and ZGC byte-for-byte
    // unchanged either way.
    // gen r4w2/concmark (2026-09-23): objects per Phase-2 slice of the
    // GENERATIONAL concurrent old-gen cycle (`gc/src/concurrent_mark.rs::
    // gen_conc_mark_slice`). Default ON (sliced, 32 Ki objects, with the
    // old-gen lock released and a safepoint polled between slices and a lost
    // remark retried); `CRATONVM_GC=-gen-conc-mark-slice` writes `0`, which
    // restores the one-lock-hold driver as the A/B arm. The bare token writes
    // `1` — one object per slice, a stress setting; set the variable itself to
    // tune. G1 and ZGC are unaffected either way.
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
    // Interpreter round i5 wave 101, lane L7: receivers of invokevirtual /
    // getfield / putfield must be assignable to the referenced class
    // (HotSpot parity; default ON, `=0` restores the reference-only check).
    E { group: Group::LOADER, token: "verify-receiver-assignable", on_key: Some("CRATONVM_VERIFY_RECEIVER_ASSIGNABLE"), off_key: None, off_word: Some("0"), since: "2026-12-03" },
    // Interpreter round i5 wave 101, lane L7: every instruction in a handler's
    // range is matched against the handler's frame (HotSpot parity; default
    // ON, `=0` restores the entry-only adoption).
    E { group: Group::LOADER, token: "verify-handler-frames", on_key: Some("CRATONVM_VERIFY_HANDLER_FRAMES"), off_key: None, off_word: Some("0"), since: "2026-12-03" },
    // Interpreter round i5 wave 101, lane L7: user-defined loaders' classes get
    // the Pass 3 type-state verdict at define (HotSpot's
    // BytecodeVerificationRemote; default ON, `=0` restores the deferral).
    E { group: Group::LOADER, token: "verify-user-loader-classes", on_key: Some("CRATONVM_VERIFY_USER_LOADER_CLASSES"), off_key: None, off_word: Some("0"), since: "2026-12-03" },
    // Interpreter round i5 wave 102, lane L7: that verdict is given at the
    // class's link, as HotSpot verifies (default ON, `=0` gives it at define).
    E { group: Group::LOADER, token: "verify-user-loader-classes-at-link", on_key: Some("CRATONVM_VERIFY_USER_LOADER_CLASSES_AT_LINK"), off_key: None, off_word: Some("0"), since: "2026-12-04" },
    // t1 wave 9, lane I24V: `-Xverify:none` / `-noverify` also drops the
    // define-time type-state verdict of application / platform classes
    // (HotSpot `BytecodeVerificationRemote` off); correctness, default ON, "0" kill.
    // t1 wave 10, lane I24W: `-Xverify:none` / `-noverify` also drops the
    // define-time class / field / method modifier and member-name legality
    // checks for application / platform classes (HotSpot `_need_verify` false);
    // correctness, default ON, "0" kill.
    // t1 wave 13, lane I24W: `-Xverify:none` / `-noverify` also drops the
    // structural `_need_verify` format checks HotSpot 25 drops (duplicate
    // members, descriptors, the constant pool's format pass, the local
    // variable tables, InnerClasses verdicts, ...); correctness, default ON,
    // "0" kill.
    // t1 wave 13, lane I24W: `-Xverify:all` extends the define-time format
    // checks to the boot and platform loaders (HotSpot
    // `BytecodeVerificationLocal`); correctness, default ON, "0" kill.
    // t1 wave 10, lane I24W: the launcher prints HotSpot 25's "Options
    // -Xverify:none and -noverify were deprecated in JDK 13 ..." VM warning on
    // stderr for each `-Xverify:none` / `-noverify`; correctness, default ON,
    // "0" = silent again.
    // Interpreter round i6 wave 119 (lane L5): one link walk for a user-loader
    // class that owes both the Pass 3 verdict and the type-map walk
    // R23-W10-FLIP: HotSpot parity, default ON, kill "0" restores the two-walk route
    // (`LoaderFlags::user_pass3_records_link_loads`; the two-walk route under-records HotSpot's verifier loads;
    // pinned to HotSpot by classloading/tests/verifier_loads_one_walk_diff.rs).
    E { group: Group::LOADER, token: "user-pass3-records-link-loads", on_key: Some("CRATONVM_USER_PASS3_RECORDS_LINK_LOADS"), off_key: None, off_word: Some("0"), since: "2026-12-21" },
    // Interpreter round i5 wave 102, lane L7: return-type, array-kind and
    // fall-off-the-end rules of HotSpot's verifiers (default ON, `=0` off).
    E { group: Group::LOADER, token: "verify-control-rules", on_key: Some("CRATONVM_VERIFY_CONTROL_RULES"), off_key: None, off_word: Some("0"), since: "2026-12-04" },
    // Interpreter round i5 wave 103, lane L7: `jsr` / `ret` methods below
    // major 51 are type-checked per subroutine context (default ON, `=0` off).
    E { group: Group::LOADER, token: "verify-subroutines", on_key: Some("CRATONVM_VERIFY_SUBROUTINES"), off_key: None, off_word: Some("0"), since: "2026-12-05" },
    // Interpreter round i5 wave 108, lane L7: the split verifier tracks
    // flagThisUninit as HotSpot does (default ON, `=0` restores the
    // locals-only check).
    E { group: Group::LOADER, token: "verify-this-uninit-flags", on_key: Some("CRATONVM_VERIFY_THIS_UNINIT_FLAGS"), off_key: None, off_word: Some("0"), since: "2026-12-10" },
    // Interpreter round i5 wave 112, lane L7: a major-50 class is verified
    // split-first with libverify failover, as HotSpot's Verifier::verify
    // (default ON, `=0` restores the single relaxed walk).
    E { group: Group::LOADER, token: "verify-major50-failover", on_key: Some("CRATONVM_VERIFY_MAJOR50_FAILOVER"), off_key: None, off_word: Some("0"), since: "2026-12-14" },
    // JIT round 21 wave 14, lane verifier: the verifier reads a hidden class's
    // own CONSTANT_Class entries as naming the hidden class, as HotSpot's
    // ClassFileParser re-points this_class (`this(...)` in a hidden class
    // verifies). Default ON, `=0` restores the class-file name.
    // Declared 2026-08-06, and RENAMED to be declarable. It was
    // `CRATONVM_JDK_ONLY_ENFORCE_SHADOW`, which `jdk_only_adds_no_environment_variable`
    // forbids in this table for a good reason: `--real-jdk` is a policy chosen
    // per invocation (contract §9), and a second env-var spelling of it would be
    // a way to half-enable strict mode from a parent shell. This knob is not
    // that -- its caller tests `is_jdk_only()` first and it only decides whether
    // §1.4's native-shadows-bytecode rule is ENFORCED or merely counted, which
    // is what its new name says. Undeclared, it was served by a live `getenv`,
    // so `CRATONVM_LOADER=enforce-native-shadow` could not reach it and no test
    // could arrange it.
    E { group: Group::LOADER, token: "enforce-native-shadow", on_key: Some("CRATONVM_ENFORCE_NATIVE_SHADOW"), off_key: None, off_word: None, since: "2026-08-06" },
    E { group: Group::LOADER, token: "unretire-native-shadow", on_key: Some("CRATONVM_UNRETIRE_NATIVE_SHADOW"), off_key: None, off_word: None, since: "2026-09-11" },
    E { group: Group::LOADER, token: "pure-bytecode", on_key: Some("CRATONVM_PURE_BYTECODE"), off_key: None, off_word: None, since: "2026-09-30" },
    E { group: Group::LOADER, token: "audit-yield-intrinsic", on_key: Some("CRATONVM_AUDIT_YIELD_INTRINSIC"), off_key: None, off_word: None, since: "2026-09-30" },
    E { group: Group::LOADER, token: "lever-keep", on_key: Some("CRATONVM_LEVER_KEEP"), off_key: None, off_word: None, since: "2026-10-02" },
    E { group: Group::LOADER, token: "lever-drop", on_key: Some("CRATONVM_LEVER_DROP"), off_key: None, off_word: None, since: "2026-10-02" },
    E { group: Group::LOADER, token: "compat-lhm-reversed-live-views", on_key: Some("CRATONVM_COMPAT_LHM_REVERSED_LIVE_VIEWS"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::LOADER, token: "compat-immutable-of-jdk-mint", on_key: Some("CRATONVM_COMPAT_IMMUTABLE_OF_JDK_MINT"), off_key: None, off_word: None, since: "2026-09-30" },
    E { group: Group::LOADER, token: "compat-setn-iteration-order", on_key: Some("CRATONVM_COMPAT_SETN_ITERATION_ORDER"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    // JIT round 16 (lane compatdrain): the r15 LOW collection review residuals and C5W4-1.
    E { group: Group::LOADER, token: "compat-unmod-iterator-jdk", on_key: Some("CRATONVM_COMPAT_UNMOD_ITERATOR_JDK"), off_key: None, off_word: Some("0"), since: "2026-10-01" },
    E { group: Group::LOADER, token: "compat-hashmap-serialized-threshold", on_key: Some("CRATONVM_COMPAT_HASHMAP_SERIALIZED_THRESHOLD"), off_key: None, off_word: Some("0"), since: "2026-10-01" },
    E { group: Group::LOADER, token: "compat-put-all-null-key", on_key: Some("CRATONVM_COMPAT_PUT_ALL_NULL_KEY"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::LOADER, token: "compat-al-add-all-empty-modcount", on_key: Some("CRATONVM_COMPAT_AL_ADD_ALL_EMPTY_MODCOUNT"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::LOADER, token: "compat-al-listitr-comod", on_key: Some("CRATONVM_COMPAT_AL_LISTITR_COMOD"), off_key: None, off_word: Some("0"), since: "2026-10-02" },
    E { group: Group::LOADER, token: "compat-hashmap-copy-presize", on_key: Some("CRATONVM_COMPAT_HASHMAP_COPY_PRESIZE"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::LOADER, token: "compat-null-collection-message", on_key: Some("CRATONVM_COMPAT_NULL_COLLECTION_MESSAGE"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::LOADER, token: "compat-chm-jdk25-sizing", on_key: Some("CRATONVM_COMPAT_CHM_JDK25_SIZING"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::LOADER, token: "compat-lhs-reversed-real-view", on_key: Some("CRATONVM_COMPAT_LHS_REVERSED_REAL_VIEW"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    // Default ON. `-cf-delegating-yield` keeps the pure-delegation
    // `CompletableFuture` natives in front of the real JDK bytecode, so the
    // yield can be A/B'd on one binary. See
    // `native_override::delegating_native_yields_to_real_bytecode`.
    E { group: Group::LOADER, token: "cf-delegating-yield", on_key: Some("CRATONVM_CF_DELEGATING_YIELD"), off_key: None, off_word: Some("0"), since: "2026-08-22" },
    // Default ON: the reader is `non_empty_non_zero_default_true` (`=0` or an
    // empty value turns it off). The row said opt-in until interpreter round
    // i4 wave 94 (lane L5 review).
    E { group: Group::LOADER, token: "aware-resolution", on_key: Some("CRATONVM_LOADER_AWARE_RESOLUTION"), off_key: None, off_word: Some("0"), since: "2026-06-24" },
    E { group: Group::LOADER, token: "boot-module-registry", on_key: Some("CRATONVM_BOOT_MODULE_REGISTRY"), off_key: None, off_word: Some("off"), since: "2026-06-23" },
    E { group: Group::LOADER, token: "classpath-jar-unnamed-module", on_key: Some("CRATONVM_CLASSPATH_JAR_UNNAMED_MODULE"), off_key: None, off_word: Some("0"), since: "2026-09-05" },
    E { group: Group::LOADER, token: "fwd-resolve-strict", on_key: Some("CRATONVM_FWD_RESOLVE_STRICT"), off_key: None, off_word: None, since: "2026-06-09" },
    E { group: Group::LOADER, token: "jar-mmap", on_key: None, off_key: Some("CRATONVM_DISABLE_JAR_MMAP"), off_word: None, since: "2026-07-24" },
    // Interpreter round i6 wave 115 (lane L5): `ClassPath::add_path` does not
    // re-read a jar the class path already has an entry for (performance
    // only; default OFF; `LoaderFlags::class_path_live_jar_no_reread`).
    E { group: Group::LOADER, token: "class-path-live-jar-no-reread", on_key: Some("CRATONVM_CLASS_PATH_LIVE_JAR_NO_REREAD"), off_key: None, off_word: None, since: "2026-12-17" },
    E { group: Group::LOADER, token: "jmod-lazy-index", on_key: Some("CRATONVM_JMOD_LAZY_INDEX"), off_key: None, off_word: Some("0"), since: "2026-10-13" },
    // zt/fp: a plain class-path jar is opened from its ZIP central directory alone (a local header is read when its entry is served); the full `ZipArchive` open (a page fault per entry of a mapped jar) is paid only by a jar that is signed, a fat jar, or one the index does not model. Default ON, kill "0".
    // zt/fp: a JDK that ships both `jmods/` and `lib/modules` boots from the image alone (HotSpot's boot source), not from its 70 JMODs (~8 MB of a Hello world's working set). Opt-in; the evidence and the flip decision are in docs/feature-designs/real-jdk-mode.md.
    E { group: Group::LOADER, token: "jar-lazy-index", on_key: Some("CRATONVM_JAR_LAZY_INDEX"), off_key: None, off_word: Some("0"), since: "2026-10-23" },
    // zs s6/rs: a lib/modules jimage on the class path is indexed by package (~900 entries, module number each) instead of by class (~27 000 String pairs, ~5 MB written at every boot); a class is confirmed by the image's own perfect-hash lookup. An image with a split package keeps the per-class map. Default ON, kill "0".
    // zs s6/rs: fixed-capacity side tables sized to a class-id bound (field_layout replace counters 4 MiB, the VM statics index 2 MiB, the getClass ref-array owners, lambda-proxy bits, java/util and class-init bits, the published-supers arena chunks, the callee negative cache, the lock-slot tables) are allocated zeroed (alloc_zeroed: only written pages become resident) instead of written element by element. Same values. Read once per process. Default ON, kill "0".
    E { group: Group::LOADER, token: "jimage-package-index", on_key: Some("CRATONVM_JIMAGE_PACKAGE_INDEX"), off_key: None, off_word: Some("0"), since: "2026-10-15" },
    // zt t3/fp: a lib/modules jimage on the class path leaves the image's /packages/<pkg>/<module> and /modules directory records out of its resource index and module list (they are libjimage's package table and jrt:/ directories; HotSpot serves none as a resource and has no module packages / modules). Saves ~2 300 index entries a boot. Read once (LoaderFlags). Default ON, kill "0" (indexed as before). Probe Zt2FpJimageDirRecords.
    E { group: Group::LOADER, token: "jimage-skip-directory-records", on_key: Some("CRATONVM_JIMAGE_SKIP_DIRECTORY_RECORDS"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    E { group: Group::LOADER, token: "class-code-compact", on_key: Some("CRATONVM_CLASS_CODE_COMPACT"), off_key: None, off_word: Some("0"), since: "2026-10-13" },
    E { group: Group::LOADER, token: "boot-class-bytes-refetch", on_key: Some("CRATONVM_BOOT_CLASS_BYTES_REFETCH"), off_key: None, off_word: Some("0"), since: "2026-10-13" },
    // zs s4/rp: boot class bytes from the runtime image beside `jmods/`.
    E { group: Group::LOADER, token: "boot-image-class-bytes", on_key: Some("CRATONVM_BOOT_IMAGE_CLASS_BYTES"), off_key: None, off_word: Some("0"), since: "2026-10-15" },
    E { group: Group::IO, token: "jimage-direct-map", on_key: Some("CRATONVM_JIMAGE_DIRECT_MAP"), off_key: None, off_word: Some("0"), since: "2026-10-13" },
    E { group: Group::LOADER, token: "lenient-clinit", on_key: Some("CRATONVM_LENIENT_CLINIT"), off_key: None, off_word: None, since: "2026-06-10" },
    E { group: Group::LOADER, token: "longrewrite-loose", on_key: Some("CRATONVM_LONGREWRITE_LOOSE"), off_key: None, off_word: None, since: "2026-07-03" },
    // Default-ON: `classloading::loaders::loader_parent_chain_enabled` treats
    // the empty string and `0` as off, everything else as on.
    E { group: Group::LOADER, token: "parent-chain", on_key: Some("CRATONVM_LOADER_PARENT_CHAIN"), off_key: None, off_word: Some("0"), since: "2026-07-30" },
    E { group: Group::LOADER, token: "resolve-cache-cap", on_key: Some("CRATONVM_RESOLVE_CACHE_CAP"), off_key: None, off_word: None, since: "2026-06-21" },
    E { group: Group::LOADER, token: "stub-delegation", on_key: Some("CRATONVM_CL_STUB_DELEGATION"), off_key: None, off_word: None, since: "2026-07-27" },
    E { group: Group::LOADER, token: "unload", on_key: Some("CRATONVM_LOADER_UNLOAD"), off_key: None, off_word: Some("0"), since: "2026-06-29" },
    E { group: Group::LOADER, token: "isolated-loader-dictionary", on_key: Some("CRATONVM_ISOLATED_LOADER_DICTIONARY"), off_key: None, off_word: None, since: "2026-09-30" },
    // zt t7/cl (t6/nd4 finding 14): the real-JDK define doors (ClassLoader.defineClass0/1/2 in both --real-jdk and --compatible, Unsafe.defineClass0, --compatible's Lookup.defineClass door) call ClassLoader.addClass(Class) for every non-hidden class a loader object defines, after the define, as HotSpot's define_instance_class does; the loader's private `classes` list was empty in both modes. The upcall's failure is the define's. Every collector (moves both modes to HotSpot); on ZGC the zgc-addclass-presize growth runs before the define. Default ON, kill "0".
    // zt t8/x5 (and, in parallel, interpreter round i7 wave 135, lane L5): the define-add-class upcall calls exactly java.lang.ClassLoader.addClass(Class) (invokespecial semantics), as HotSpot's JavaCalls::call on Universe::loader_addClass_method does; a user loader's own addClass(Class) no longer intercepts it, and Unsafe.defineClass into the application loader makes the upcall on every collector. Default ON, kill "0" (t7's virtual call on the loader).
    // zt t15/x12 (2026-10-20): a defineClass1 / defineClass2 with a null name whose bytes this very loader object already defined raises HotSpot's duplicate-definition LinkageError for the class the bytes name, parenthetical "(a.b.C is in unnamed module of loader X ...)" included, as the named path does; before, the nameless arm fell to a LinkageError without it. Every collector, both modes. Default ON, kill "0".
    // Interpreter round i7 wave 135, lane L5 (performance, opt-in; i71-orch): the define's addClass upcall enters ClassLoader.addClass's bytecode by a per-VM declared-method hint instead of the by-name declared door with its selection question. Real ClassLoader layout only; read once per VM.
    E { group: Group::LOADER, token: "define-add-class-hinted", on_key: Some("CRATONVM_DEFINE_ADD_CLASS_HINTED"), off_key: None, off_word: None, since: "2027-01-01" },
    // t1 wave 5, lane L71 (performance, opt-in; i71-orch): under --real-jdk, ClassLoader.findLoadedClass(String) is not registered, so no never-run Bridge stands in front of the JDK body and compiled loadClass callers reach it by their fast doors instead of the by-name invoke_or_native ladder.
    E { group: Group::LOADER, token: "loader-find-loaded-no-standin", on_key: Some("CRATONVM_LOADER_FIND_LOADED_NO_STANDIN"), off_key: None, off_word: None, since: "2026-10-26" },
    // t1 wave 4, lane S (performance, opt-in; i147-orch / t1-interp2 proxy hit-path cuts 1+2): AbstractClassLoaderValue's Sub key answers a complete SubIds row from a per-thread memo keyed by VM identity, and builds the key shape without its locks vector (derived on an intern miss). Same ids, same interned key.
    E { group: Group::LOADER, token: "clv-sub-fast-key", on_key: Some("CRATONVM_CLV_SUB_FAST_KEY"), off_key: None, off_word: None, since: "2026-10-26" },
    // JIT round 23 wave 2 (proxy-define page): perf, default ON, kill "0". The per-instruction handler-frame check asks a
    // handler's catch-type-to-frame-slot assignability once per handler per method, not at every pc of its range.
    E { group: Group::LOADER, token: "verify-handler-catch-memo", on_key: Some("CRATONVM_VERIFY_HANDLER_CATCH_MEMO"), off_key: None, off_word: Some("0"), since: "2026-10-21" },
    // JIT round 23 wave 8f (lane W8-L1, proxy-define page): perf, OPT-IN (n23-w8g A/B neutral). The link's type-map walk
    // of a class whose verified maps the Pass 3 walk already published (first writer wins) walks for its verifier loads
    // only, building no row, and is skipped when it records no load (classloading `type_map_walk_is_loads_only`).
    E { group: Group::LOADER, token: "link-type-map-walk-loads-only", on_key: Some("CRATONVM_LINK_TYPE_MAP_WALK_LOADS_ONLY"), off_key: None, off_word: None, since: "2026-10-22" },
    // Verifier-link-loads page (item 2): HotSpot alignment, default ON, kill "0". The verifier walks <init>, then
    // <clinit>, then the other methods (HotSpot's Symbol-address method order puts the two vmSymbols first).
    E { group: Group::LOADER, token: "verify-initializers-first", on_key: Some("CRATONVM_VERIFY_INITIALIZERS_FIRST"), off_key: None, off_word: Some("0"), since: "2026-10-24" },
    // JIT round 23 wave 8f (lane W8-L1): perf, OPT-IN (n23-w8g A/B neutral). A define's loader label reads the parent
    // loader's text once per (parent address, address epoch) from a per-VM memo in the loader-constraint table.
    E { group: Group::LOADER, token: "loader-label-parent-memo", on_key: Some("CRATONVM_LOADER_LABEL_PARENT_MEMO"), off_key: None, off_word: None, since: "2026-10-22" },
    // JIT round 23 wave 2e (proxy-define page): perf, default ON, kill "0". The user-loader Pass 3 walk memoises its
    // is_subclass answers for the class being walked (each was two loader-chain name resolutions).
    // JIT round 23 wave 3 (proxy-define page): perf, default ON, kill "0". ClassManager::has_real_boot_classes latches a
    // `true` (the boot class path only grows) instead of reading java/lang/Object's bytes from the image per call.
    // JIT round 23 wave 3 (proxy-define page): perf, default ON, kill "0". A compile-panic containment scope (every
    // guarded JIT helper call) takes its thread's depth cell once at enter and decrements through it at drop.
    // JIT round 23 wave 6 (proxy-define page, proposal 4f): perf, OPT-IN. A virtual site whose every loaded subtype
    // selects one body (a getter inherited from an abstract base) is spliced behind `0 < class word < watermark` (the
    // class count when the VM's CHA answered), the call as its miss edge; a later class misses, so no retirement is needed.
    // JIT round 23 wave 4 (proxy-define page): perf, OPT-IN. A method-entry optimizing build plants an own-code
    // checkcast/instanceof of an unloaded class as a site trap even after a side effect (resumed in place from its
    // frame state; the site-trap policy recompiles on the first firing), for methods whose sinks resume precisely.
    // JIT round 23 wave 5 (proxy-define page): perf, default ON, kill "0". A checkcast/instanceof of the null constant
    // to an unloaded class plants no trap: JVMS 6.5 resolves only for a non-null operand (cast leaves null, test is 0).
    // JIT round 23 wave 5: correctness, default ON, kill "0". A deopt frame state describes the Ref null constant as
    // FrameValue::Object(0) (a null reference) instead of Int(0), which every resume sink rebuilt as an int.
    // JIT round 23 wave 8e (lane W8-L1): perf, OPT-IN (n23-w8g A/B neutral). A spliced multi-return body's tail cold
    // throw arm (every way in a guard) reached with dead control is stepped over instead of refusing the whole method
    // (Wrapper.forPrimitiveType, DirectCodeBuilder.labelToBci under the ClassFile-API bodies).
    // t1 wave 10, lane L71 (proposal t1-intv-proposal-loader-memo-rows-retire-boot-class-site-caches): perf, OPT-IN "1".
    // The interpreter's cast / new / anewarray / catch-row / ldc site entries of a class with NO loader namespace are
    // stamped with the store slot's global resolution counter, which a user loader's new initiating row does not move.
    // JIT round 23 wave 5 (proxy-define page): perf, default ON, kill "0". The split verifier's per-method type-map
    // builder reuses the previous method's emptied row vectors within one class walk (maps byte-identical).
    E { group: Group::LOADER, token: "verify-type-map-buffer-reuse", on_key: Some("CRATONVM_VERIFY_TYPE_MAP_BUFFER_REUSE"), off_key: None, off_word: Some("0"), since: "2026-10-21" },
    E { group: Group::LOADER, token: "verify-subclass-memo", on_key: Some("CRATONVM_VERIFY_SUBCLASS_MEMO"), off_key: None, off_word: Some("0"), since: "2026-10-21" },
    // r23 wave 12 L1: the user-loader walk memoises is_interface / is_resolvable / is_direct_superclass / catch_type_is_throwable per walk (nothing loads during it, so answers are unchanged). Perf, OPT-IN "1" (neutral on n23-w12a).
    // r23 wave 12 L1: the built-in findLoadedClass rule's direct-subtype scan remembers misses per (root, loader set) and reads only subtypes added since; the full scan was quadratic in defined classes. Exact (edge movers and get_mut drop the memo). Perf, OPT-IN "1" (neutral on n23-w12a).
    // JIT r24 (proxy-define residual page): perf, default ON, kill "0". ClassStore::direct_subtype_any_since remembers misses
    // (root, predicate key) -> highest subtype id read, so builtin_image_class_has_direct_supertype stops rescanning a JDK root's
    // subtypes on every fresh loader (10 % of Zs9Nd8ProxyCost define at 30 000 defines).
    E { group: Group::LOADER, token: "verify-name-answer-memo", on_key: Some("CRATONVM_VERIFY_NAME_ANSWER_MEMO"), off_key: None, off_word: None, since: "2026-10-23" },
    // JIT round 23 wave 6 (proxy-define page): perf, default ON, kill "0". The verifier's link loads
    // remember, per user-defined loader (per-VM ClassRealm table, pruned when the namespace empties),
    // "isolated URL loader" and "parent chain reaches the app loader"; the per-name check stays live.
    E { group: Group::LOADER, token: "loader-link-verdict-memo", on_key: Some("CRATONVM_LOADER_LINK_VERDICT_MEMO"), off_key: None, off_word: Some("0"), since: "2026-10-21" },
    // JIT round 23 wave 6 (proxy-define page): perf, default ON, kill "0". receiver_overrides_load_class
    // answers from a per-VM memo keyed by (receiver class id, overload), stamped with the hierarchy epochs.
    E { group: Group::LOADER, token: "loader-load-class-override-memo", on_key: Some("CRATONVM_LOADER_LOAD_CLASS_OVERRIDE_MEMO"), off_key: None, off_word: Some("0"), since: "2026-10-21" },
    // JIT round 23 wave 7 (proxy-define page): perf, OPT-IN "1" (neutral on the n23-w7 A/B). find_class_by_name_for_loader walks
    // its slash / dot keys without allocating them up front (same keys, same order).
    E { group: Group::LOADER, token: "class-by-name-no-alloc", on_key: Some("CRATONVM_CLASS_BY_NAME_NO_ALLOC"), off_key: None, off_word: None, since: "2026-10-22" },
    // JIT round 23 wave 2 (proxy-define page): perf, default ON, kill "0". Array.newArray (Array.newInstance,
    // Arrays.copyOf(.., Class)) maps the component mirror to its ClassId once, for the name and the allocation.
    E { group: Group::COMPAT, token: "reflect-new-array-one-resolve", on_key: Some("CRATONVM_REFLECT_NEW_ARRAY_ONE_RESOLVE"), off_key: None, off_word: Some("0"), since: "2026-10-21" },
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
    E { group: Group::IO, token: "net-jdk-backlog", on_key: Some("CRATONVM_NET_JDK_BACKLOG"), off_key: None, off_word: Some("0"), since: "2026-09-12" },
    E { group: Group::IO, token: "jar-archive-cache-cap", on_key: Some("CRATONVM_JAR_ARCHIVE_CACHE_CAP"), off_key: None, off_word: None, since: "2026-09-17" },
    E { group: Group::IO, token: "httpsrv-keepalive", on_key: Some("CRATONVM_HTTPSRV_KEEPALIVE"), off_key: None, off_word: Some("0"), since: "2026-09-12" },
    E { group: Group::IO, token: "socket-capture", on_key: Some("CRATONVM_SOCKET_CAPTURE"), off_key: None, off_word: None, since: "2026-06-12" },
    E { group: Group::IO, token: "zip-max-entry-bytes", on_key: Some("CRATONVM_ZIP_MAX_ENTRY_BYTES"), off_key: None, off_word: None, since: "2026-05-22" },
    // `ThreadMXBean.getLockedSynchronizers` support; DEFAULT-ON, so a kill
    // switch. THREADS rather than a JMX group because the group vocabulary
    // has no JMX and this is a threading capability the bean exposes.
    E { group: Group::THREADS, token: "jmx-owned-synchronizers", on_key: Some("CRATONVM_JMX_OWNED_SYNCHRONIZERS"), off_key: None, off_word: Some("0"), since: "2026-08-27" },
    // JIT round 19 proposal wave 9 (lane gc9, GC819-3): a caller-held synchronized body names its own method monitor; opt-in (no probe shape shows a Java-visible difference; kept OFF at round close).
    E { group: Group::THREADS, token: "jmx-caller-held-method-monitor-depths", on_key: Some("CRATONVM_JMX_CALLER_HELD_METHOD_MONITOR_DEPTHS"), off_key: None, off_word: None, since: "2026-10-09" },
    // JIT round 19 proposal wave 12 (lane gc12): another RUNNING thread's ThreadInfo reports the monitors it held at the instant its trace was published by the frame-trace pause (recorded in that pause), not at the reader's earlier lock-stack read (HotSpot parity, default ON; `=0` kill switch).
    E { group: Group::THREADS, token: "jmx-running-locks-at-trace", on_key: Some("CRATONVM_JMX_RUNNING_LOCKS_AT_TRACE"), off_key: None, off_word: Some("0"), since: "2026-10-09" },
    // gci i44/sp: default ON (VMManagementImpl safepoint counters read the STW barrier), =0 kills.
    // gci i46/lg: default ON (a pause with no HotSpot VM operation -- a handshake, a part of another operation -- adds nothing to the VMManagementImpl safepoint counters), =0 kills.
    // gci i46/hc: default ON (the platform MemoryMXBean is ManagementFactoryHelper's singleton, as HotSpot), =0 mints the VM's own MemoryImpl.
    // gci i46/hc: default ON (--compatible getGarbageCollectorMXBeans() in HotSpot's HashMap order), =0 keeps MemoryImpl's manager order.
    // gci i46/hc: default ON (bean names and GC notification strings built fresh, as HotSpot's create_from_str), =0 interns them.
    // 2026-09-08. Default-ON kill switch over the uncontended monitorenter /
    // monitorexit fast path (peek-first opcode operand, per-thread cached JMX
    // monitor book). `0` restores the previous path in the SAME binary, which is
    // what `probes/SyncCost.java` needs: a sequential pair of builds on a shared
    // host is not a measurement. Behaviour is identical either way --
    // `probes/JmxMonitorOwnership.java` is green on both.
    E { group: Group::THREADS, token: "monitor-fastpath", on_key: Some("CRATONVM_MONITOR_FASTPATH"), off_key: None, off_word: Some("0"), since: "2026-09-08" },
    E { group: Group::THREADS, token: "monitor-spin-handover-abort", on_key: Some("CRATONVM_MONITOR_SPIN_HANDOVER_ABORT"), off_key: None, off_word: None, since: "2026-09-29" },
    // gen r4w5/thrash5 (2026-09-24). Default-ON kill switch over the bounded
    // spin a contended `monitorenter` takes before it parks (and retires its
    // TLAB); `0` restores the park-at-once path
    // (`threading::monitor::monitor_enter_spin_enabled`).
    E { group: Group::THREADS, token: "monitor-enter-spin", on_key: Some("CRATONVM_MONITOR_ENTER_SPIN"), off_key: None, off_word: Some("0"), since: "2026-09-24" },
    E { group: Group::THREADS, token: "monitor-spin-after-wake", on_key: Some("CRATONVM_MONITOR_SPIN_AFTER_WAKE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::THREADS, token: "monitor-thin-notify", on_key: Some("CRATONVM_MONITOR_THIN_NOTIFY"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    E { group: Group::THREADS, token: "monitor-index-prune", on_key: Some("CRATONVM_MONITOR_INDEX_PRUNE"), off_key: None, off_word: Some("0"), since: "2026-09-26" },
    // gcg g3/k (opt-in): the lock-key registry is sharded by identity hash
    // (`native-builtins/src/lock_key_shards.rs`, read once per process).
    E { group: Group::THREADS, token: "lock-key-shards", on_key: Some("CRATONVM_LOCK_KEY_SHARDS"), off_key: None, off_word: Some("0"), since: "2026-10-01" },
    // `ThreadLocal` / `InheritableThreadLocal` reviewed Intrinsics (a Rust side
    // table): DEFAULT depends on the policy (on under `--compatible`, off under
    // `--real-jdk`, where the JDK's own `ThreadLocalMap` bytecode runs), so this is
    // a set-either-way override: `1` registers them, `0` does not, in either mode.
    E { group: Group::THREADS, token: "tl-intrinsic", on_key: Some("CRATONVM_TL_INTRINSIC"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    // gch h3/jx (gch-h2jr-compat-unstarted-thread-inherited-values-outlive-their-thread-by-collections):
    // a never-started Thread's queued inherited ThreadLocal values are held by the pending table and
    // pinned to the Thread (`metadata_pin`, native-root row `inherited-tl-pending`) instead of being
    // JNI global roots. Default ON since gch h4/fl (`=0` restores the JNI-global values); reached only from the `--compatible` Thread constructor shims.
    // SBR round 2, wave 8 (lane `tlocal`). DEFAULT ON under `--real-jdk`:
    // `ThreadLocal.get()` / `set(Object)` are reviewed Intrinsics that read the REAL
    // `Thread.threadLocals` table for the hot found-key case and hand every other
    // case to the JDK's own `get(Thread)` / `set(Thread, Object)` bytecode; they are
    // also what the JIT's thin direct helpers are admitted in front of.
    // `0` turns the fast path off (the JDK bytecode runs alone). Read at
    // registration; never registered under `--compatible`.
    E { group: Group::THREADS, token: "tl-real-fast", on_key: Some("CRATONVM_TL_REAL_FAST"), off_key: None, off_word: Some("0"), since: "2026-09-30" },

    // JIT round 21 lane tl2 (docs/feature-designs/native-dispatch-architecture.md): the real-field ThreadLocal get/set natives read their resolved slots with the declared descriptor byte (NativeContext::get_field_typed), skipping the per-read descriptor lookup. Default ON, kill "0".
    // SBR round 2 (lane `thread`). OPT-IN: claim `Reference.refersTo` /
    // `refersTo0` as LEAF natives (a field read and a compare, so no native-call
    // funnel). No gain measured on the local Windows box, kept for a Linux A/B.
    E { group: Group::THREADS, token: "refers-to-leaf", on_key: Some("CRATONVM_REFERS_TO_LEAF"), off_key: None, off_word: None, since: "2026-09-30" },
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
    // Interpreter round i6 wave 134, lane L4 (review of w133, item 4): `execute`'s
    // native stack guard returns the same lazy StackOverflowError as the
    // `EXEC_DEPTH` trip next to it (converted by the caller's loop) instead of
    // building it in place below the floor (parity, default ON, `=0` in place).
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
    // JIT round 16 wave 8 lane monitor: the contended-entry starvation guard
    // (a parked entrant that waited 4 ms reserves the monitor; read by the VM
    // and by the JIT's inflated arm). Opt-in.
    E { group: Group::THREADS, token: "monitor-heir", on_key: Some("CRATONVM_MONITOR_HEIR"), off_key: None, off_word: Some("0"), since: "2026-10-01" },
    // JIT round 16 wave 10 lane monitor2 (MON16-7): with `monitor-heir`, only
    // the longest-parked entrant may reserve the monitor (ordered), and the
    // heir parks on a condvar of its own that a release owed to it signals
    // alone (signal). Opt-in, unmeasured.
    E { group: Group::THREADS, token: "monitor-heir-ordered", on_key: Some("CRATONVM_MONITOR_HEIR_ORDERED"), off_key: None, off_word: None, since: "2026-10-02" },
    E { group: Group::THREADS, token: "monitor-heir-signal", on_key: Some("CRATONVM_MONITOR_HEIR_SIGNAL"), off_key: None, off_word: None, since: "2026-10-02" },
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
    // `ClassLoader.getResource` / `Class.getResource` stopping at the first
    // matching classpath entry, default-ON. `0` restores the whole-list walk
    // that built every matching URL and returned element 0. The two are
    // required to answer identically, so the flag can only change how much of
    // the classpath was touched — which makes it the same-binary A/B for that
    // cost.
    // Take the elision decision, then rebuild anyway and compare, panicking
    // on divergence. Turns the soundness claim into something measured
    // rather than argued; expensive, so default-OFF.
    // `org.jboss.logmanager.LogContextInitializer` consultation at JBoss
    // logger-node creation, default-ON. `0` restores the previous behaviour:
    // no provider is ever asked, so every node is born with no initial
    // handlers and no initial level. It is the one place in the logging
    // natives that runs APPLICATION bytecode from inside a logger allocator,
    // and WildFly, Keycloak and Quarkus all reach it — the switch is what
    // makes "is this the initializer?" a same-binary question.
    // Default-ON, and off for the *exact* untrimmed string `0` only — the
    // `!matches!(…, Ok("0"))` at `logmanager::jboss_logger_level_filter`.
    E { group: Group::COMPAT, token: "jboss-mp-root", on_key: Some("CRATONVM_JBOSS_MP_ROOT"), off_key: None, off_word: None, since: "2026-05-20" },
    // JIT round 18 wave 11, lane mhresid (MHRESID18-1): the compiled-first MethodHandle site link
    // runs with the constant bind's switches off too. Correctness, default ON.
    // nio21 wave 1 L1: the per-thread printed-lines buffer drops its oldest half at 65 536 lines. Default ON, kill "0".
    // nio21 wave 7 L3: the Unsafe natives test the array arm before the synthetic-offset arm. Default ON, kill "0".
    // zs s2/nd2: a throwable raised before main is entered (main class <clinit>, instance-main receiver allocation / constructor) is reported as java.c's ExceptionDescribe does: `Exception in thread "<name>" ` from the VM, then printStackTrace() with whatever it throws discarded (HotSpot 25 prints only the prefix on a full heap). Since the zs/dev merge (zt t1/ob text fix): unset or any value but an off-word = on for every collector; "0" / "false" / "off" / "no" = off (the uncaught dispatch).
    // nio21 wave 18 L5: where --real-jdk runs initPhase1's tail (Finalizer init, startThreads, VM.initLevel(1)) under CRATONVM_INIT_FINALIZER_BEFORE_PHASE1: unset/"layer" after the boot layer is built, "phase1" right after the fallback (diagnostic), "0"/"early" the wave-79 order.
    E { group: Group::THREADS, token: "init-finalizer-tail", on_key: Some("CRATONVM_INIT_FINALIZER_TAIL"), off_key: None, off_word: None, since: "2026-10-09" },
    // Interpreter round i5 wave 100 (lane L1): the quiet blocking-region leave builds its key into a recycled per-thread list and compares it streamed (no allocation); the comparison stays exact. Default OFF until the A/B medians.
    E { group: Group::THREADS, token: "blocking-leave-key-reuses-buffers", on_key: Some("CRATONVM_BLOCKING_LEAVE_KEY_REUSES_BUFFERS"), off_key: None, off_word: Some("0"), since: "2026-10-15" },
    // nio21 wave 16 L2: the cached lambda implementation row keeps its JitCache hit stamped with the cache generation. Default ON, kill "0".
    // nio21 wave 16 L2: a recognised lambda site enters its compiled implementation through a per-thread site cache before the general dispatch. Default ON, kill "0".
    // nio21 wave 16 L2: the interpreted lambda door enters a compiled implementation itself. Default ON, kill "0".
    // nio21 wave 16 L2: a compiled lambda implementation that declares an exception table is entered compiled (its own table routes a throw). Default ON, kill "0".
    // nio21 wave 15 L5: a refused cached lambda implementation template is remembered per (vm, proxy, receiver class) until the next redefinition. Default ON, kill "0".
    // nio21 wave 15 L5: a per-thread memo of class-pair passes of the lambda argument cast verdict (never per-object arms or failures). Default ON, kill "0".
    // nio21 wave 12 L1: lambda proxies past MAX_LAMBDA_PROXIES (100 000) are still registered (rows go with their host class). Default ON, kill "0" restores the cap.
    // nio21 wave 12 L5: the --compatible printStackTrace walker prints each header through the throwable's own toString() and lets an exception it throws leave printStackTrace (HotSpot s.println(this)). Default ON, kill "0".
    // Legacy opt-in to native forwarding to Pattern.compile's own bytecode; each call constructs
    // a fresh JDK Pattern with the caller's original String, including with =1.
    // r23 wave 4, lane L5: perf, default ON since wave 5 (R23-W5-FLIP; split("\\s+") 25-29 % faster on n23-w4,
    // facts equal), kill "0". A real-JDK registry without the compile cache registers no Pattern.compile row, so
    // dispatch reaches the JDK body (native-builtins `pattern_compile_yields_to_bytecode`).
    // r23 wave 2, lane L5: perf, default ON, kill "0". The --compatible StringBuilder codePointAt / codePointBefore /
    // codePointCount natives read only the units the answer needs, not the whole builder (native-builtins
    // `sb_code_point_window_enabled`).
    // r23 wave 2, lane L5: perf, default ON, kill "0". String.codePointCount over a short span and
    // offsetByCodePoints by a short walk read their units one at a time instead of decoding the whole String
    // (native-builtins `string_code_point_window_enabled`).
    // r23 wave 2, lane L6 (page `r23-w2-native-string-formatter-exceptions-lose-jdk-frames-FIXED-20261021.md`): HotSpot parity,
    // default ON, kill "0". The String codePointAt / codePointCount / offsetByCodePoints natives hand a failing call to the
    // JDK's own body (`invoke_special_bytecode_only`), so the exception carries the Preconditions / Character frames
    // (native-builtins `lang_string::string_error_from_jdk_body`).
    // nio21 wave 6 L3: a cache-line-crossing Unsafe CAS takes a split lock as HotSpot does. Default ON since JIT round 20 (the process-lock arm lost an update: R20AtomSplitLock); kill "0" for a split_lock_detect=fatal host (not atomic against a plain store into a byte the update changes).
    // nio21 wave 6 L3: a misaligned in-line (and 2-byte) Unsafe CAS is a locked instruction instead of the process lock. Default ON, kill "0".
    // nio21 wave 6 L3: Unsafe CAS on a reference-array element (and a narrow reference field) is a hardware LOCK CMPXCHG with the store barriers. Default ON, kill "0".
    // nio21 wave 6 L1: the StringLatin1 compareTo natives (getChar has no native since 2026-10-13; and, since 2026-10-13, the four ArraysSupport.mismatch natives) throw HotSpot's NPE / AIOOBE (with its messages) on a null or short array. Default ON, kill "0".
    // JIT round 20 wave 11, lane pNat (W5L4-6): the same retirement for StringLatin1.toLowerCase(String,byte[],Locale); the JIT's single-pass toLowerCase ladder binds no thin helper while it is on. Default ON, kill "0".
    // nio21 wave 5 L3: Unsafe CAS on a primitive-array element is a hardware atomic on the element address (was a locked read-compare-write). Default ON, kill "0".
    // nio21 wave 2 L4: AbstractOwnableSynchronizer.setExclusiveOwnerThread keeps its owner-index intrinsic (getLockedSynchronizers); "0" leaves the bytecode alone.
    E { group: Group::COMPAT, token: "stack-trace-module-prefix", on_key: Some("CRATONVM_STACK_TRACE_MODULE_PREFIX"), off_key: None, off_word: Some("0"), since: "2026-09-28" },
    E { group: Group::COMPAT, token: "stack-trace-element-exact-class", on_key: Some("CRATONVM_STACK_TRACE_ELEMENT_EXACT_CLASS"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "stack-trace-element-full-origin", on_key: Some("CRATONVM_STACK_TRACE_ELEMENT_FULL_ORIGIN"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    E { group: Group::COMPAT, token: "stack-trace-binary-names", on_key: Some("CRATONVM_STACK_TRACE_BINARY_NAMES"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    // Interpreter round i2 wave 60, lane L3: `Reflection.getCallerClass()`
    // answers from the two innermost frames when it provably can.
    // Interpreter round i6 wave 121, lane L3: the fast arm above also under
    // reflective and served `Thread.run` calls two frames below the asker.
    E { group: Group::COMPAT, token: "throwable-thread-run-nested-tasks", on_key: Some("CRATONVM_THROWABLE_THREAD_RUN_NESTED_TASKS"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::COMPAT, token: "thread-run-thread-declared-fields", on_key: Some("CRATONVM_THREAD_RUN_THREAD_DECLARED_FIELDS"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::COMPAT, token: "thread-layout-cache", on_key: Some("CRATONVM_THREAD_LAYOUT_CACHE"), off_key: None, off_word: Some("0"), since: "2026-09-30" },
    E { group: Group::COMPAT, token: "stack-trace-element-origin-memo", on_key: Some("CRATONVM_STACK_TRACE_ELEMENT_ORIGIN_MEMO"), off_key: None, off_word: Some("0"), since: "2026-09-29" },
    // r23 wave 3, lane L6 (page `r23-w2-native-string-formatter-exceptions-lose-jdk-frames-FIXED-20261021.md`): HotSpot parity,
    // measured with `C:\craton\n23p\w3L6\FormatProbe.java` (stand-in vs JDK bytecode, both modes). With it, the
    // String.format / formatted / java.util.Formatter stand-ins are not registered and the JDK's bodies run, so format
    // errors carry the Formatter frames (native-builtins `jdk_formatter_bodies_enabled`).
    // R23-W4-FLIP: default ON since r23 wave 4 (A/B on n23-w3: HotSpot-equal FormatProbe in both modes, 4-6x faster),
    // kill "0".
    // `array-autobox-latch` — off restores the unconditional `autobox_payload`
    // probe on every non-null reference-ARRAY element read. That probe opens
    // with `is_object_address` and can only answer `Some` when a wrapper
    // exists, which is what the latch records; the FIELD read paths have been
    // latched since the latch was introduced and the array ones were not.
    // Default-ON, off for the exact untrimmed string `0` only — the
    // `!matches!(…, Ok("0"))` at `vm_exec::vh_strict_reference_return`. Same
    // shape as `mh-strict-invokeexact` above and deliberately a SEPARATE knob:
    // the two rules fire on disjoint method names and share only their funnel,
    // so one going wrong in the field must not force the other off.
    // Default-ON, off for the exact string `0` only -- the
    // `!matches!(…, Ok("0"))` at `locale_resources::cldr_locale_fidelity_enabled`.
    // r16 locale lane: CLDR candidate chain (script, zh/nb/no, irregular
    // parents), the `-u-nu-` numbering system, and `findNonFormatChar` + the
    // `*Text` symbol strings in the --compatible `DecimalFormatSymbols.initialize`.
    // Default-ON, off for the exact string `0` only -- read at
    // `vm_init::linux_parselocale_enabled` and `system_bootstrap::native_platform_properties`.
    // r16 locale lane (b): Linux `user.*` locale keys from `java_props_md.c`'s
    // `ParseLocale` (C.UTF-8 -> `en`, no country; NULL subtags unpublished).
    // Round 16 wave 13 lane wmte: a door's `WrongMethodTypeException` lists the
    // JDK frames HotSpot raises it in. Correctness, DEFAULT-ON, so a KILL SWITCH.
    // Round 16 wave 7, lane varhandle: the interpreter's signature-polymorphic
    // site cache serves every `VarHandle` site again (the round-15 merge kept
    // three-parameter sites -- instance compareAndSet / compareAndExchange --
    // off it, ~7x under --nojit). Performance restoration, DEFAULT-ON, kill
    // switch. `vh-interp-cas-fast` (MH16-6) default ON since wave 7 (-31..-36% CAS under --nojit), kill switch.
    E { group: Group::LOADER, token: "real-builtin-loaders", on_key: Some("CRATONVM_REAL_BUILTIN_LOADERS"), off_key: None, off_word: Some("0"), since: "2026-10-02" },
    // JIT round 16 wave 7, lane arraylist: default ON (kill switch "0"); see
    // native-collections `iterator_interface_shim_kind`.
    // JIT round 16 wave 8, lane fieldsite: both default OFF. The real
    // `ArrayList$Itr` skips the synthetic slot floor and gets a compact layout
    // (classloading `floor_is_synthetic_only`); SA6-1, the interpreter's
    // trivial-getter fast path asks the per-thread field site first
    // (`dispatch_virtual::try_execute_cached_trivial_instance_getter`).
    // JIT round 16 wave 11, lane overpad: both default OFF. The real `ArrayList$ListItr` (with the
    // `Itr` switch) and `StringBuilder` / `StringBuffer` skip the synthetic slot floor and get a
    // compact layout (classloading `floor_is_synthetic_only`).
    // JIT round 23 wave 8, lane W8-L2: default ON (perf; kill switch "0"). The real
    // `CompletableFuture` skips the synthetic slot floor and gets its compact two-field layout
    // (classloading `floor_is_synthetic_only`), so both JIT tiers can inline `RESULT` / `STACK`.
    // JIT round 16 wave 13, lane fieldsite2; default ON since wave 17 (lane fieldsite3, a layout
    // correctness fix; kill switch "0"). Caps the synthetic slot floor of a real
    // class at its stub's own extent (classloading `synthetic_floor_capped_at_stub_extent`); the
    // `Reference` family and the `FLOOR_EXEMPT_CLASSES` rows are never moved by it. The DBG row is
    // OP16-2: a native read or write of a padded (descriptor-less) slot, per class and slot
    // (vm `NativeContextImpl::get_field` / `set_field` / `set_field_raw`).
    // zt t34: the stub-extent cap also applies to the java/lang/ref/Reference family, whose define-time floor padded WeakReference / SoftReference / PhantomReference and their subclasses by two descriptor-less slots and so kept every live reference on the legacy 16-byte cell layout (the bulk of the ZGC R6 plan's legacy pass). Default ON; "0" restores the uncapped floor for that family.
    // JIT round 16 wave 15, lane nio: default ON since the r23 L8 heap-buffer close, kill "0" (performance and
    // memory, FS2-1); no own fabricated
    // table for `HeapByteBuffer` / `HeapCharBuffer`, five slots for `CharBuffer` (classloading
    // `heap_buffer_stub_narrow_enabled`). With `synthetic-floor-stub-extent` the real heap buffers
    // reach their real extents (compact layout).
    // JIT round 19 wave 2, lane funnel2 (NIO2-17-5): opt-in perf, default OFF.
    // `lang_string::is_plain_string` (String.valueOf(Object), builder append(Object) /
    // append(CharSequence) / insert) answers "is java/lang/String" from the class-facts
    // memo instead of allocating the class name per call.
    // JIT round 19 wave 3, lane funnel3 (NIO2-17-5 rungs 2-5 and candidate 5): opt-in
    // perf, default OFF, one row per rung. Each answers a per-call class-name compare
    // from the class-facts memo: the boxed-wrapper kind (String.valueOf(Object) /
    // append(Object), Boolean literal text); the primitive / array mirror tests
    // (Class.getModifiers, getClassLoader, isPrimitive fallback); Class.isInstance /
    // cast's AnnotationProxy / Proxy$Instance tests; Object.toString's MessageBytes,
    // PrintWriter autoflush and PrintStream UTF_8 tests; Objects.hashCode / hash's
    // value-hash kind.
    E { group: Group::JIT, token: "class-facts-memo-wrapper", on_key: Some("CRATONVM_CLASS_FACTS_MEMO_WRAPPER"), off_key: None, off_word: Some("0"), since: "2026-10-08" },
    E { group: Group::JIT, token: "class-facts-memo-mirror", on_key: Some("CRATONVM_CLASS_FACTS_MEMO_MIRROR"), off_key: None, off_word: None, since: "2026-10-08" },
    // JIT round 19 proposal round close (2026-10-09): default ON, `=0` restores the old path (A/B in the round record).
    E { group: Group::JIT, token: "class-facts-memo-proxy", on_key: Some("CRATONVM_CLASS_FACTS_MEMO_PROXY"), off_key: None, off_word: Some("0"), since: "2026-10-08" },
    E { group: Group::JIT, token: "class-facts-memo-carriers", on_key: Some("CRATONVM_CLASS_FACTS_MEMO_CARRIERS"), off_key: None, off_word: Some("0"), since: "2026-10-08" },
    E { group: Group::JIT, token: "class-facts-memo-value-hash", on_key: Some("CRATONVM_CLASS_FACTS_MEMO_VALUE_HASH"), off_key: None, off_word: None, since: "2026-10-08" },
    // JIT round 19 proposal wave 6, lane str6 (FUNNEL3-19-3): opt-in perf, default OFF, the sixth rung.
    // Class.isInstance / cast's false path reads the mirror's class name as the class store's Arc<str>
    // instead of a fresh String per call (native-builtins `lang_class`, `class_facts_memo::Rung::IsInstanceName`).
    E { group: Group::JIT, token: "class-facts-memo-isinstance-name", on_key: Some("CRATONVM_CLASS_FACTS_MEMO_ISINSTANCE_NAME"), off_key: None, off_word: Some("0"), since: "2026-10-08" },
    // JIT round 19 proposal wave 8, lane str8 (STR719-3): opt-in perf, default OFF. The by-name instanceof
    // fallback (vm `typecheck::synthetic_implements`) answers a target outside the shared table
    // (`cratonvm_types::synthetic_targets`) false before reading the object's name under the class-manager lock.
    // JIT round 19 proposal round close (2026-10-09): default ON, `=0` restores the old path (A/B in the round record).
    // JIT round 19 proposal wave 8, lane str8 (STR619-3): opt-in perf, default OFF, the seventh rung.
    // Class.isInstance / cast's false path skips the name rules when the mirror's class facts prove neither
    // can admit (not a synthetic_implements target, by-name rule inert) -- no name, no lock
    // (native-builtins `lang_class`, `class_facts_memo::Rung::IsInstanceFalse`).
    // JIT round 19 proposal round close (2026-10-09): default ON, `=0` restores the old path (A/B in the round record).
    E { group: Group::JIT, token: "class-facts-memo-isinstance-false", on_key: Some("CRATONVM_CLASS_FACTS_MEMO_ISINSTANCE_FALSE"), off_key: None, off_word: Some("0"), since: "2026-10-09" },
    // JIT round 20 wave 17, lane pRefl (handoff r20 "Round-21 performance targets", `R20PNatAssignable`):
    // perf, default ON, `=0` restores the name path. Class.isAssignableFrom's resolved pair decides its
    // `false` from the per-thread class-facts memo (arrays, primitive names, synthetic targets) instead of
    // two class-name reads under the class-manager lock (native-builtins `lang_class`,
    // `class_facts_memo::Rung::AssignableFalse`).
    E { group: Group::JIT, token: "class-facts-memo-assignable-false", on_key: Some("CRATONVM_CLASS_FACTS_MEMO_ASSIGNABLE_FALSE"), off_key: None, off_word: Some("0"), since: "2026-10-13" },
    // JIT round 20 wave 17, lane pRefl (handoff r20 "Round-21 performance targets", `R20PAllocEntryCheck`):
    // perf, default ON, `=0` restores the refresh. The signature-polymorphic linker door
    // (`lang_invoke::mh_linked_invoker`; every `invokeExact` of a real JDK handle, among them the one each
    // `Method.invoke` makes in `DirectMethodHandleAccessor.invokeImpl`) skips its full root-snapshot deposit
    // before the linker call when the site's linker pair came from the VM's table (no pin is read after the call).
    // JIT round 20 wave 19, lane pRefl (page jr20-reflective-method-invoke-runs-a-native-door-chain-per-call):
    // perf, default ON, `=0` keeps every compiled signature-polymorphic site on its native door. A compiled
    // `invokeExact` with a filed linker pair, `invokeBasic` on a real handle, and `linkToStatic/Virtual/Interface`
    // call their Java target as a compiled call (vm `jit/helpers.rs` `mh_compiled_link_redirect`).
    E { group: Group::JIT, token: "jit-mh-compiled-link", on_key: Some("CRATONVM_JIT_MH_COMPILED_LINK"), off_key: None, off_word: Some("0"), since: "2026-10-13" },
    // JIT round 21 lanes reflect / reflect2 (re-landed t1 wave 8 lane RL-REFL after merge `624fae0b49`
    // dropped the code; ON as host-verified in the round 21 summary record). PAIR_FIRST: the compiled
    // `invokeExact` link takes a filed `linker:` row (which the door files only for a shareable descriptor)
    // as the proof instead of re-parsing the site descriptor per call; `=0` asks the parse first.
    // LEAN: the compiled signature-polymorphic link checks basic-type agreement without allocating, takes a
    // memoised `MemberName` decode without the class-name check, and builds the `invokeExact` linker key in
    // one `String`, per `Method.invoke`. Default ON; `=0` off.
    // Interpreter round i5 wave 109, lane L2 (performance, opt-in): a compiled `linkToSpecial` calls its
    // member as a compiled `invokespecial`, and a constant `invokeExact` site binds a real `$Special`
    // handle or a plain private-method one to that call (`lang_invoke::mh_compiled_bind_special_target`).
    // Interpreter round i6 wave 116, lane L2 (performance, opt-in): a compiled `invokeBasic` whose
    // vmentry is a hidden (spun) LambdaForm calls its published compiled body by class id
    // (no deopt points, no exception table) instead of the native door's re-entry.
    // Interpreter round i6 wave 122, lane L2 (performance, opt-in): the by-id admission above
    // for a compiled `invokeExact` site's linker whose `MemberName` is a hidden spun form.
    E { group: Group::JIT, token: "jit-mh-compiled-link-linker-by-id", on_key: Some("CRATONVM_JIT_MH_COMPILED_LINK_LINKER_BY_ID"), off_key: None, off_word: None, since: "2026-12-24" },
    // Interpreter round i5 wave 103, lane L4 (performance, opt-in): a compiled `invokeExact`
    // site naming an application class is bound to the linker from the door's basic-member
    // and site-type rows (`lang_invoke::mh_compiled_linker_user_site`).
    E { group: Group::JIT, token: "jit-mh-compiled-link-user-site", on_key: Some("CRATONVM_JIT_MH_COMPILED_LINK_USER_SITE"), off_key: None, off_word: None, since: "2026-12-05" },
    // r21 reflect (JIT round 21 wave 1, lane reflect; probe `R21ReflectInvoke.java`):
    // perf, opt-in. The compiled `invokeExact` site of `DirectMethodHandleAccessor.invokeImpl` calls the
    // reflected method as an ordinary compiled call (exact-wrapper unboxing, `Integer.valueOf` / void / reference
    // returns in the helper) instead of the linker chain and its spun adapter form (vm `jit/helpers.rs`
    // `reflect_accessor_link_redirect`; reader `lang_invoke::jit_reflect_accessor_link_enabled`).
    // r21 reflect: correctness, default ON, kill "0". A reflective call of `MethodHandle.invoke` /
    // `invokeExact` or a `VarHandle` access-mode method (JDK 25's `NativeAccessor.invoke0`) raises HotSpot's
    // `UnsupportedOperationException` "... cannot be invoked reflectively" inside `InvocationTargetException`
    // (native-builtins `lang_class::reflective_signature_polymorphic_refusal`; reader `NativeFlags::reflect_sig_poly_uoe`).
    // r21 reflect: correctness, default ON, kill "0". `NativeAccessor.invoke0` raises `JVM_InvokeMethod`'s own
    // check texts (message-less NPE, "object is not an instance of declaring class", "wrong number of arguments")
    // (native-builtins `lang_class::native_method_invoke_body`; reader `NativeFlags::reflect_invoke0_vm_texts`).
    // r21 reflect (review item 3): correctness, default ON, kill "0". The `Method.invoke` door answers
    // `JVM_InvokeMethod`'s texts for methods JDK 25's `useNativeAccessor` sends to `NativeAccessor`
    // (native-builtins `lang_class::method_takes_native_accessor`; reader `NativeFlags::reflect_door_native_accessor_texts`).
    // r21 reflect2: correctness, default ON, kill "0". `NativeAccessor.invoke0` / `newInstance0` raise
    // `Reflection::invoke`'s argument refusals (message-less IAE for a null to a primitive, cause-less
    // "argument type mismatch") and `newInstance0` "wrong number of arguments"
    // (native-builtins `lang_class::native_accessor_argument_refusal`; reader `NativeFlags::reflect_native_accessor_arg_texts`).
    // r21 reflect2: correctness, default ON, kill "0". The `Method.invoke` / `Constructor.newInstance` doors
    // honour `-Djdk.reflect.useNativeAccessorOnly=true` and answer the native accessor's texts for every member
    // (native-builtins `lang_class::door_honours_native_accessor_only`; reader `NativeFlags::reflect_door_native_accessor_only`).
    // r21 reflect: perf, opt-in. With `jit-mh-compiled-link`, a compiled `invokeBasic` / linked `invokeExact` /
    // `linkToStatic` whose target is a static member of an initialized HIDDEN class (a spun `LambdaForm` form)
    // calls it by its owner's id (`interpreter::execute`, frames real) instead of the native door (vm
    // `jit/helpers.rs` `mh_link_leaf_static`; reader `lang_invoke::jit_mh_compiled_link_hidden_enabled`).
    // r21 reflect: perf, opt-in. The leaf `Class.getName()` answers from a lock-free per-thread memo keyed by
    // (VM, collection address epoch, mirror address), guarded by the mirror's `name` slot, for every mirror
    // including arrays and primitives (native-builtins `lang_class::class_get_name_mirror_memo_hit`).
    // r21 reflect (REFLECT21-3): perf, opt-in. The compiled `invokeExact` site of
    // `DirectConstructorHandleAccessor.invokeImpl` allocates the instance and calls its `<init>` as a compiled
    // call (vm `jit/helpers.rs` `reflect_ctor_link_redirect`; reader `lang_invoke::jit_reflect_ctor_link_enabled`).
    // r21 reflect (wave 8; page `docs/feature-designs/real-jdk-mode.md`):
    // correctness, both modes, default ON, kill "0". `linkToSpecial` of a `<init>` member (the constructor call of every
    // constructor MethodHandle, so of every JDK 25 Constructor.newInstance) registers a finalizable receiver, as HotSpot's
    // Object.<init> return does (native-builtins `link_to_special_constructor_registers`; reader `NativeFlags::mh_ctor_registers_finalizable`).
    // r21 reflect (wave 8, same page): correctness, both modes, default ON, kill "0". The constructor-less native allocations
    // (Unsafe.allocateInstance, instantiate(NoConstructor), allocate_instance) note a java.lang.ref subclass for discovery at
    // allocation, so a reflectively constructed WeakReference subclass is cleared and enqueued (vm
    // `gc_and_alloc::init_no_constructor_instance`; reader `VmConfig::native_alloc_notes_references`, read once per VM).
    // r21 jni (wave 8, same page, "JNI residual"): correctness, both modes, default ON, kill "0". A JNI
    // Call[Nonvirtual]<T>Method* of a `<init>` methodID (the AllocObject + CallNonvirtualVoidMethod(<init>) idiom) registers
    // a finalizable receiver before the constructor runs, as HotSpot's Object.<init> return does (vm `native::jni`
    // `jni_constructor_call_registers`; read only for a `<init>` call). JNI AllocObject also notes a java.lang.ref subclass
    // at allocation now, under `CRATONVM_NATIVE_ALLOC_NOTES_REFERENCES` above (`init_no_constructor_instance`).
    // r21 jni (wave 8): correctness, both modes, default ON, kill "0". CallVoidMethod(obj, superCtorID) runs exactly that
    // constructor (HotSpot: `<init>` has no vtable index, so the virtual call selects the resolved method), not the
    // receiver class's same-descriptor `<init>` (vm `native::jni` `jni_call_instance`; read only for a `<init>` whose
    // declaring class is not the receiver's).
    // r21 jni (wave 10, page `r21-review-w68-native-findings-20261019` item 5): correctness, both modes, default ON, kill
    // "0". Call<T>Method[V|A](obj, mid) runs exactly the methodID's method when it is private, final or declared by a final
    // class (HotSpot `jni_invoke_nonstatic`, `can_be_statically_bound`), not the receiver class's same-signature method (vm
    // `native::jni` `jni_call_instance` / `jni_mid_is_statically_bound`; reader `VmConfig::jni_call_statically_bound_exact_mid`, read once per VM).
    // r21 jni (wave 10, same item): correctness, both modes, default ON, kill "0". Any other inherited methodID is selected
    // against its own declaration (JVMS 5.4.5 / 5.4.6, `invoke_on_receiver_class_selecting`), so a cross-package
    // package-private superclass method is not run as the receiver's same-signature method (vm `native::jni`
    // `jni_call_instance`; reader `VmConfig::jni_call_virtual_selects`, read once per VM).
    // r21 finalize (wave 10, page `r21-review-w68-native-findings-20261019` item 4): correctness, both modes, default ON,
    // kill "0". A finalizable object whose constructor provably threw before any `<init>` call on it completed (so before
    // Object.<init> returned, where HotSpot registers) has its early registration withdrawn: at the interpreted creator
    // frame, or by the native door that allocated it (MH linkToSpecial, JNI NewObject / `<init>` call, NativeContext
    // constructions); since wave 11 at the walk's entry frame too, whoever the creator is (compiled code, a JIT helper)
    // (vm `interpreter::exception_dispatch` `unconstructed_mark_of`; reader `finalizer_register_at_object_init_enabled`,
    // read only on a finalizable receiver).
    // r21 finalize (wave 11, page `r21-finalize-compiled-ctor-throw-keeps-registration-20261019`): correctness, both
    // modes, default ON, kill "0". A constructor of a finalizable class that can throw before its `this(..)` /
    // `super(..)` call is never compiled, bound or spliced, so that throw always leaves an interpreted frame the
    // withdrawal above can prove (vm `interpreter::exception_dispatch::finalizer_bars_compiling`; reader
    // `finalizer_ctor_stays_interpreted_enabled`, read only for a constructor of a finalizable class).
    // r21 finalize (wave 13, same page, residual 1): correctness, both modes, default ON, kill "0". The same rule for a
    // constructor of a class WITHOUT a finalizer that a loaded finalizable subclass reaches by `super(..)`; the define of
    // a finalizable class withdraws its ancestors' constructor bodies and their splices (vm
    // `interpreter::exception_dispatch::finalizer_bars_compiling` / `finalizable_define_stale_ancestors`, jit
    // `JitCache::invalidate_for_finalizable_subtype`; reader `VmConfig::finalizer_ancestor_ctor_stays_interpreted`, read
    // once per VM, as are the wave-10 and wave-11 switches above since wave 13).

    // JIT round 21 lane reflect (page jr21-reflective-invoke-and-mcs-flip-handoff-rows): perf, default ON,
    // `=0` re-checks the site descriptor's shareability (a descriptor parse and a class lookup per
    // component) before the linker-table probe on every compiled `invokeExact` link; ON, the filed row
    // (which the door files only for a shareable descriptor) is the proof.
    // JIT round 21 lane reflect2 (page jr21-reflect2-method-invoke-per-call-link-work): perf, default ON,
    // `=0` off. The compiled signature-polymorphic link checks basic-type agreement without allocating, takes
    // a memoised `MemberName` decode without the class-name check, and builds the `invokeExact` linker key
    // in one `String`, per `Method.invoke`.
    // JIT round 21 lane reflect3 (page jr21-reflect3-method-invoke-steady-state-string-work): perf, default ON,
    // `=0` off. The compiled signature-polymorphic link's basic-type agreement is memoised on the member's
    // per-epoch decode record (two descriptor parses per shape per `Method.invoke` before).
    E { group: Group::JIT, token: "jit-mh-compiled-link-agree-memo", on_key: Some("CRATONVM_JIT_MH_COMPILED_LINK_AGREE_MEMO"), off_key: None, off_word: None, since: "2026-10-14" },
    // JIT round 21 lane reflect3 (same page; REFLECT2-21-4): perf, default ON, `=0` off. A compiled
    // `invokeExact` link's linker pair is memoised on the handle's per-epoch record (a key `String` and a
    // locked table probe per `Method.invoke` before).
    E { group: Group::JIT, token: "jit-mh-compiled-link-pair-memo", on_key: Some("CRATONVM_JIT_MH_COMPILED_LINK_PAIR_MEMO"), off_key: None, off_word: None, since: "2026-10-14" },
    // JIT round 21 lane reflect3 (page jr21-reflect3-forname-builtin-loader-runs-loadclass-interpreted): perf,
    // default ON, `=0` off. `--real-jdk` `Class.forName` with the application or platform loader takes that
    // loader's `findLoadedClass0` hit as the answer (HotSpot's dictionary hit) instead of running
    // `loadClass` -> `loadClassOrNull` -> `findLoadedClass` as interpreted bytecode per call.
    // JIT round 21 lane reflect4 (REFLECT4-21-1, page jr21-reflect3-bytecode-only-fallback-bodies-always-interpret):
    // opt-in. Round 21's door-invisible form: a reviewed `Intrinsic` that runs its own JDK body through the
    // bytecode-only doors enters a compiled body of it published under a key no ordinary caller looks up
    // (vm `runtime/interpreter/bytecode_only_body.rs`). Dev's re-land of the same item
    // (`CRATONVM_JIT_BYTECODE_ONLY_BODY`, default ON: the method is marked re-entry-only) is the default;
    // this form kept its own switch so the two never both compile the same body.
    E { group: Group::JIT, token: "jit-bytecode-only-key", on_key: Some("CRATONVM_JIT_BYTECODE_ONLY_KEY"), off_key: None, off_word: Some("0"), since: "2026-10-27" },
    // t1 wave 12, lane I24J: correctness, both modes, default ON, kill "0". EVERY constructor of a finalizable class
    // stays interpreted while the class or a superclass declares one that can throw before its initializing call, so a
    // compiled creator's retried `<init>` returns through the interpreter and re-registers the object withdrawn at the
    // walk's entry (vm `interpreter::exception_dispatch::finalizer_retry_ctor_stays_interpreted_enabled`; read only on the
    // bar's cold path for a constructor of a finalizable class).
    // JIT round 17 wave 8, lane nio3: opt-in perf, default OFF. `NativeContext::class_id_from_mirror`
    // answers from a per-thread, per-VM memo keyed by (vm_identity, GC pause generation, mirror
    // address) instead of the `class_mirrors_reverse` RwLock (native-builtins `mirror_id_memo`).
    // JIT round 18 wave 10, lane buffers: opt-in perf, default OFF. `NativeContext::class_id_from_mirror`
    // answers from a per-VM, lock-free, GC-generation-validated table shared by every thread
    // (native-builtins `mirror_id_memo::MirrorIdTable`, held by `ClassRealm`); read once per VM.
    // JIT round 19 wave 1, lane classshape (BUF18-6): perf, default ON since the 2026-10-07 A/B
    // (`=0` restores the natives). Over a JDK 25+ image
    // `Class.isArray` / `isInterface` / `isPrimitive` are not registered, so JDK 25's field-reading
    // bodies answer and the IR tier splices them (native-builtins `lang_class::class_shape_jdk_body`).
    E { group: Group::JIT, token: "class-shape-jdk-body", on_key: Some("CRATONVM_CLASS_SHAPE_JDK_BODY"), off_key: None, off_word: Some("0"), since: "2026-10-06" },
    // Lane classshape's CLASSSHAPE19-1: perf, default ON (`=0` restores the natives). Over a JDK 25+
    // image `Class.getComponentType` / `componentType` are not registered, so JDK 25's
    // `componentType`-reading bodies answer (native-builtins `lang_class::class_component_jdk_body`).
    E { group: Group::JIT, token: "class-component-jdk-body", on_key: Some("CRATONVM_CLASS_COMPONENT_JDK_BODY"), off_key: None, off_word: Some("0"), since: "2026-10-07" },
    // JIT round 18 wave 10, lane buffers: opt-in perf, default OFF. The IR splice planner visits
    // invoke sites whose next instruction is a `return` after every other site, so cold tail exits
    // stop spending the 24-site cap ahead of a loop's helpers (jit `ir_splice_order`).
    // JIT round 19, lane irops19 (R15B-31): opt-in perf, default OFF. The optimizing tier lowers
    // `Math.sqrt(D)`, FP `Math.min`/`max` and `Math.multiplyHigh`/`unsignedMultiplyHigh` as
    // `ScalarOp`s instead of refusing the calling method (jit `ir::ir_fp_math_ops_enabled`).
    // r23 wave 2, lane L8: perf, default ON, kill "0". The four FP `Math.min`/`max` rows are IR scalar intrinsics
    // without `ir-fp-math-ops` (Java-exact lowering; makes VEC21-5's FP reduction reachable) (jit `ir_fp_minmax_rows_enabled`).
    // Same lane (IN16-1): opt-in, default OFF. `String.valueOf` sites leave the planner's blanket
    // `java/lang/String` refusal and build as ordinary static calls (jit `ir_string_value_of_call_admits`).
    // Same lane (IRLOWER17-1 / OI-3): opt-in, default OFF. LICM hoists a plain `getstatic` the
    // lowering reads directly (initialised class) out of a loop with no call, static store,
    // allocation, monitor or volatile access (jit `ir_optimize::ir_licm_direct_static_enabled`).
    // JIT round 20 wave 11 (lane pLoop), default ON (`=0` kill): LOOPOPT20-4 of
    // `docs/feature-designs/jit-ir-optimizations.md` (= L5W7-3), a direct non-volatile
    // `getstatic` in a loop body is no hard barrier for LICM's field / array load hoist
    // (jit `ir_optimize::licm_static_read_not_barrier_enabled`).
    // JIT round 19 wave 2, lane irops2 (IROPS19-1): opt-in perf, default OFF. A direct non-volatile
    // `getstatic` / primitive `putstatic` is a plain read / write of its own `AliasClass::Static` cell in
    // the effect model, and the direct-static LICM refuses per cell (jit `ir::ir_static_alias_class_enabled`).
    // Same lane (IROPS19-2): opt-in, default OFF. `Math.floor/ceil/rint` (SSE4.1), `Math.fma` (FMA3) and
    // `Integer/Long.bitCount` (POPCNT) as IR `ScalarOp`s (jit `ir::ir_round_fma_bitcount_ops_enabled`).
    // Same lane (IROPS19-2): opt-in, default OFF. IR `numberOfLeading/TrailingZeros` as one LZCNT / TZCNT
    // where the host has it (jit `ir_lower::ir_lzcnt_tzcnt_enabled`).
    // Same lane (IROPS19-3): opt-in, default OFF. Range transfers and constant folds for IR scalar
    // intrinsics (jit `ir::ir_scalar_range_fold_enabled`).
    // Same lane (IROPS19-7): opt-in, default OFF. An IR OSR entry stub re-reads an int/long direct static
    // read the direct-static LICM hoisted (jit `ir_lower::ir_osr_remat_static_read_enabled`).
    // JIT round 19, lane splice19 (CH16-4): opt-in, default OFF. With the chain snapshots on, the
    // IR builder records one only at a spliced instruction a guard can be lowered at
    // (jit `ir::ir_chain_snapshots_lazy_enabled`); the DBG row prints the per-compile census.
    // Same lane (CH6W5-1): opt-in, default OFF. An unproven nested exact-receiver site splices
    // behind an exact class test with its call on the miss edge (jit `ir::ir_nested_guarded_splice_enabled`).
    // JIT round 19 wave 2, lane splice2 (SPLICE19-4): opt-in, default OFF, inert without
    // ir-nested-guarded-splice. A nested site whose callee profile names a bimorphic pair tests the
    // second class on the first's miss edge (jit `ir::ir_nested_bimorphic_splice_enabled`).
    // Same lane (SPLICE19-2 census): one line per IR-mode body the inline resolver refuses for its
    // invokedynamic, with how many of its indy sites have a constant direct target.
    // JIT round 19 wave 3, lane splice3 (SPLICE2-2 = TF15-1): opt-in, default OFF. A chain trap of an
    // implicit exception (null / bounds / div-zero) inside a splice escalates to a named innermost
    // entry instead of the call-site wildcard; the recompile keeps the splice and marks that guard
    // `ResumeSemantics::expected_throw`, whose trips the VM resumes without charging
    // (jit `ir::ir_splice_expected_throw_enabled`, vm `deopt_resume`).
    // Same lane (SPLICE2-5): opt-in, default OFF, inert without the post-call splice exits. The
    // nested guarded splice's miss call gets a post-call chain snapshot at the join's successor, with
    // the call's result in place of the join phi (jit `ir::ir_nested_miss_post_call_exit_enabled`).
    // JIT round 19 wave 4, lane splice4 (SPLICE3-1): opt-in, default OFF, inert without
    // ir-nested-guarded-splice. `nest-profiled` falls back to the class the callee's own optimizing
    // artifact guarded-spliced at the nested pc (`CompiledMethod::guarded_splice_class_at_bci`) when
    // neither the callee profile nor an inline cache answers (vm `jit_bridge` nest-profiled).
    // Same lane (SPLICE3-2): opt-in, default OFF, inert without ir-splice-expected-throw. The stash
    // sinks outside the door sink (the trapped-callee helper sink, the first-call tier-up sink) leave
    // a marked expected-throw trip uncharged and escalate a chain's implicit exception to the named
    // innermost entry first, as the door sink does (vm `helpers::despeculate_trapped_method_of_frame`).
    // Same lane (SPLICE3-3): opt-in, default OFF, inert without ir-splice-expected-throw. Marked
    // expected-throw trips are counted per artifact; past a floor, a body whose trips exceed half its
    // entries is charged as without the mark (vm `deopt_resume::expected_throw_trip_uncharged`).
    // JIT round 19 wave 5, lane handler5 (SPLICE4-1): opt-in, default OFF. A plain call of an
    // optimizing body whose `invoke` the method's first covering exception-table row protects gets a
    // compiled handler entry: its exceptional exit asks the local-handler lookup (no-collect form) and,
    // on a catch, continues in the body's compiled copy of the handler instead of leaving for the
    // interpreter (jit `ir::IrBuilder::note_catch_arm`, `ir_lower::Lowerer::emit_catch_probes`).
    // JIT round 19 wave 6, lane handler6 (HANDLER6-1): opt-in, default OFF, inert without
    // ir-compiled-handler-entry. The scheduler moves the pure users of an armed call's result out of the
    // call's block onto the normal edge (the rest before the call), so the compiled handler probe is wired
    // for calls whose value is used (`s += f(i)`), which every one of them refused before
    // (jit `ir_schedule::split_catch_arm_call_blocks`).
    // Same lane (HANDLER19-3): opt-in, default OFF, inert without ir-compiled-handler-entry. A call gets one
    // compiled handler arm per handler of the longest table-order prefix of its covering rows that all
    // qualify (nested try, multi-catch), one probe dispatching on the lookup's candidate index; calls inside
    // an opened handler region are planned too (a catch block's call covered by finally / an outer catch)
    // (jit `ir::IrBuilder::plan_catch_arms` / `note_catch_arm`, `ir_lower::Lowerer::emit_catch_probes`).
    // JIT round 19 wave 7, lane handler7 (SYNCDOOR4-3): a bytecode-callee door (special / static /
    // virtual) whose callee IS compiled stands down on the method's per-VM probe memo word while the JIT
    // cache generation has not moved, instead of re-running the string-keyed `JitCache::get` per call
    // (vm `lambda::bytecode_callee_compiled_or_nominate`, jit-api `record_jit_probe_found`).
    // JIT round 19 wave 8, lane handler8 (HANDLER7-1): opt-in, default OFF. The compiled entry-counter
    // drain reports a loop-dominated C1 body (>= 16 entries) at back_edges / 2 instead of entries +
    // back_edges / 8, so the C1 -> C2 hotness bar opens at 40 000 back edges (HotSpot's
    // Tier4BackEdgeThreshold) (jit `entry_counter::weighted_count`).
    // r23 wave 11 L1: default ON since the w2c/churn close (2026-10-24), kill "0". A C2-tier OSR publish promotes the method's single-pass entry body to C2 (that body never migrates into the OSR body), instead of waiting for the drain clock. Zt4ChChurnShapes method / ChurnCtor.
    // JIT round 19 wave 9, lane handler9 (HANDLER8-1): opt-in perf, default OFF. A C1 body published outside the
    // tiered manager (eager callee chain, sync callee compile) whose method passes `c2_upgrade_would_engage` gets the
    // manager path's immediate Low-priority C2 supersede, bounded per VM by
    // CRATONVM_JIT_OUTSIDE_MANAGER_C2_UPGRADE_BUDGET (default 128) (jit `tiered::request_outside_manager_c2_upgrade`).
    // JIT r25 wave 4, lane regex rows: perf, opt-in. A C1 body published outside the manager (callee compiler, eager
    // chain) is promoted to C2 once the entry-counter drain reports 5 000 invocations (HotSpot's
    // Tier4InvocationThreshold) instead of the 20 000 bar (Pattern.<init> on the `compile` row)
    // (jit `tiered::outside_c1_drain_c2_enabled`).
    // Wave-9 follow-up (lane handler9): the supersede above compiles C2 only for a method whose profile store counted
    // at least this many invocations (default CRATONVM_JIT_THRESHOLD, the manager path's depth; 0 = no gate, the
    // configuration that failed R16LocaleWide 1/40 under load) (vm `jit_bridge::outside_publish_profile_is_warm`).
    // JIT round 19 wave 12, lane handler12 (HANDLER11-3, opt-in, default OFF): a sentinel consumer that completes a
    // call over the deopt stash entry it saw on entry discards that orphan at once instead of only naming it (the
    // naming, `orphan born: <consumer>` under CRATONVM_DBG_DEOPT, is unconditional) (vm `helpers::note_completed_call_left_stash`).
    // JIT round 19 wave 12, lane handler12 (HANDLER11-6, opt-in, default OFF): the outside-manager supersede is asked
    // only for a C1 body that predicts its C2 publish can forward the baked callers (vm
    // `jit_bridge::outside_supersede_forward_predicted`; census `forward-refused=`).
    // JIT round 19 wave 3, lane strgap (STRGAP19-1): opt-in perf, default OFF. The compiled concat
    // bridge renders an all-Latin-1 concat whose operands need no `toString()` straight into one
    // exact-size TLAB String; decided per site at construction (vm `invokedynamic::concat_latin1`).
    // JIT round 19 proposal wave 4, lane strgap4 (STRGAP19-3): opt-in perf, default OFF. The single-pass
    // backend builds an admitted `StringConcatFactory` concat (String/int/long/char/boolean operands, Latin-1
    // literals) inline: one length pass, one TLAB bump, one fill pass; the bridge call is the decline edge
    // (jit `x64/op_concat.rs`, plan `concat_inline_plan.rs`).
    // JIT round 19 proposal wave 4, lane strgap4 (STRGAP19-4): opt-in perf, default OFF. A direct
    // `String.valueOf(Object)` call of an exact `String` answers its argument inline (single-pass prefix;
    // null and every other class keep the call) (jit `x64/op_concat.rs`).
    // JIT round 19 proposal wave 5, lane concat5 (STRGAP4-1): opt-in perf, default OFF. The optimizing tier's
    // twin of `concat-inline`: an admitted bridged concat (`Op::IndyBridge`, itself behind
    // CRATONVM_JIT_IR_INDY_BRIDGE) is built inline ahead of the bridge call, which stays its decline edge
    // (jit `ir_lower.rs` `lower_indy_bridge_concat_inline`, `x64/concat_emit.rs`).
    // JIT round 19 proposal wave 5, lane concat5 (STRGAP4-2): opt-in perf, default OFF. Compiled code reads this
    // VM's rooted `"null"` / `""` literal slots (JNI-global-ref words in the per-VM ldc slot table): the null
    // argument of the `string-valueof-thin` prefix and the empty builder of the inline `toString()` stay inline
    // (vm `jit::helpers::string_literal_slot_for`, jit `x64/op_concat.rs`, `x64/op_invoke.rs`).
    // JIT round 19 proposal wave 6, lane str6 (CONCAT5-4): opt-in perf, default OFF. The compiled concat
    // bridge's general renderer builds an all-Latin-1 result from the thread's TLAB (refill allowed), so an
    // inline concat's TLAB-full decline refills the TLAB without CRATONVM_JIT_CONCAT_LATIN1_FAST; decided per
    // site at construction (vm `invokedynamic::concat_values_to_string`, `concat_latin1::tlab_result_enabled`).
    // JIT round 19 proposal wave 7, lane str7 (STRGAP4-4): opt-in perf, default OFF. Both tiers' inline concat
    // lowers a site whose literal text forces a UTF16 result (a literal unit above 0xFF): units counted, LATIN1 String operands
    // inflated, UTF16 ones copied, `coder = UTF16`; without it such a site keeps its bridge call
    // (jit `concat_inline_plan.rs` `JitConcatInlinePlan::wide`, `x64/concat_emit.rs` `emit_*_pass_wide`).
    // JIT round 19 proposal wave 7, lane str7 (CONCAT5-3 follow-up): opt-in perf, default OFF. The optimizing
    // tier's indy bridge and its inline concat as ONE switch (CRATONVM_JIT_IR_INDY_BRIDGE +
    // CRATONVM_JIT_IR_CONCAT_INLINE): the bridge alone made hot concats bare bridge calls, the "8x worse" arm
    // (jit `ir::ir_indy_bridge_with_inline_enabled`).
    // JIT round 19 proposal wave 6, lane str6 (STRGAP19-6): opt-in, default OFF. Both tiers' inline
    // `String.hashCode()` honour and set `hashIsZero` as JDK 25's body does, on a COMPACT receiver whose layout
    // records the one-byte field (jit `x64/op_invoke.rs` kind 3, `ir_lower::emit_str_hash_compute`,
    // `StringFieldLayout::with_hash_is_zero`).
    // r23 wave 13, lane L6: perf, opt-in "1" (lost on the n23-w13a A/B: split rows ~2x slower on). The String-intrinsic pin to the single-pass backend ignores
    // invoke sites inside a cold throw arm (the message-building StringBuilder calls of `throw new X(a + " > " + b)`),
    // so Arrays.copyOfRange([BII) and Pattern.<init> reach the optimizing tier (jit
    // `string_pin_skips_cold_throw_arms_enabled`).
    // JIT round 19 wave 4, lane irops3 (IROPS2-7): opt-in perf, default OFF. The IR bounds check of a
    // clamped index (`max(0, min(i, a.length - 1))` and its mirror) is dropped where `a.length >= 1` is
    // proven (jit `ir_check_elim::clamp_index_enabled`).
    // JIT round 19 wave 5, lane osr5 (IROPS3-1): opt-in perf, default OFF, inert without ir-bce-clamp-index.
    // A loop-entry `Guard(0 < a.length)` for a loop indexing an invariant array by a clamp, so the first
    // clamped access loses its check too (jit `ir_optimize::plant_clamp_length_predicates`).
    // Same lane (IROPS2-5): opt-in, default OFF, inert without ir-static-alias-class. A reference
    // `putstatic` of an initialised class is a write of its own static cell plus a safepoint in the
    // effect model and the LICM predicates (jit `ir::ir_static_ref_store_effect_enabled`).
    // Same lane (IROPS19-5): opt-in, default OFF. IR FP `Math.min`/`max` of two register-resident
    // operands is a branch-free AVX sequence where the host has AVX (jit `ir_lower::ir_fp_minmax_avx_enabled`).
    // Round nio20 wave 1, lane L5 (NIO19-4): perf, both default ON (kill switch "0"). The IR splice planner
    // visits sites in profile order (in-loop first, cold exits last) and a hot-loop method may plan 40
    // sites, the extra slots in-loop only (jit `ir_splice_order::profile_plan_order`).
    // JIT round 20 wave 11 lane pSplice (jit-r20-nio-proposals-w2-L3.md L3W2-4, jit-r20-nio-proposals-L5.md NIO20-L5-2/3): the profile plan order ranks in-loop sites by execution count before loop depth when every in-loop site has a count (both the method-entry planner and the OSR door). Default ON, kill "0".
    // Round nio20 wave 1, lane L3 (NIO19-8): perf, default OFF (A/B pending). The optimizing tier's
    // invoke-planning gate bails a method for a `java/lang/String` call only when the single-pass
    // backend would intrinsify that member (jit `string_site_bails_ir_planning`). nio20 wave 4 (L5):
    // the value `callers` narrows it to methods outside `java/lang/String{,Latin1,UTF16}`
    // (jit `string_site_bails_ir_planning_in`). JIT r23 wave 6 (L1): default ON, kill "0" (split-comma
    // 2080-2256 -> 1322-1672 ns/op, heapcb unchanged).
    // R23-W6-FLIP (JIT round 23 wave 6, lane L1): default ON, kill "0" (String.split(CIZ) admitted to C2; split-comma 2080 -> 1322-1672 ns/op, w5all facts 144/146 SAME).
    // Round nio20 wave 3, lane L2: perf, all default ON (kill switch "0"). The optimizing tier answers
    // `String.hashCode/equals/compareTo/indexOf(I)/indexOf(String)` inline as a prefix of the call, and
    // the planning gate / String pin / splice twin admit methods with such sites (jit
    // `ir_string_search_prefix_enabled`); the same prefix answers the guarded `CharSequence`
    // `length/isEmpty/charAt` behind a header class-id compare (jit `ir_string_search_prefix_kind`);
    // the exact-`ArrayList` pin is released because the optimizing tier lowers that prefix itself (jit
    // `ir_tier_lowers_arraylist_prefix`, was the const `IR_TIER_LOWERS_ARRAYLIST_PREFIX = false`).
    // J27 host A/B (2026-11-02): the StringBuilder length/charAt/append(C) prefix
    // is default-on with `=0` as its kill switch; `append(String)` remains opt-in.
    // The prefix and its pin/splice carve-outs share `ir_sb_prefix_enabled`.
    // nio20 wave 5, lane L5 (W4-L5-2): `StringBuilder.append(String)` as an IR prefix kind.
    // JIT r20 wave 18, lane xSb (book `jit-r20-strings-proposals.md`, STR20-2): `StringBuilder.append(int)`
    // and `toString()` as IR prefix kinds (jit `ir_sb_append_int_prefix_enabled` /
    // `ir_sb_to_string_prefix_enabled`; inside `CRATONVM_JIT_IR_SB_PREFIX`).
    // JIT r20 wave 18, lane xSb (STR20-1): a `StringBuilder.charAt` site keeps its call (and the IR prefix)
    // instead of splicing `AbstractStringBuilder.charAt` (jit `ir_sb_charat_stays_call_enabled`).
    // nio20 wave 6, lane L2: `String.getChars(II[CI)V` / `StringLatin1.inflate` as IR prefix kinds
    // (jit `ir_string_getchars_prefix_enabled`; inside `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`).
    // JIT r20 wave 12, lane qStr (book `jit-r20-nio-proposals-w6-L2.md`, W6-L2-2): `toCharArray()`,
    // `valueOf(char[])` / `copyValueOf(char[])` and `new String(char[])` as allocating IR prefix kinds
    // (jit `ir_string_chars_prefix_enabled`; inside `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`).
    // JIT r20 wave 18, lane xUtf8 (book `jit-r20-strings-proposals.md`, STR20-6):
    // `StringCoding.countPositives` / `implEncodeAsciiArray` as IR prefix kinds, the SSE2 scan / copy
    // HotSpot's stubs make (jit `ir_string_coding_prefix_enabled`; inside
    // `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`).
    // JIT r20 wave 19, lane zCp (page `jr20-pstr3-code-point-latin1-static-init-mcs-flip-rows-CLOSED-20261012.md`,
    // `R20PIRCodePoint` cp-append / cp-tochars): `StringBuilder.appendCodePoint(int)` (LATIN1 byte, UTF16
    // char or surrogate pair, in place; NOT inside `CRATONVM_JIT_IR_SB_PREFIX`) and the static
    // `Character.toChars(int)` (an inline `char[1]` / `char[2]`) as IR prefix kinds (jit
    // `ir_sb_append_code_point_prefix_enabled` / `ir_character_to_chars_prefix_enabled`; inside
    // `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`). Default ON, kill "0".
    // r23 wave 1, lane L5: correctness, default ON, kill "0". The appendCodePoint prefix's supplementary arm checks
    // `count < capacity` before its room test, so a negative `count` never addresses outside the array (jit
    // `ir_sb_append_code_point_count_guard_enabled`).
    // r21 strings (JIT round 21 wave 1, lane strings; proposal CP21-1 in `jit-r20-wave20-residual-proposals.md`,
    // `R20PIRCodePoint` cp-iterate): `String.codePointAt(int)` as an IR prefix kind -- lifts the String
    // invoke-planning gate for the site and answers LATIN1 / UTF16 / surrogate pairs inline (jit
    // `ir_string_code_point_at_prefix_enabled`; inside `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`). Perf: default ON since the r22 A/B, kill "0".
    // r23 wave 1, lane L5 (STRINGS21-4): perf, default ON, kill "0". String.codePointBefore(int) and
    // codePointCount(int, int) as optimizing-tier prefix kinds (inline answer, the planning gate lifted, the splice
    // carve-out), each its own switch (jit `ir_string_code_point_before_prefix_enabled` /
    // `ir_string_code_point_count_prefix_enabled`).
    // r21 strings (same lane, book `jit-r21-strings-proposals.md` STRINGS21-1 / STRINGS21-2, `jit-r20-zCp-proposals.md`
    // ZCP-3): the planning gate admits `String.coder()` / `isLatin1()` (inline prefix kinds) and `String.<init>([BB)V`
    // (an ordinary call) (jit `ir_string_internal_admit_enabled`); `compareToIgnoreCase` with two LATIN1 operands
    // inline (jit `ir_string_compare_to_ci_prefix_enabled`); `toLowerCase` of an already-lower LATIN1 string answers
    // the receiver (jit `ir_string_lower_unchanged_prefix_enabled`). All three perf, default ON since the r22 A/B, kill "0".
    // r21 strings, wave 3 (STRINGS21-3): `StringBuilder.length()` as an IR prefix kind outside the opt-in
    // `CRATONVM_JIT_IR_SB_PREFIX`, releasing builder loops whose only pinned site it is (jit
    // `ir_sb_length_prefix_enabled`). Perf: opt-in.

    // JIT round 21, lane strings (ZCP-4, page `jr21-latin1-case-rows-FIXED-20261015.md`, row `R20PNatLatin1`
    // latin1-compareToCI): `StringLatin1.compareToCI(byte[],byte[])` as an IR call prefix
    // (jit `ir_latin1_compare_to_ci_prefix_enabled`; inside `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`).
    // Default ON, kill "0".
    // JIT round 21, lane str21b (STRINGS2-21-4, gate G3 of `str21b-strings2-host-gates-20261102.md`):
    // `StringLatin1.regionMatchesCI(byte[], int, byte[], int, int)` -- the tail of every LATIN1
    // `equalsIgnoreCase` -- as an IR call prefix (jit `ir_latin1_region_matches_ci_prefix_enabled`; inside
    // `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`). Opt-in until a host A/B decides it.
    // JIT round 21, lane strings2 (STRINGS21-1, page `jr21-strings2-latin1-case-map-and-switch-interactions-20261014.md`,
    // rows `R20PNatLatin1` latin1-toLowerCase, `R21Strings2Rows`): `StringLatin1.toLowerCase` / `toUpperCase(String,
    // byte[], Locale)` as an allocating IR call prefix (jit `ir_latin1_case_map_prefix_enabled`; inside
    // `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`). Default ON, kill "0".
    // JIT round 21, lane ci (same page, rows `R21Strings2Rows` compareToCI-steady, `R21StringsLatin1Case` ci-*):
    // a nested `StringLatin1.compareToCI` / `toLowerCase` / `toUpperCase` body is dropped to a bound call so the
    // inline prefix fronts it (jit `ir_latin1_case_nested_to_call_enabled`, read by the VM's inline resolver).
    // Default ON, kill "0".
    // JIT round 21, lane strings (page `jr21-latin1-case-rows-FIXED-20261015.md`, row `R20PNatLatin1`
    // latin1-toLowerCase): `String.<init>([BB)V` in a `StringLatin1` / `StringUTF16` method leaves the
    // optimizing planner's blanket String refusal (jit `ir_string_coder_ctor_call_admits`). Default ON,
    // kill "0".
    // JIT round 21, lane strings (same page, row latin1-compareToCI): `String.coder()` / `isLatin1()` /
    // `value()` leave the optimizing planner's blanket String refusal (jit
    // `ir_string_coder_accessor_call_admits`). Default ON, kill "0".
    // JIT round 21, lane strings (page `jr21-string-code-point-at-native-funnel-20261014.md`, row
    // `R20PIRCodePoint` cp-append): a method with a `StringBuilder.appendCodePoint` site is not pinned to
    // the single-pass backend (jit `sb_code_point_site_unpins`). Default ON, kill "0".
    // JIT r20 wave 12, lane qStr (book `jit-r20-nio-proposals-w4-L5.md`, W4-L5-1): the SSE2 first-byte
    // scan in the LATIN1 `indexOf(String)` prefix (jit `ir_string_indexof_scan_enabled`; inside
    // `CRATONVM_JIT_IR_STRING_SSE2`).
    E { group: Group::JIT, token: "profile-fast-call-counts", on_key: Some("CRATONVM_JIT_PROFILE_FAST_CALL_COUNTS"), off_key: None, off_word: Some("0"), since: "2026-10-07" },
    // Round nio20 wave 4, lane L3: perf, all default ON (kill switch "0"). The precise-frames splice
    // interlock is per site at the single-pass method-entry door and the OSR door's guarded pass too
    // (jit `sp_splice_precise_per_site_enabled`, W3L3-1/2); a nested trap guard is charged under its
    // own named speculation at the call site so its withdrawal keeps the outer splice (jit
    // `ir_nested_trap_own_charge_enabled`, W3L4-4); the single-pass `Thread.currentThread()` prefix
    // reads the thread's mirror word (vm `thread_java_obj_offset_for`; not under --real-jdk).
    // Round nio20 wave 5, lane L3: perf, both default ON (kill switch "0"). The optimizing tier's
    // counted-loop recogniser reads the induction phi's slots from the header's predecessor order,
    // takes a non-constant init (runtime-bound) and `phi - c`, fully unrolls a constant trip of up
    // to 16 over a small body and hands a longer constant trip to the main/post form (jit
    // `ir_optimize::unroll_wide_counted_enabled`); the full unroller takes a loop a frame names when
    // the LOOP cannot deopt and no stranded frame is reachable, its copies pc-less (jit
    // `ir_optimize::pure_body_frames_unconsultable`, net in `ir_lower` against the transfer bcis).
    // Round nio20 wave 6, lane L4: perf, all three default ON (kill switch "0"). A plain `getstatic`
    // the compile reads directly moves out of a loop that writes no memory (jit
    // `ir_optimize::loop_static_hoist`); a loop indexing an invariant array with `x & m` (`m` a
    // non-negative constant) gets one pre-header `m < a.length` guard deopting to the loop entry (jit
    // `ir_optimize::mask_predicate`); the check elimination reads `m < a.length` (or a fresh array of
    // constant length) as proving `a[x & m]` (jit `ir_check_elim::const_mask_in_range`).
    // JIT round 21, lane loopstatic (page jr21-loopstatic-static-read-loop-cost-20261015): perf, default ON
    // (kill switch "0"). The loop static-read hoist runs once more after the loop promotions, over the loops a
    // promotion left write-free, so their direct `getstatic`s leave the loop and the post-promotion unroll can
    // take it (jit `ir_optimize::loop_static_hoist::loop_static_hoist_after_promote_enabled`).
    // JIT round 19 wave 11, lane hoist11 (opt-in, default OFF): HOIST11-1, the loop static-read
    // hoist also moves past instance/element stores (and, with the static alias class, direct
    // `putstatic`s of other cells); HOIST11-2, its moved reads are recorded for the OSR entry stub's
    // re-read (`Graph::licm_hoisted_static_reads`). jit `ir_optimize::loop_static_hoist`.
    // JIT round 19 wave 12, lane hoist12 (opt-in, default OFF): HOIST11-3, the direct-static LICM arm
    // on the optimizing OSR route only; HOIST11-4, the loop static-read hoist on the OSR route,
    // moving only reads the entry stub re-reads; HOIST11-5, one shared body classifier for both
    // direct-static hoists. jit `ir_optimize::loop_static_hoist` / `ir_optimize::licm_outcome`.
    // HOIST12-1 (opt-in): a direct static read parked in a loop pre-header no longer counts as a trap
    // for the later LICM arms (it was exempt only under the LICM direct-static switch).
    // JIT round 19 wave 12 integration (2026-10-09): default ON, `=0` kill (A/B in the round record).
    // Round nio20 wave 4, lane L1: perf, both default ON (kill switch "0"). The `ScopedMemoryAccess`
    // fold is classed as RAW memory (aliases primitive array elements and other raw accesses, never a
    // field) instead of opaque (jit `ir::scoped_raw_effect`, W3L4-1); the builder's field memo records
    // and answers inside spliced bodies with depth-scoped clears (jit
    // `IrBuilder::field_load_memo_clear_at_splice_join`, W3L4-2).
    // r23 (wave 2e, lane L2; W4L1-1 / W4L1-2 of jit-r20-nio-proposals-w4-L1.md): perf, default ON, kill "0". The G14-1 field
    // memo skips a guarded splice diamond's miss call while the hit body is walked (applied at the join), and keeps at a
    // forward join of the compiling method's own code the entries every incoming edge carries unchanged (jit
    // `ir_field_memo_defer_miss_call_enabled`, `ir_field_memo_precise_joins_enabled`).
    // Round nio20 wave 4, lane L2: perf, default ON (kill switch "0"). The optimizing tier's
    // `ScopedMemoryAccess` fold admits a non-null session inline when its exact class is one of the
    // JDK's never-closing `GlobalSession` / `GlobalSession$HeapSession` / `ImplicitSession` (jit
    // `ir_lower::scoped_session_never_closing_class_ids`); off = every non-null session deopts.
    // Round nio20 wave 6, lane L1: perf, both default ON (kill switch "0"). A plain `getstatic` the
    // lowering serves as the direct read is a static-cell read for the alias model, not `Opaque` (jit
    // `ir::plain_static_read_effect`); a profile-guarded `ByteBuffer` access splice takes a deopt as its
    // miss edge instead of the virtual call (jit `lib.rs` `ir_buffer_trap_miss_enabled`).
    // nio21 wave 8 L5: the single-pass box/hash prefix has a java.lang.Class arm at Object.hashCode() sites (identity hash inline). Default ON, kill "0".
    // nio21 wave 9 L2: twelve reviewed java/lang/Math *Exact methods splice their JDK body in place of the Intrinsic native (overflow arm = deopt guard). Default ON, kill "0".
    // nio21 wave 10 L1: under ZGC the inline allocation stores the header, then the start bit, then commits the TLAB cursor (both tiers, box prefixes, inline toString). Default ON, kill "0".
    // nio21 wave 11 L5: a VarHandle access through a static final constant handle on an int / long / reference instance field is inline in both tiers (guarded prefix, the call on a miss). Default ON, kill "0".
    // nio21 wave 14 L4: the constant-handle inline checks interface-typed field values (stored and expected) against the value class interface set in its supers display. Default ON, kill "0".
    // JIT r20 wave 18, lane xVhArr (AT-2 of docs/feature-designs/jit-ir-optimizations.md): a static final arrayElementVarHandle(int[].class / long[].class) site's get / set / CAS / compareAndExchange / getAndAdd / getAndSet / getAndBitwise is the element access inline in both tiers (a null array, an index out of range, another array decline to the call). Default ON, kill "0".
    // JIT r20 wave 19, lane yFfmShared (MEM20-2 of docs/feature-designs/jit-ir-optimizations.md): the IR scoped fold lowers a SHARED model session (Arena.ofShared(): owner null, state open) inline under the thread's FFM access window (LOCK INC of the leased access word, state re-read inside it, INC after the access) instead of deopting to the helper path. Needs CRATONVM_JIT_IR_SCOPED_CONFINED_FOLD and CRATONVM_FFM_ACCESS_HANDSHAKE. JIT round 22 (lane memory): also the JDK's own SharedSession (every Arena.ofShared() since the JdkForeign row), and default ON, kill "0".
    // JIT r20 wave 19, lane zLdc (THROW20-4 `date` of docs/feature-designs/jit-ir-optimizations.md): an optimizing splice may carry an ldc of a String / Class entry of the callee's pool whose constant is already resolved and has a GC-maintained slot (built as the top-level ConstString / ConstClass with the callee's holder and cp index); inside it, x.getClass() == C.class on a fresh `new X` is answered at build time. The single-pass inliner keeps refusing. Default ON, kill "0".
    // nio21 wave 14 L1: a capturing lambda site memoises its proxy class and capture count and tries a pin-free TLAB allocation inside the bridge (full path on a miss). Default ON, kill "0".
    // nio21 wave 21 L3: a trapping CharSequence.charAt whose receiver is loop-invariant becomes IR: a String class guard plus value/coder/length reads at the loop entry, index guards and the LATIN1/UTF16 decode at the call. Default ON, kill "0".
    E { group: Group::JIT, token: "ir-seq-charat-hoist", on_key: Some("CRATONVM_JIT_IR_SEQ_CHARAT_HOIST"), off_key: None, off_word: None, since: "2026-10-09" },
    // JIT r25 wave 6 W6-C: a loop-entry String class guard for the loop-invariant receiver of every CharSequence.charAt call in a loop (method-entry graphs, and OSR graphs behind the header's opaque arm); licenses the charAt trap, the field forward and (with ir-seq-charat-hoist) the hoist. Opt-in.
    E { group: Group::JIT, token: "ir-seq-charat-predicate", on_key: Some("CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE"), off_key: None, off_word: None, since: "2026-10-24" },
    // JIT r25 wave 7 W7-D: the charAt predicate (and the charAt hoist) also for a loop SPLICED from a callee, whose header has only an inlined chain snapshot: the guard takes the loop-entry chain frame and the call site's chain id (regex-groups' BmpCharPropertyGreedy loops). Opt-in.
    E { group: Group::JIT, token: "ir-seq-charat-predicate-spliced", on_key: Some("CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE_SPLICED"), off_key: None, off_word: None, since: "2026-10-24" },
    // JIT r25 wave 7 W7-D (W6C-P4): the charAt predicate plants only when every call's receiver profile (the callee's own for a spliced site) is monomorphic String. Opt-in.
    // JIT r25 wave 7 W7-D (W6C-P2): in a charAt-guarded loop, a guarded splice's receiver read from an invariant object's field gets a loop-entry read and class guard; its class tests fold and the miss-arm calls go. Opt-in.
    E { group: Group::JIT, token: "ir-seq-charat-field-class-predicate", on_key: Some("CRATONVM_JIT_IR_SEQ_CHARAT_FIELD_CLASS_PREDICATE"), off_key: None, off_word: None, since: "2026-10-24" },
    // JIT r25 wave 7 W7-D (W6C-P3): a hoisted charAt's per-character i < count and 0 <= i guards answered by one loop-entry x <= count guard from the loop's own i < x test. Opt-in.
    E { group: Group::JIT, token: "ir-seq-charat-entry-bound", on_key: Some("CRATONVM_JIT_IR_SEQ_CHARAT_ENTRY_BOUND"), off_key: None, off_word: None, since: "2026-10-24" },
    // JIT rx27 wave 1 (lane scanloop): a trapping CharSequence.charAt whose receiver a dominating loop-entry ExactClassIs(recv, String) guard proves skips the String prefix's per-call null, kind-tag and class-id tests (the regex scan loops). Opt-in.
    // JIT rx27 wave 3 (lane scanloop): the trapping charAt hoist at the outer (String-guarded) position loop, with a coder == LATIN1 entry guard (one byte load per char) and the entry bound offered the nested scan loop. Opt-in.
    // JIT rx27 wave 3 (lane scanloop): the LATIN1 speculation of ir-seq-charat-outer-hoist; default ON under it, "0" keeps the branchless LATIN1/UTF16 decode.
    // JIT rx27 wave 4 (lane scanloop, P-1): in a charAt-guarded loop, each guarded-splice class test of a read whose base chain is loop-invariant (this.next, next.predicate) becomes an identity compare with one class-guarded loop-entry read of the chain; the reads stay, so calls in the loop need no writer proof. Opt-in.
    // nio21 wave 20 L3: the ThreadLocal expansion's field loads carry their own compact field rows (per node), so compact objects load inline instead of through the checked getfield helper. Default ON, kill "0".
    // JIT round 21 lane tlocal (docs/feature-designs/jit-ir-optimizations.md): the ThreadLocal get/set expansion walks the linear-probe chain up to two slots past the home slot (a cleared key is a miss), so a displaced key hits inline instead of paying the helper per call. Default ON, kill "0".
    // JIT round 21 lane tl2 (same page): an InheritableThreadLocal-typed get/set site with an exactly-InheritableThreadLocal receiver takes the ThreadLocal expansion over Thread.inheritableThreadLocals. Default ON, kill "0".
    // JIT round 21 lane tl2 (same page): a ThreadLocal get/set call left in an IR-spliced body (virtual sites only) binds the jit_thread_local_{get,set}_direct thin helper the compiling method's own sites bind. Default ON, kill "0".
    // JIT round 21 lane tl3 (docs/feature-designs/zgc-architecture.md): the optimizing tier answers Reference.refersTo(Object) -- and a spliced refersToImpl / refersTo0 -- as an inline identity compare of the referent slot (no keep-alive, volatile-ordered), the call kept for a null receiver only. --real-jdk (rides the ThreadLocal layout). Default ON, kill "0".
    // JIT round 21 lane refs2 (same page): the refersTo expansion takes Reference.referent's index from the VM's referent-only answer (--real-jdk) when the compile has no ThreadLocal layout, instead of waiting for InheritableThreadLocal / TerminatingThreadLocal to be loaded. Default ON, kill "0".
    // JIT round 21 lane tlsub (TL321-4; docs/feature-designs/jit-ir-optimizations.md): a ThreadLocal.get / set site whose receiver profile names a ThreadLocal subclass that the VM says reads the plain map (no get / set override: the initialValue-override idiom) expands for that class too; the id is recorded as a copied class (unload withdraws the code). Default ON, kill "0".
    // JIT round 21 lane tlget (TLGET21-1; docs/feature-designs/jit-ir-optimizations.md): a ThreadLocal.get / set site with no settled receiver profile (never run when its method compiled, e.g. a kernel spliced into a driver warmed on another row) tests the layout's classes as an uncommon trap at the invoke, so a subclass receiver deopts once and the recompile admits it from the profile; withdrawn per site by the de-spec registry. Default ON, kill "0".
    // JIT round 21 lane tlget (TLGET21-2; docs/feature-designs/jit-ir-optimizations.md): the ThreadLocal.get / set expansion's reference loads (thread.threadLocals, map.table, entry.referent, entry.value) take the trusted-receiver form -- one read-bounds-table-live compare instead of the alignment test and six region compares -- on their Ref-typed bases. Default ON, kill "0".
    // TL321-5 (docs/feature-designs/jit-ir-optimizations.md): the ThreadLocal expansion's Entry.referent compares carry the referent's compact row (layout word 18), so a compact Entry is compared inline instead of through jit_getfield on every probe. Default ON, kill "0".
    // TLGET21-3 (same page): a trap-form ThreadLocal.get / set chain in a loop that writes nothing else moves whole to the loop's pre-header as loop-entry predicates (ir_optimize/tl_chain_predicate.rs); a set keeps its store in the loop. Default ON, kill "0".
    // (same page, tl-set-home): the ThreadLocal.set expansion's e.value store carries Entry.value's compact row, so a compact Entry takes the collector's gated inline reference store instead of jit_putfield_object. Default ON, kill "0".
    // (docs/feature-designs/jit-ir-optimizations.md, worker-chain-3): the probe walk tries four slots past home instead of two. Default ON, kill "0".
    // TLGET21-6 (docs/feature-designs/jit-ir-optimizations.md): a ThreadLocal site with no settled profile is the whole trap form (the layout's classes), not TLGET21-1's call form with a trapping class test. Default ON, kill "0".
    // TLGET21-6 (same page): one receiver class alone over the 16-sample seed floor (not 32) names the trap form's class. Default ON, kill "0".
    // TLGET21-7 (docs/feature-designs/jit-ir-optimizations.md): a never-run ThreadLocal.get / set site compiled while the expansion's layout is pending (ThreadLocalMap / Entry not loaded yet) is an uncommon trap at its invoke, so the recompile expands it. Default ON, kill "0".
    // JIT round 21 lane tlsub (TL321-1; docs/feature-designs/jit-ir-optimizations.md): a ThreadLocal.get / set site whose receiver profile saw one admitted class alone builds every miss of the expansion as an uncommon trap at the invoke (top level, or a spliced body's chain snapshot) instead of the call; withdrawn per site by the de-spec registry. Default ON, kill "0".
    // JIT round 21 lane tl3 (TL221-1; jr21-tl2-weakref-refersto-and-get-cost-FIXED-20261015.md): a spliced Reference.referent read (Reference.get's body) reads inline while the collector's SATB pre-gate byte is clear, and takes the checked jit_getfield helper (the SATB keep-alive) while it is armed. Default ON, kill "0".
    // t1 wave 10 lane REFS3 (page t1-refs2-refersto-expansion-load-takes-the-referent-keep-alive): the refersTo expansions' compare-only referent reads mark their checked jit_getfield call GETFIELD_NO_KEEPALIVE, so a compact receiver's read skips the slot-0 SATB keep-alive (HotSpot: AS_NO_KEEPALIVE). Correctness, default ON, kill "0".
    // t1 wave 12 lane REFS4: the refersTo expansions' referent reads carry Reference.referent's compact row (VM-named on Gen / G1 only), so a compact receiver reads inline instead of calling the checked jit_getfield helper. Default ON, kill "0".
    // t1 wave 12 lane REFS4: the VM's referent queries answer --compatible too (real class bytes, a loaded Reference whose first instance field is referent), so the refersTo expansion runs in both modes. Default ON, kill "0".
    // JIT round 21 lane spg1vh (G1VHSOFT21-1; docs/feature-designs/jit-barrier-lowering.md): the single-pass constant-VarHandle prefix builds a reference write mode (set* / CAS / compareAndExchange* / getAndSet*) under G1 against G1's plan (the optimizing tier's vh_g1_gates: SATB pre-gate, null / same-region screen) with the cross-region post edge (the access, the edge filter, the single-pass putfield's jit_g1_post_write_barrier call), instead of keeping the call. Default ON, kill "0".
    // nio21 wave 20 L3: the optimizing tier expands ThreadLocal.set(Object) inline (first-probe hit stores e.value through an ordinary barriered store) with the call as the miss arm; admitted only when jdk.traceVirtualThreadLocals is off. Default ON, kill "0".
    // nio21 wave 19b L4: trap_cause_at_frame believes a stash cause named by a non-rethrow guard box of this artifact at the frame bci, so a failing guard is charged under its real reason (evict, de-spec, recompile) instead of the uncharged TransferToInterpreter resume point (the TreeMap trap storm). Default ON, kill "0".
    // nio21 wave 19 L3: strip_trivial_phis strips each phi input recursively (depth 8) so a loop-carried lock local matches its monitorenter at a join (ConcurrentHashMap.putVal / replaceNode). Default ON, kill "0".
    // nio21 wave 19 L3: the optimizing tier expands ThreadLocal.get() inline (current thread mirror, threadLocals map, first-probe entry) with the helper call as the miss arm. Default ON, kill "0".
    // nio21 wave 19 L3: the loop field reload admits a writer inside a splice, naming a copy of its post-call chain snapshot. Default ON, kill "0".
    // nio21 wave 19 L3: the loop field reload admits a writer whose successor is a join whose stack top is the phi the call feeds. Default ON, kill "0".
    // JIT round 20 wave 12, lane qNull (book docs/feature-designs/jit-ir-optimizations.md, L1W19-2): a single-pass implicit-null getfield site whose null receiver faulted PER_BCI_DESPEC_LIMIT times is recorded in the VM's de-spec registry and the method evicted; the recompile keeps an explicit TEST/JZ there (HotSpot Reason_null_check). Default ON, kill "0".
    E { group: Group::JIT, token: "null-trap-recompile", on_key: Some("CRATONVM_JIT_NULL_TRAP_RECOMPILE"), off_key: None, off_word: Some("0"), since: "2026-10-12" },
    // nio21 wave 18 L3: an athrow reached while the body holds a monitorenter lock becomes an always-firing deopt guard instead of refusing the method (ConcurrentHashMap.putVal gets an optimizing body). Default ON, kill "0".
    // nio21 wave 18 L3: loop field forwarding admits a loop with an opaque writer (call/new/indy) on a cold arm by reloading each forwarded field right after the writer. Default ON, kill "0".
    // nio21 wave 18 L4: an optimizing compile skipped by the acceptance memo publishes a refusal verdict, so the supersede-abandoned rule stops republishing identical single-pass bodies. Default ON, kill "0".
    // nio21 wave 18 L4: a single-pass kind-1 call site with no direct row and no inline plan, statically bound to a final owner, gets the MIC/PIC pair (misses through jit_invoke_bound_site_mic). Default ON, kill "0".
    // JIT round 20 wave 11, lane pLate (book docs/feature-designs/jit-ir-optimizations.md W18L4-2 = DISP20-1): the single-pass bound-site IC (method entry and OSR door) drops its final-owner condition; its PIC is marked SITE_BOUND (no megamorphic gate, no hashed ways, no shared-table selector or vtable column). Default ON, kill "0".
    // t1refl (page t1-R20DISP-proposal-bound-pic-hashed-ways): perf, opt-in. A SITE_BOUND single-pass PIC past its four inline ways opens its megamorphic gate and takes hashed ways of its own (never a selector, column, baked binding or profile seed), so a fifth receiver class no longer calls jit_invoke_bound_site_mic on every call. jit `inline_cache_pic::bound_pic_hashed_ways_enabled`.
    // JIT round 20 wave 19, lane yDisp (DISP20-2 of docs/feature-designs/jit-ir-optimizations.md): a lambda thunk whose outgoing side (context + captures + SAM args) is past the register file is FRAMED (push rbp, the stack words at [rsp + shadow + 8k], call impl, leave; ret), registered as a JIT code range with an empty label and a reference-free band so walkers see through it. Default ON, kill "0".
    // r21 throws (THROWS21-1 of docs/feature-designs/jit-ir-optimizations.md): perf, opt-in. A
    // compiled catch does not count the process-wide local-handler census rows (two shared `lock xadd`s per
    // caught throw) unless CRATONVM_DBG_JIT_METHOD_STATS, the census line's only reader, is on
    // (vm `jit::helpers::note_local_handler_at_catch`; reader `ThrowableLayoutCache::throw_census_skipped`).
    // r22 throws
    // (churn-loop J1, docs/feature-designs/deopt-osr.md): perf,
    // opt-in. The optimizing OSR door's build bridges an `invokedynamic` ONLY where the default arm would bail
    // the whole build (its uncommon trap refused by the replay guard, or no shape), so a concat after a hot
    // loop no longer keeps the loop in the single-pass OSR body; plantable traps are kept
    // (jit `ir::ir_indy_bridge_on_trap_refusal_enabled`, `IrBuilder::indy_default_trap_would_plant`).
    // r22 throws: correctness, default ON, kill "0". A compiled `newarray` / `anewarray` with a negative length
    // whose NegativeArraySizeException cannot be built publishes what its construction threw (wave 2), else the
    // OutOfMemoryError (no-collect, then the preallocated one), instead of exiting with nothing pending
    // (vm `jit::helpers::jit_negative_array_size`).
    // r22 throws wave 2 (THROWS22-3): correctness, default ON, kill "0". The IR builder's trap replay guard also
    // asks the VM sink's own rule (`deopt::replay_from_entry_commits_nothing_with_handlers`, first arrival), so
    // CRATONVM_JIT_DEOPT_REPLAY_FIRST_ARRIVAL=0 no longer plants traps the sink refuses (jit `ir::trap_replay_is_safe`).
    // r22 throws wave 10: correctness, default ON, kill "0". The IR builder's trap replay walk also follows the
    // exception rows this body takes in-frame (local throw / implicit / call-site edges, compiled handler entries),
    // so a handler's side effect placed after a site trap is seen (jit `ir::commits_before_first_arrival_with_handlers`).
    // r22 throws wave 2 (THROWS21-5): perf, opt-in. The fast-throw trap profile is keyed by label under a
    // reader-writer lock: a probe takes a shared lock and clones no Arc (vm `exceptions::FastThrowTrapProfile`).
    // nio21 wave 17 L4: the loop field forward treats two bases guarded to different exact classes at loop entry as different objects. Default ON, kill "0".
    // nio21 wave 17 L2: the ThreadLocal.get helper reads Thread/ThreadLocalMap/Entry fields as raw slots through the collector's load barrier instead of the generic accessor. Default ON, kill "0".
    // JIT round 23 wave 1, lane L4 (w17-L2 item 3): perf, default ON, kill "0". The ThreadLocal.set thin helper reads and
    // logs the overwritten Entry.value only on Generational while its SATB queue is active; G1's and ZGC's set_field log
    // it themselves (vm `env_cache::jit_tl_set_single_satb`).
    E { group: Group::JIT, token: "tl-set-single-satb", on_key: Some("CRATONVM_JIT_TL_SET_SINGLE_SATB"), off_key: None, off_word: Some("0"), since: "2026-10-21" },
    // JIT round 23 wave 2f (lane L4, INTRINSICS21-8 / W10L2-2): perf, default ON, kill "0". The inline resolver's
    // static-intrinsic-keeps-its-own-fast-path refusal skips a nested-only splice row (Math.abs(J)J inside the spliced
    // Math.multiplyExact(JJ)J body), so that splice engages (vm `jit_bridge::resolve_inline_site_from`).
    E { group: Group::JIT, token: "splice-nested-row-past-static-intrinsic", on_key: Some("CRATONVM_JIT_SPLICE_NESTED_ROW_PAST_STATIC_INTRINSIC"), off_key: None, off_word: Some("0"), since: "2026-10-21" },
    // JIT round 20 wave 11, lane pNat (L5W17-5, docs/feature-designs/jit-ir-optimizations.md): the shared fast-throw instance is handed out by reading its JNI global-ref slot without the table's mutex, detailMessage's slot from the per-VM cell. Default ON (inert unless fast throw is on), kill "0".
    // JIT round 20 wave 11, lane pNat (L5W17-5's positive control): count the unlocked fast-throw hand-outs and print "[fast-throw-handout] unlocked n=<count>" at every power of two. Diagnostic, off.
    // JIT round 23 wave 1b, lane L3 (R23W1L3-3): count the throwables each door built directly (the trap-frame door, create_exception_object, a compiled new X(msg)) against those built through their constructors, the plan walks and the remembered refusals, and print one "[direct-throwable] ..." line at exit. Diagnostic, off.
    // fast-throw hotness page (2026-10-10): with OmitStackTraceInFastThrow in effect, a compiled implicit-exception site the method-profile rule does not make hot still turns hot after FAST_THROW_HOT_THRESHOLD full throws (the pre-profile count). Default ON (inert unless fast throw is on), kill "0".
    // nio21 wave 16 L3: the loop class predicate admits a receiver tested only by trap-form guards. Default ON, kill "0".
    // nio21 wave 16 L3: a field read takes the value of an earlier read or store of the same cell across forward joins (value phi where paths differ). Default ON, kill "0".
    // nio21 wave 16 L4: an in-loop virtual site of an eagerly compiled method with no receiver profile seeds the profile from its inline cache's misses and asks one recompile (budget 512 per VM). Default ON, kill "0".
    // nio21 wave 16 L1: a publication makes the body it displaced not-entrant and forwards it to the new body, so callers that baked the old entry (C1 -> C1 and C1 -> C2) reach the successor and its entry counter. Default ON, kill "0".
    // r23 wave 10, lane L6: perf, default ON, kill "0". The stale-call rebind also re-points the optimizing tier's
    // `MOV RAX, imm64 ; CALL RAX` direct call (an 8-aligned imm64, one LOCK CMPXCHG) instead of declining it and
    // sending the caller to a full recompile (jit `not_entrant::stale_call_rebind_abs64_enabled`).
    // r23 wave 10, lane L6: perf, default ON, kill "0". The optimizing tier NOP-pads its direct cross call so the
    // `MOV RAX, imm64` immediate is 8-byte aligned, the shape the abs64 rebind can patch atomically (jit
    // `ir_lower::ir_direct_call_imm_align_enabled`).
    // r23 wave 10, lane L6: perf, default ON, kill "0". The single-pass backend NOP-pads a CALL rel32 of compiled
    // code so its displacement is 4-aligned (never straddling an 8-byte word), the shape the stale-call rebind patches
    // (jit `not_entrant::sp_patchable_call_align_enabled`).
    // JIT round 21, lane typecheck (TYPECHECK21-8, page jr21-typecheck-lane-perf-rows-20261014): the optimizing OSR memo forgets a body built while a class whose statics it reaches through the helpers was not initialized, once that class is, so the door rebuilds it with direct accesses (the OSR twin of static-init-recompile). Opt-in ("1") since the w1f A/B lost (pending set read a lazy memo; fixed to init_state, re-measure).
    // nio21 wave 15 L1: a statically bound instance site without a direct row gets an IR inline cache the helper fills with the current compiled body. Default ON, kill "0".
    // nio21 wave 15 L2: the interpreter's receiver / call-site profile memo is an 8-way per-thread table instead of one entry. Default ON, kill "0".
    // nio21 wave 12 L2: the constant-handle inline admits subclass receivers and subclass values through a per-VM primary-supers display. Default ON, kill "0".
    // JIT round 23 wave 1, lane L9 (W14L4-1 of `jit-r21-nio-proposals-w14-L4.md`): perf, default ON, kill "0". A class's
    // supers display is published when its <clinit> starts (hierarchy-only verdicts only), so instances its own <clinit>
    // creates meet a display at once (vm `jit::helpers::vh_const_publish_display_at_init_start`).
    // nio21 wave 12 L4: Math.multiplyExact(JJ)J and abs(J)J splice their JDK body. Default ON since nio21 wave 14 (abs(J)J nested-only; w12 host mx1 arm matched HotSpot in both modes), kill "0".
    // nio21 wave 12 L4: the self-copy, synchronized-splice, cannot-throw and callee-handler screens admit a division by a non-zero constant (the handler plan also a guarded variable divisor). Default ON, kill "0".
    // nio21 wave 12 L5: a ZGC inline allocation that refuses the inline start bit calls zgc_note_tlab_object before committing the TLAB cursor (both tiers; single-pass needs a cached thread slot). Default ON, kill "0"; off whenever CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT=0.
    // nio21 wave 11 L3: compile time primes a linked non-capturing lambda site's constant word (no first bridge call needed). Default ON, kill "0".
    // nio21 wave 11 L3: the optimizing tier splices a callee whose invokedynamic sites are linked non-capturing lambdas with a published constant word. Default ON, kill "0".
    // nio21 wave 10 L2: Math.floorDiv / floorMod (II)I and (JJ)J splice their JDK body in place of the Intrinsic native. Default ON, kill "0".
    // nio21 wave 10 L2: a variable divisor's zero guard may stand inside a splice (resumes through the callee frame or re-runs the call). Default ON, kill "0".
    // JIT round 20 wave 11 lane pSplice (jit-r21-nio-proposals-w10-L2.md W10L2-1, SPLICE20-6): the IR inline resolver admits a division by a variable divisor in EVERY spliced body, not only the reviewed Math.floorDiv/floorMod/multiplyExact rows (the guard stands under ir-splice-div-guard's resume rule). Default ON, kill "0".
    // nio21 wave 10 L2: a division by a non-zero constant plants no zero guard and a callee with one is spliced. Default ON, kill "0".
    // nio21 wave 10 L3: a compiled non-capturing lambda site reads its singleton through the site's constant word (the interpreter's LAMBDA_SINGLETON_CACHE slot) instead of calling jit_indy_bridge. Default ON, kill "0".
    // nio21 wave 7 L3: Unsafe CAS / getAndAdd on a compact instance int/long field are inline through a per-VM field-cell table. Default ON, kill "0".
    // nio21 wave 7 L5: loop field forwarding also runs for loops whose class guard the length predicate hoisted. Default ON, kill "0".
    // JIT round 20 wave 19 lane yBuf (MEM20-4 / LOOPOPT20-5): with ir-loop-field-forward-guard-hoisted off, the forward still runs for the guard-hoisted loops whose ScopedMemoryAccess folds all have a heap base (heap ByteBuffer loops); direct-buffer loops are left. Default ON, kill "0".
    // nio21 wave 7 L4: bimorphic / polymorphic guarded bodies of a hot site share the hot cap. Default ON, kill "0".
    // nio21 wave 7 L4: a scoped access whose base is a ByteBuffer.hb [B read skips the primitive-array screen. Default ON, kill "0".
    // JIT round 20 lane memory (r20-heapbb-twelve-x-residual-is-guards-not-calls): the ByteBuffer.hb fact also covers the compiling method's own hb reads and a phi of them. Default OFF since wave 2 (host A/B: no gain), opt-in "1".
    // nio21 wave 7 L4: a static call the splice lowers as a scalar op (Math.min ...) no longer refuses the body. Default ON, kill "0".
    // nio21 wave 6 L5: opt-in. The unroller admits a loop whose masked loads cannot deopt and drops the fenced loads.
    // nio21 wave 6 L5: a charAt trap withdraws its class predicate per site, not per method. Default ON; "0" restores the per-method rule.
    // nio21 wave 6 L5: check elimination proves a[i + o] in range from the offset predicate's guards. Default ON, kill "0".
    // nio21 wave 6 L5: loop-entry guards for a[i + o] accesses with a loop-invariant offset. Default ON, kill "0".
    // JIT round 20 lane loopopt (r21-encode-loop-field-hoist-and-buffer-bce-need-a-call-free-loop): a trap-form Guard(v < x) is a loop-bound fact for the monotone rule and the variable-offset BCE arm (strictly after the guard). Default ON, kill "0".
    // JIT round 20 lane loopopt (R20ReviewForwardNullBase): default ON since JIT round 24 (the pre-header read names its null guard as its licence), kill "0". The loop field forward also forwards a base not known non-null, behind a pre-header null guard.
    // nio21 wave 6 L4: reviewed intrinsic JDK bodies that make calls (String.regionMatches) splice too. Default ON, kill "0".
    // JIT round 20 wave 17, lane pStr3 (handoff row `R20PIRCodePoint` cp-iterate): a
    // compiled caller may splice the JDK body of `String.codePointAt(I)I` over its
    // blanket `Intrinsic` native (a native-funnel call per code point). Default ON.
    // nio21 wave 6 L4: a nested body the planner will refuse is bound as an ordinary call so its parent can still splice. Default ON, kill "0".
    // nio21 wave 6 L4: a never-run static call in a spliced body takes the precise ClassLoading exit instead of refusing the body. Default ON, kill "0".
    // nio21 wave 6 L4: a profile-hot guarded splice site takes a 325-byte body cap (budgeted) instead of 35. Default ON, kill "0".
    // JIT round 20 lane splice (r21-profile-guarded-splice-caps-hot-sites-at-the-cold-size): a NESTED profile-guarded row whose enclosing body's receiver row has >= 256 samples takes a share of the 325-byte cap instead of 35. Default ON, kill "0".
    // rx27 wave 3 lane bimodal: determinism, OPT-IN "1". The nested hot cap reads a row's frequency (samples per interpreted entry of
    // its enclosing body, C2's InlineFrequencyRatio) instead of its sample count, which moved with how long that body ran interpreted.
    // rx27 wave 3 lane bimodal: perf, OPT-IN "1". A contradicted nest-exact row is dropped at every site (not only chain sites), so the
    // profiled route plans the class the evidence names (Matcher.match pc 121 -> BmpCharPropertyGreedy.match in regex-matches).
    // rx27 lane bimodal: perf, OPT-IN "1". The IR frame block is published as ONE descriptor record (rbp, count, packed home offsets) that every shadow-window reader expands.
    // JIT round 20 lane splice wave 4: a nested guarded body over 35 bytes takes the nested hot cap only when it is a leaf (no call, no loop). Default ON, kill "0".
    // JIT round 20 wave 11 lane pSplice (jit-r21-nio-proposals-w7-L4.md W7L4-5): a nest-exact row on a hot pc of its enclosing body (entry prefix or >= 256 samples) takes the nested hot cap when its tree holds no loop (calls allowed; profiled rows keep the leaf rule). Default ON, kill "0".
    // JIT round 20 lane splice wave 4: a nested profiled row whose callee profile is polymorphic (>= 32 samples, >= 2 classes, none dominant) does not fall back to the compiled cache's class. Default ON, kill "0".
    // nio21 wave 6 L2: getfield / putfield NPEs inside a try get implicit edges (under CRATONVM_JIT_IR_IMPLICIT_EXCEPTION_EDGES, default ON). Default ON, kill "0".
    // nio21 wave 5 L2: an implicit NPE / AIOOBE / ArithmeticException inside a try whose handler is in the same IR body branches to it. Default ON since WALKTHROW21-7 (jit-r21-walkthrow-proposals.md: an OSR body otherwise threw every such exception in the interpreter, and its method-entry body went not entrant at the guard), kill "0".
    // t1 (lcap, kernel q): an implicit exception edge's optional pad alone does not make a body take the VM context (ir_lower scan_frame_needs), so a heap-free body keeps its C1 twin's entry ABI. Default ON, "0" restores the reservation.
    // nio21 wave 5 L5: opt-in. The mask predicate is planted before the loop rounds.
    // nio21 wave 5 L5: a CharSequence.charAt call in a class-predicated loop deopts on a String-prefix miss instead of calling, so the loop is call-free. Default ON, kill "0".
    // nio21 wave 5 L4: a receiver-forwarding bridge's nested exact call takes the hot size cap, not the 35-byte cold cap. Default ON, kill "0".
    // JIT round 20 wave 11 lane pSplice (jit-r21-nio-proposals-w5-L4.md W5L4-5): the forwarder shape also takes an invokeinterface on this (a default method that re-dispatches), and the VM resolves that nested row for the enclosing body's exact receiver. Default ON, kill "0".
    // nio21 wave 5 L4: a reviewed Intrinsic-kind native whose JDK body is a leaf (Boolean.booleanValue, Character.charValue; StringLatin1.getChar until 2026-10-13, now plain bytecode) splices its bytecode instead of refusing the body. Default ON, kill "0".
    // r23 wave 10 L1: correctness fix inside the opt-in callee-handler family. The always-failing handler-entry trap ends its block with an unreachable Throw, so lowering no longer refuses the try/catch(Throwable) callee (CompletableFuture.uniApplyNow). Default ON, kill "0".
    // r23 wave 11b L5: perf, default ON, kill "0". A store at a synthetic field pc (the VH fresh-field set*) takes its node's own
    // compact row (Graph::field_rows) like the load arm, instead of jit_putfield_object every time (CfLoop done 129 vs 80 ns/op).
    // r23 wave 12 L5: perf, OPT-IN "1" (lost on n23-w12a: SplitParts cached/full slower on). A depth-0 splice whose in-loop virtual call its own artifact guard-splices, and the
    // nested resolution cannot reproduce, stays a call (regex Start.match into Matcher.search: one call per find, not one per position).
    // rx27 lane bimodal: perf, default ON, kill "0". The keep-loop-splice-caller rule does not count a String row the copy answers with the
    // inline String prefix (chain-prefix-kept) as lost, so its answer no longer flips with which body of the callee is published.
    // rx27 lane bimodal: perf, default ON, kill "0". Nor a row at a site under C2's InlineFrequencyRatio of the body's entries (the chain
    // rule's frequency verdict): its call is paid per entry, not per iteration.
    // r23 wave 13 L5: perf, default ON, kill "0". The fresh-receiver publication walk passes checkcast / instanceof on the memory chain
    // (they publish nothing), treats a checkcast of the allocation as the allocation; only the opt-in VH-fresh / volatile-fresh paths ask it.
    // r23 wave 10 L1: a spliced branch whose two arms are both masked cold throw arms (fall-through arm plus tail arm, CompletableFuture.reportJoin) traps whole at the branch instead of refusing at the tail athrow. Acts only under the opt-in CRATONVM_JIT_IR_SPLICE_ATHROW_EXIT=1 (and CRATONVM_JIT_IR_COLD_THROW_TAIL_ARMS). Default ON, kill "0".
    // nio21 wave 4 L5: soundness fix, a class test reached only as an input still refuses the forward. "0" restores the old (unsound) answer for bisecting only.
    // JIT r22 nio hand-ins (W19L3-2): opt-in. The loop field forward runs over every loop of a method-entry body (a header it gives a value phi loses its OSR entry stub).
    // nio21 wave 4 L5: a loop-invariant receiver tested by a one-class guarded splice gets one pre-header class guard; in-body class and null tests fold. Default ON, kill "0".
    // nio21 wave 4 L5: check elimination proves p+k in range from a guarded room test (x - p >= k). Default ON, kill "0".
    // JIT round 20 wave 11, lane pAtom (W7-L3-1, re-filed AT-3 in docs/feature-designs/jit-ir-optimizations.md):
    // volatile int/long Unsafe get/put of a compact instance field the VM published in the field cell table is one MOV / XCHG in both tiers. Default ON, kill "0".
    // t1 wave 9, lane ATOM2 (JIT round 21 proposal ATOMICS21-1, book docs/feature-designs/jit-ir-optimizations.md): perf, OPT-IN "1" (no A/B yet).
    // Unsafe {get,put}{Int,Long}Volatile (and Acquire/Opaque/Release) on a static (staticFieldBase mirror + static offset) whose STATIC_RMW entry is published is one MOV / XCHG of the static's payload word in both tiers' accessor prefixes.
    // JIT round 21, lane staticfwd (OI-4 in docs/feature-designs/jit-ir-optimizations.md, page docs/feature-designs/jit-ir-optimizations.md):
    // a plain primitive putstatic's value answers a later getstatic of the same static in the IR builder's static memo (no write, volatile access, monitor or call between). Default ON, kill "0".
    // JIT round 21, lane staticfwd (FG16-3, same page): a spliced callee body may carry a plain non-volatile putstatic of a class initialised at resolve time, committed behind the replay fence (was refused as ir-splice-putstatic). Default ON, kill "0".
    // JIT round 23 wave 9, lane W9-L2 (CF-W9-1): default ON (perf; kill switch "0"). The Unsafe
    // field-cell table is two-way: a key whose first cell another field owns publishes at its
    // second cell and the compiled screen probes both (jit `direct_helpers`
    // `unsafe_field_cell_two_way_enabled`; vm `jit_realm.rs` `publish_cell`).
    // JIT round 23 wave 1, lane L9 (W4-L4-3 of `jit-r21-nio-proposals-w4-L4.md`, the reference half): perf, default ON,
    // kill "0". Unsafe.getAndSetReference* on a published compact reference field is one gated XCHG in both tiers, and
    // the optimizing tier keeps its JDK body a call where that arm serves it (jit
    // `direct_helpers::unsafe_ref_get_and_set_field_enabled`).
    // r21 updaters (UPDATERS21-1): getReferenceVolatile / putReferenceVolatile (and the acquire / opaque / release bodies) on a published
    // compact reference field as one MOV / XCHG in both tiers, a put behind the gated reference-store screen (jit
    // `direct_helpers::unsafe_ref_volatile_field_enabled`). Perf: opt-in.
    // r21 updaters (UPDATERS21-2): the AT-5 Unsafe RMW-body splice veto applies only when the compile bakes a field arm
    // (jit `direct_helpers::unsafe_rmw_veto_needs_field_arm_enabled`). Perf: opt-in.
    // r21 updaters (UPDATERS21-3): a never-run private instance call in an optimizing-tier splice is a precise exit, not a splice
    // refusal (jit `ir::ir_splice_never_run_private_exit_enabled`, vm `jit_bridge::never_run_private_exit_pcs`). Perf: opt-in.
    // r21 updaters (wave 3): a constant-VarHandle CAS / getAndAdd / getAndSet / getAndBitwise whose result is discarded (a `...)V`
    // descriptor) is an inline shape (jit `ir_lower::vh_const_void_rmw_enabled`). Perf: opt-in.
    // r21 updaters (UPDATERS21-5): a static final arrayElementVarHandle(Object[].class) GET mode as the element load behind the
    // reference-array / index screens, a narrower return as a cast screen (jit `ir_lower::vh_const_ref_array_get_enabled`;
    // the VM files the plan only for an Object[] arrayType). Perf: opt-in.
    // r21 barriers (BARRIERS21-2): under G1 the optimizing tier's reference aastore stores inline while the pre-gate byte is clear
    // (null or same-region value: no barrier; cross-region: the lean G1 remembered-set call) instead of the whole jit_aastore call
    // (jit `ir_lower::ir_g1_gated_aastore_enabled`). Perf: default ON since the r23 A/B (R23-AB-FLIP), kill "0".
    // r21 splice (JIT round 21 wave 2, lane splice; probe `R21SpliceRows.java`): planning defect,
    // default ON, kill "0": CRATONVM_JIT_IR_INLINE_MAX_NEST is clamped so a planned chain never opens more splice frames
    // than the builder's MAX_IR_SPLICE_DEPTH (a deeper chain refused the whole build; jit `ir_inline_max_nest_depth`).
    // r21 splice (wave 3): perf, opt-in. A guarded splice of a package-private override whose JVMS selection answers
    // Lenient is decided on the receiver's superclass chain (vm `jit_bridge::package_private_override_by_class_chain`).
    // r21 splice (wave 3): perf, opt-in. An unprofiled own virtual / interface site of an optimizing compile is guarded on
    // the class-hierarchy single implementor, the call as the miss edge (jit `ir_cha_guarded_splice_enabled`).
    // r21 splice (wave 8): trace defect, default ON, kill "0". A stack walk whose innermost compiled frame belongs to a
    // body made NOT ENTRANT after the frame was built names it by decoding its caller's direct call, instead of dropping
    // the frame (vm `conservative_roots::trace_innermost_by_caller_call`).
    // r21 trace (wave 14): trace correctness, both modes, default ON, kill "0". A constant-pool splice whose selected
    // body is declared above the constant-pool class (an inherited default reached through invokevirtual K3.d1) names the
    // declaring class for its frame label, class id and dependency, so the frame reads A.d1:110, not K3.d1:-1
    // (vm `jit_bridge::splice_declaring_class_frame_enabled`).
    // r21 hidden-resid (wave 22, page r21-review-w15-native-findings-FIXED-20261020.md item 6): opt-in. The compiled hidden-self
    // guard rules out other names from a per-VM lock-free table of recorded class-file name hashes before the lock.
    // r21 lookup-in (wave 16, page r21-review-w10-findings-FIXED-20261020.md residual 3R): correctness, both modes, default ON,
    // kill "0". Lookup.in's getEnclosingClass walk loads an enclosing class that is not loaded yet through the inner class's
    // defining loader (HotSpot klass_at), so a spoofed/hidden pair throws the ICCE and a missing outer class its
    // NoClassDefFoundError (native-builtins classloader::lk_in_loads_outer_enabled).
    E { group: Group::COMPAT, token: "lookup-in-loads-outer", on_key: Some("CRATONVM_LOOKUP_IN_LOADS_OUTER"), off_key: None, off_word: Some("0"), since: "2026-10-19" },
    // r21 jit-selfcopy (wave 16, page r21-review-w15-jit-findings-FIXED-20261020.md item 4): opt-in. The IR self-copy predicate
    // compares the nested static call's resolved declaring class id with the site's class id (a subclass-qualified self call
    // is self); an unknown id falls back to the name compare (jit `ir_self_copy_by_id_enabled`).
    // r21 deopt-resume (wave 16, page r21-review-w15-jit-findings-FIXED-20261020.md item 5): correctness, default ON, kill "0".
    // With no scope hint, deopt resume takes the enclosing class by id when the key names it, else resolves the declaring
    // class through the constant-pool classes' defining loaders before failing closed (vm `deopt_resume_scope_by_loader_enabled`).
    // r21 finalize-reinit (wave 16, page r21-review-w10-findings-FIXED-20261020.md residual 4R): correctness, default ON, kill
    // "0". A finalizable object withdrawn after a throwing constructor is registered again (once) at a later successful
    // interpreted <init> return, and a constructed row is never withdrawn (read once per VM into
    // `VmConfig::finalizer_reregister_at_init_return`, which arms the interpreter's 0xb1 gate bit and the reference processor).
    // r21 perf-cf-lean (wave 16, page r21-lean-allocation-rows-cost-is-outside-the-inline-sequence-FIXED-20261024.md): opt-in.
    // The IR inline TLAB bumps prefetch ahead of the new top (HotSpot AllocatePrefetchStyle=1; PREFETCHW when supported).
    // r22 memory
    // (wave 1, ALLOC21-1 of jit-r21-alloc-tier1-proposals-RETIRED-20261025.md): opt-in. On Generational / G1 the optimizing tier's inline
    // `newarray` / `anewarray` bump emits its safepoint map on the slow path only, as `new` does (ALLOC_MAP_SINK).
    // (wave 2, MEMORY22-1 of jit-r22-memory-proposals.md): opt-in. On ZGC the announced inline `new` / `newarray` bump
    // (with ALLOC_MAP_SINK / NEWARRAY_MAP_SINK) emits its map on the slow path only; the announce helper is a leaf call.
    // (wave 2, W6-L2-4 of jit-r20-nio-proposals-w6-L2.md): correctness, default ON, kill "0". ZGC `is_heap_addr`'s unordered arms
    // (Hash kill switch, a spilled bitmap) take the total nearest-base lookup instead of a walk bounded by `used()`.
    // (wave 3): HotSpot parity, default ON, kill "0". A deopt rebuilds an eliminated Integer / Long box without running
    // Java: the VM's box cache slot (installed when empty) or a fresh box (vm `deopt_materialize::deopt_box_no_java_enabled`).
    // (wave 8, page r22-w7-flipgcrows-nojit-g1-oom-20261020.md): HotSpot parity, default ON, kill "0". G1's staged OOME-path
    // compaction also continues after a stage that shrank the Old extent by an eighth of a region without freeing one.
    // (wave 9, same page): HotSpot parity, default ON, kill "0". G1's forced full cycle renews its 2 s budget while other
    // initiators' collections complete (6 s cap since wave 10), instead of giving up on a loaded host.
    // (wave 11, same page): HotSpot parity, default ON, kill "0". When G1's staged OOME-path compaction stops, one last
    // maximal mode takes every Old region with any garbage (ignoring the 1/8-region floor) before the OOME is thrown.
    // (wave 11, same page): opt-in diagnostic. Read once per VM; when the allocation-failure ladder throws an OOME it
    // prints ONE `[cratonvm] oome-ladder:` line naming each step, its region census, and why the loop ended.
    // JIT round 23 wave 1 L7 (same page): opt-in diagnostic, read-only. Per root KIND (each collect_roots step, each
    // thread's snapshot, the frozen peers, finalizable candidates, published JIT pins) the objects and bytes it reaches
    // first, its top classes, the bytes only it keeps, the ten largest single roots and, on G1, reached vs used per region;
    // printed as `[root-census]` lines at failure / explicit / G1 mark pauses (`=all`: every collection pause).
    E { group: Group::DBG, token: "root-kind-census", on_key: Some("CRATONVM_DBG_ROOT_KIND_CENSUS"), off_key: None, off_word: None, since: "2026-10-21" },
    // (wave 12, same page): HotSpot parity, default ON, kill "0". The compiled `new` / `anewarray` slow paths consult a
    // latched GC-overhead streak only after an allocation attempt failed, as the interpreter's ladder does.
    // r22 review (wave 11, page r22-w10-full-cycle-budget-renews-on-any-completed-pause-FIXED-20261020.md): correctness, default
    // ON, kill "0" (the wave-10 Free/Old measure). The full-cycle budget renews only when the tenured footprint (Old +
    // Humongous, regions or bytes) shrank, so a young pause that empties Eden into Free no longer counts as progress.
    // r21 cf (wave 22, CF page): default ON since R24-KPC-FLIP (kill "0"). The optimizing tier skips the StoreLoad fence
    // after a volatile putfield into an object the compile allocated, when no escaping use can run between the
    // allocation and the store.
    // JIT round 23 close (the zki volatile-store row of the r21 lean-allocation page): perf, default ON, kill "0". A
    // volatile store's StoreLoad fence stands before the next node that needs it (a volatile load, call, guard, return,
    // poll slow path) instead of right after the store, so a run of volatile static stores pays one fence (jit
    // `ir_lower::Lowerer::ensure_fence_sink_plan`).
    // r21 perf-cf-lean (wave 16, page r21-completablefuture-stage-cost-is-allocation-and-calls-20261018-RETIRED-20261024.md): opt-in. While
    // the SATB pre-gate is set, the constant-VarHandle reference store stays inline when the old value is provably null.
    // r21 vhconst (wave 22, page r21-vh-const-refuses-cast-and-statement-sites-FIXED-20261020.md, UPDATERS21-6): opt-in. A
    // reference compareAndExchange in a cast context runs inline; the witness's cast is decided from one read before any write.
    // r21 vhconst (wave 22, same page, UPDATERS21-5): opt-in. arrayElementVarHandle(Object[].class) store modes run inline
    // behind the barrier gates and the covariant store check; the VM files the Object[] plan under it.
    // r21 perf-cf-lean (wave 16, CF page): opt-in. The IR tier skips a void instance call whose entry returns at once
    // while one reference field of `this` is null (CompletableFuture.postComplete with stack == null); VM-admitted per target.
    // r21 jit (wave 17, page r21-review-w16-findings-FIXED-20261020.md item 6): opt-in. A finalizable-subtype define withdraws
    // ancestor constructors by declaring class id (spliced bodies through copied_classes), not by name across loaders.
    // r21 finalize (wave 18, page r21-review-w16-findings-FIXED-20261020.md item 2, perf half): opt-in. A finalizer-bar
    // refusal row stamped with another redefine epoch is ignored and the bar asked again (JitRealm::finalizer_bar_refusals).
    // r21 finalize (wave 22, page r21-finalize-reinit-residuals-FIXED-20261020.md item 2): a JNI <init> call on an already
    // registered finalizable object registers one more Finalizer (finalize() runs again), as HotSpot does.
    // r21 finalize (wave 23, page r21-review-w23-findings-FIXED-20261020.md items 2-3): the JNI re-<init> count follows the
    // receiver through a collection and is skipped for failures that cannot have entered the constructor; =0 wave-22 rule.
    // r21 finalize (wave 22, page r21-review-w21-findings-FIXED-20261020.md item 8): opt-in. The <init>-return receiver scan is
    // decoded once per method (memo on the quickened code) instead of on every finalizable constructor return.
    // r21 initwitness (wave 25, page r21-review-w24-findings-FIXED-20261020.md item 2): a failed JNI re-<init> counts its second
    // Finalizer when the receiver row's Object.<init>-return witness moved during the call; =0 wave-24 error-kind rule.
    // r21 finprologue (wave 26, page r21-fuzz8-probes-FIXED-20261020.md item 4): the throw-before-initialization proof accepts a
    // JEP 513 prologue handler whose rows cover only pcs before the initializing call; =0 the handler-pc-only rule.
    // r22 review (page r21-review-w24-findings-FIXED-20261020.md item 2): a JNI re-<init> of a registered finalizable object runs
    // interpreted with every frame kept, and Object.<init>'s own return moves the witness; =0 the silent witness.
    // r22 review (wave 2; page r21-review-w24-findings-FIXED-20261020.md item 2, the per-object imprecision): a witness that moved
    // while a nested JNI re-<init> of the same object ran proves nothing; =0 trusts any moved witness.
    // r22 review (wave 2; page r22-unload-withdrawal-ignores-lost-copy-record-FIXED-20261020.md): an unload also withdraws a body
    // whose compile lost a copy record when an unloaded class was ever copied on the cache; =0 ignores the flag.
    // r23 splice (wave 1, lane L2; INLINE22-4 = RUNTIME22-3; probe C:\craton\n23p\w1L2\W1L2LambdaSam.java): perf, default ON,
    // kill "0". A profiled lambda proxy receiver's SAM call is spliced as the proxy's forwarding body (capture reads, casts,
    // the implementation spliced in turn) behind ExactClassIs(proxy id) instead of refused (jit `ir_lambda_sam_splice_enabled`,
    // vm `jit_bridge::resolve_ir_lambda_sam_site`; census `CRATONVM_DBG_JITC=1 | grep lambda-proxy-census`).
    // t1 wave 11 (lane JLAM, LAMBDA221-2 remainder): perf, default ON since the j27 flip of 2026-10-28, kill "0". The lambda SAM splice's forwarder screen
    // admits an ARRAY checkcast (`Function<String[], R>`) as a typecheck row instead of refusing "an array cast" (jit
    // `ir_lambda_sam_splice_array_casts_enabled`, vm `jit_bridge::lambda_forwarder_rows_with`).
    // r23 splice (wave 2, lane L2): profile defect, default ON, kill "0". The interpreter's lambda door and general lambda
    // arm record the proxy receiver in the call site's receiver profile (vm `VmConfig::lambda_receiver_record`); before,
    // no profile ever named a proxy, so the lambda SAM splice never fired (wave-1 census: 0 lines).
    // r23 (wave 2c, lane L2; page r23-w2-boxing-method-ref-call-costs-microseconds-FIXED-20261021.md): perf, default ON, kill "0".
    // A lambda adapter's boxing calls the reviewed Integer/Long.valueOf Intrinsic directly (and an in-range Integer from the
    // box cache) instead of a by-name invoke_shared per call (vm `interpreter::box_value_of_intrinsic`).
    // r23 (wave 3b, lane L2): perf, default ON, kill "0". The compiled lambda arm boxes an adapted site's int return with
    // the compiled Integer.valueOf helper's body on the thread it holds (box cache / TLAB bump), not the native funnel
    // (vm `jit::helpers::lambda_site_adapt_return`, `VmConfig::lambda_arm_tlab_box`).
    // r23 (wave 3c, lane L2; L2W3B-1): perf, default ON, kill "0". A constructor reference (Foo::new) enters <init> through
    // the per-site memoised body (vm `interpreter::lambda::try_invoke_cached_lambda_impl`) after the unchanged init check
    // and allocation, instead of a by-name invoke_on_class_shared per call (vm `lambda_ctor_ref_shape_enabled`).
    // r23 (wave 4b, lane L2; L2W4B-1): perf, default OFF, opt-in. The optimizing builder reads a resolved reference condy
    // ldc from its slot (jit `ir_ldc_condy_slot_enabled`), and an invokeExact whose receiver is such a constant DirectMethodHandle
    // is compiled as a call of the handle's own member, bound by class id (vm `mh_constant_condy_enabled`, `mh_condy_answer`).
    // r23 w4 review (lane L2): correctness, default ON, kill "0". A REF_invokeSpecial lambda forwarder whose member is declared by
    // another class than the proxy host throws HotSpot's WrongMethodTypeException (JDK-8172817) instead of calling the member
    // (vm `interpreter::lambda::lambda_special_forwarder_type_refusal`).
    // r23 (wave 5, lane L2): perf, default ON, kill "0". An unboxing guard inside an opaque depth-0 lambda forwarder that has
    // committed nothing resumes at the outermost invoke, like the forwarder's checkcast (jit `IrBuilder::unbox_resumes_flat_in_forwarder`).
    // r23 (wave 5, lane L2): perf, default ON, kill "0". A forwarder's kept Number.xxxValue() on a checkcast-proved box class is
    // planned as the box class's own method, statically bound (vm `resolve_ir_lambda_sam_site`).
    // r23 (wave 5, lane L2; default ON since wave 6, kill "0"): perf. A body whose one-proxy SAM call would stay a call when spliced
    // at depth 0 is kept as a call, so the caller reaches the body's own optimizing artifact (vm `resolve_inline_site_from`).
    // R23-W6-FLIP (JIT round 23 wave 6, lane L2): default ON, kill "0" (BoxRef forwarder splice calls boxed's optimizing body).
    // r23 (wave 6, lane L2): correctness of a compile-time answer (determinism), default ON, kill "0". jit_well_known_class_id's
    // Integer / Long class ids are memoised per VM, so a compile racing a class definition no longer bakes no inline box (vm `well_known_box_class_memo`).
    // r21 innerclasses (wave 19, page r21-review-w18-findings-FIXED-20261020.md items 3, 4, 7): correctness, both modes, default
    // ON, kill "0". --compatible nesting kind / simple / canonical names follow EnclosingMethod as JDK 25 does; reflection
    // fills are guarded by a per-VM inner-tables generation bumped at a redefinition swap; a failed outer-class resolution
    // is recorded per VM and rethrown without asking the loader again (JVMS 5.4.3).
    // nio21 wave 3 L4: Reference/PhantomReference.refersTo0 take thin direct helpers. Default ON, kill "0".
    E { group: Group::JIT, token: "refers-to-direct", on_key: Some("CRATONVM_JIT_REFERS_TO_DIRECT"), off_key: None, off_word: Some("0"), since: "2026-10-08" },
    // nio21 wave 3 L4: Unsafe.fullFence takes a thin direct helper. Default ON, kill "0".
    E { group: Group::JIT, token: "unsafe-fence-direct", on_key: Some("CRATONVM_JIT_UNSAFE_FENCE_DIRECT"), off_key: None, off_word: Some("0"), since: "2026-10-08" },
    // nio21 wave 3 L4: Unsafe putReferenceVolatile / compareAndSetReference take thin direct helpers (the native's own barriers). Default ON, kill "0".
    E { group: Group::JIT, token: "unsafe-ref-store-direct", on_key: Some("CRATONVM_JIT_UNSAFE_REF_STORE_DIRECT"), off_key: None, off_word: Some("0"), since: "2026-10-08" },
    // nio21 wave 3 L4: Unsafe get/put Int/Long volatile take thin direct helpers. Default ON, kill "0".
    E { group: Group::JIT, token: "unsafe-volatile-direct", on_key: Some("CRATONVM_JIT_UNSAFE_VOLATILE_DIRECT"), off_key: None, off_word: Some("0"), since: "2026-10-08" },
    // nio21 wave 3 L5: opt-in. A planted mask predicate proves a masked array access in range at lowering.
    // nio21 wave 3 L5: check elimination proves a monotone induction stays >= 0 (entry >= 0, steps bounded by a guarded limit). Default ON, kill "0".
    // JIT round 23 wave 7b (lane L1): perf, default ON, kill "0". An `a[i]` under `i < a.length` gives its φ's entry value a
    // pre-header `0 <= init` guard (OSR bodies: the entry merge φ), and the planter runs on the OSR route for loops entered
    // only through their pre-header, so the in-loop bounds check goes.
    // nio21 wave 3 L3: the single-pass Unsafe prefix also serves a null base (raw pointer / arena handle). Default ON, kill "0".
    // JIT round 20 lane memory (r20-unsafe-accessor-inline-is-optimizing-tier-only): the single-pass Unsafe dispatch prefix also serves the big-endian four-operand *Unaligned forms. Default OFF since wave 2 (no JDK caller reaches the dispatch arm), opt-in "1".
    // nio21 wave 3 L3: a compile whose only planned byte-buffer site is a put keeps the call as its miss edge (no trap). Default ON, kill "0".
    // nio21 wave 2 L3: global / auto / implicit / heap model sessions carry state 2 (non-closeable; close throws HotSpot's UOE). Default ON, kill "0".
    // nio21 wave 2 L3: the scoped fold passes a model session marked non-closeable (global / auto). Default ON, kill "0".
    // nio21 wave 2 L3: the field-load memo forwards a final, non-volatile instance field read within one IR body. Default ON, kill "0".
    // nio21 wave 2 L2: opt-in. The single-pass tier arms its local handlers without precise exception frames when the handler reads only parameters.
    // nio21 wave 1 L3: the scoped fold admits a null base and resolves a tagged Unsafe-arena address through the arena's JIT table. Default ON, kill "0".
    // nio21 wave 1 L5: scalar intrinsic rows (surrogate tests) inside nested splices no longer refuse the splice. Default ON, kill "0".
    // JIT round 16 wave 17, lane fieldsite3 (FS16-4): default OFF. The trivial-getter fast path memoizes
    // its descriptor verdict per thread instead of taking the class_manager read lock per call
    // (vm `dispatch_virtual::try_execute_cached_trivial_instance_getter`); the DBG row prints fills/hits.
    E { group: Group::JIT, token: "trivial-getter-desc-memo", on_key: Some("CRATONVM_TRIVIAL_GETTER_DESC_MEMO"), off_key: None, off_word: Some("0"), since: "2026-10-03" },
    // zt t25/hunt8: a compiled getfield/putfield helper refuses an ARRAY receiver (getfield: miscompile NPE; putfield: store dropped) instead of striding packed elements as 16-byte legacy cells (vm `jit::helpers::jit_field_receiver_is_array`). Default ON, kill "0".
    E { group: Group::JIT, token: "field-array-receiver-screen", on_key: Some("CRATONVM_JIT_FIELD_ARRAY_RECEIVER_SCREEN"), off_key: None, off_word: Some("0"), since: "2026-10-20" },
    // JIT round 25 (W11-7 review fix): the W10-3 nest-depth raise and 24-byte
    // leaf rule hold only in a dispatch-chain splice tree.
    // JIT round 21 lane conf, CONF21-2 (page `jr21-conf-string-identity-differs-only-from-hotspot-c2-FIXED-20261027.md`): the
    // optimizing tier also fuses a `new StringBuilder()...toString()` chain whose result may be empty (no append, or
    // only String operands), so it answers a fresh String as C2's OptimizeStringConcat does (jit `sb_chain_fuse.rs`).
    // HotSpot conformance, default ON, kill "0".
    // JIT round j27 (2026-10-28), wave 2.
    // Round rx27, lane splitfast: `String.substring(II)` / `String.subSequence(II)` (and, under its own key,
    // `CharSequence.subSequence(II)` on a String) answered by an allocating optimizing-tier prefix, the site kept a call. Default ON (rx27 A/B), `=0` kills.
    // Round rx27 wave 3, lane splitfast: the static `Arrays.copyOfRange(byte[], int, int)` answered by an allocating
    // optimizing-tier prefix, and `ArrayList.add(Object)` by an inline prefix where the JDK `ArrayList` bytecode runs. Opt-in.
    // Opt-in, default OFF: native branch observations for C1/single-pass bodies.
    E { group: Group::JIT, token: "osr-live-stack-retire", on_key: Some("CRATONVM_JIT_OSR_LIVE_STACK_RETIRE"), off_key: None, off_word: Some("0"), since: "2026-10-28" },
    // t1misc (NBD-3): perf, default ON, kill "0". The single-pass twin of CRATONVM_JIT_IR_IC_KIND_SCREEN_ELIDE: with "0" every
    // single-pass guarded-inline chain and MIC/PIC cascade keeps its KIND_TAGS screen whatever the static owner (jit
    // `x64::inlining::sp_ic_kind_screen_elide_enabled`, read by `static_receiver_admits_arrays`).
    E { group: Group::JIT, token: "ir-splice-putstatic-loop-replay", on_key: Some("CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_REPLAY"), off_key: None, off_word: None, since: "2026-10-28" },
    E { group: Group::JIT, token: "ir-loop-intrinsic-call", on_key: Some("CRATONVM_JIT_IR_LOOP_INTRINSIC_CALL"), off_key: None, off_word: None, since: "2026-10-28" },
    E { group: Group::JIT, token: "ir-splice-putstatic-loop-chain", on_key: Some("CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_CHAIN"), off_key: None, off_word: None, since: "2026-10-28" },
    // JIT round 16 wave 8, lane collections: all three default ON (kill switch "0"); see
    // native-collections `iterator_subinterface_shim_kind`, `snapshot_itr_box_element` and
    // `spliterator_interface_shim_kind`.
    // JIT round 16 wave 9, lane spliter: both default ON (kill switch "0"); see native-collections
    // `spliterator_facts_enabled` (--compatible snapshot spliterator facts) and
    // `spliterator_jdk_fallback_enabled` (--real-jdk real ArraySpliterator fallback).
    // JIT round 16 wave 9, lane satb: default ON (kill switch "0"); the Generational
    // SATB pre-barrier of native reference stores, vm `vm_exec::NativeStoreSatb`.
    // JIT round 16 wave 12, lane nativearr: default ON (perf, W2A); the native array doors
    // bump the thread's TLAB (vm `env_cache::native_array_tlab_mode`). `-refill` adds the no-GC refill.
    // gcj j1/yg: default ON (`=0` kill) -- a `-Xlog:gc+tlab=debug`-only spec arms the collection's
    // events on Generational, so HotSpot's `TLAB totals` line prints (vm `gc_events::xlog_tlab_totals_alone_wanted`).
    // gcj j1/yg: default OFF -- a term-4 cycle the promotion-OOM guard alone kept from the pinned
    // copy is offered to it (gc `gen_heap::gen_pinned_copy_takes_promotion_risk_enabled`).
    // gcj j2/yg2 -- begin
    // gcj j2/yg2: default ON (`=0` kill) -- the Generational non-moving sweep keeps the known-zero claim for
    // the previous set's bytes still on the free list, and the TLAB tail sink returns tails without
    // forgetting the set (gc `gen_heap::gen_young_known_zero_carry_enabled`; `yp_kz_carried_bytes=`).
    // JIT round 23 wave 6 (lane L4): perf, default ON, kill "0". The non-moving young sweep leaves its reclaimed spans to
    // the arena's lazy-dirty set and exact mirror (zeroed at hand-out, declined by is_object_address) instead of
    // zeroing them in the pause. 0 = the in-pause memset (gc `gen_heap::sweep_young_non_moving`).
    // t1 wave 8 (lane YGZERO, book gcj-j2yg2): measuring arm, default OFF. Young memory is zeroed on RECLAIM, never twice:
    // forces gen-sweep-lazy-zero and gen-in-place-lazy-zero off, and the pinned in-place rebuild claims the dead spans it
    // just zeroed as known-zero (gc `gen_heap::zero_on_reclaim_claim`), so a free-list hand-out inside one skips its memset.
    // JIT round 23 wave 8 (lane W8-L3): perf, default ON, kill "0". Proposal 14: in the fused walk, an anchored chunk
    // with no marked base, no conservative candidate / pin / unresolved mark and no marked object reaching into it is
    // declared dead whole without reading a header, so the non-moving young sweep is O(live chunks). Declined while a
    // reserved in-JIT TLAB tail is published (gc `gen_heap::dead_chunk_sweep`; `yp_dead_chunks=`).
    // JIT round 23 wave 8b (lane W8-L3): perf, default ON, kill "0". With gen-sweep-dead-chunk-skip engaged, the fused
    // walk runs on the allocator anchor grid subsampled at 256 KiB instead of the 8 MiB sweep anchors, so chunks the
    // live data touches cost about a TLAB each (gc `gen_heap::fine_dead_chunk_anchors`).
    // JIT round 23 wave 8b (lane W8-L3): perf, default ON, kill "0". The young arena's lazy-dirty mirror (one writer at a
    // time: the arena lock or stop the world) is written with plain Release stores instead of one locked fetch_or /
    // fetch_and per word, in the sweep's publication and every TLAB refill's clear (gc `arena::LazyDirtySet::plain_stores`).
    // JIT round 23 wave 9 (lane L3): perf, default ON, kill "0". The non-moving young sweep's side-mark bitmap keeps a
    // per-page summary of the blocks a mark touched; collect_marked (the side_sorted build) reads only those pages
    // instead of every word of a bitmap sized by young `used` (gc `young_mark::YoungMarkBits::collect_marked`).
    // JIT round 23 wave 4 (lane L4): perf, default ON since JIT round 24 (item A2), kill "0". A moving Generational
    // young cycle builds its exact object-start bitmap lazily, chunk by chunk, each walked from an allocator-certified
    // anchor; the prefix every conservative / pin / root / dirty-card word needs is proven before anything is
    // forwarded and a failure still skips the cycle. A chunk unprovable after that is a fatal stop (wave 4c), never a
    // refused reference (gc `young_mark::LazyStartGrid`).
    // JIT round 23 wave 4c (lane L4): safety arm, default OFF. With gen-lazy-young-object-starts, prove every lazy chunk
    // by induction from offset 0 (the eager walk's proof) instead of trusting the arena-certified allocator anchors.
    // gcj pcp: perf, default ON (kill "0"). The parallel moving young copy forwards a precise edge (a scanned object's
    // reference slot, a dirty-card slot) without the object-start proof, and the card values' lazy chunks are not
    // proven before the copy; only conservative words ask the start bitmap (gc `ParEvac::evacuate_precise`).
    // Pinned-copy pause: ergonomic Generational TLABs are at most 64 KiB so
    // conservative words' lazy proofs do not walk MiB-sized allocations.
    // Per VM; explicit/frozen HotSpot sizing and the numeric cap take priority.
    // JIT round 23 wave 4b (lane L4): perf, default OFF (implied by gen-lazy-young-object-starts). With gen-in-place-lazy-zero,
    // a pinned in-place young cycle whose arena has the exact lazy-dirty mirror (review R-L4z) skips the dead-header
    // poisoning: the mirror already makes every dead byte "not an object" to is_object_address and the non-moving mark.
    // R23-W6-FLIP: default ON since wave 6 (kill "0"), with gen-sweep-lazy-zero.
    // gcj j2/yg2: default OFF -- Generational only: HotSpot's refill-waste limit, re-armed to
    // desired / TLABRefillWasteFraction at every refill and raised by -XX:TLABWasteIncrement words per
    // kept miss (vm `vm_init::gci_i45_sc_tlab_sizing`, gc `tlab::Tlab::keep_on_miss_sized`).
    // gcj j2/yg2, default ON since gcj j1/yg (2026-10-25), kill "0": a heap the Serial-shaped young trigger armed
    // runs no young pause goal unless CRATONVM_GC_YOUNG_PAUSE_MS is set (gc `gen_heap::gen_serial_eden_no_pause_goal_enabled`).
    // gcj j2/yg2 -- end
    // gcj j3/yg3 -- begin
    // gcj j3/yg3: default ON (`=0` kill) -- a Generational root scan that publishes its precise-table counts
    // (gen-precise-root-promote) roots a young user-loader mirror from the class-mirror table instead of
    // deferring it under gen-young-mirror-defer, so step 14w' no longer forces the in-place young sweep
    // (vm `memory::roots::young_mirror_widens`; `young_mirrors_deferred=` on `[GC] conc_y2o:`).
    E { group: Group::GC, token: "gen-young-mirror-root-when-promotable", on_key: Some("CRATONVM_GEN_YOUNG_MIRROR_ROOT_WHEN_PROMOTABLE"), off_key: None, off_word: None, since: "2026-10-19" },
    // gcj j3/yg3: default ON (`=0` kill) -- the Generational heap's smallest-whole refill floor and TLAB tail
    // sink use the VM's typed -XX:MinTLABSize, not the 8 KiB constant (gc
    // `gen_heap::gen_typed_min_tlab_floor_enabled`, `GenerationalHeap::set_min_tlab_floor`).
    // gcj j3/yg3: default OFF -- Generational only: the TLAB share sizer's underpredicted raise counts the
    // window's granted bytes, not its refills (vm `vm_init::gci_i45_sc_tlab_sizing`, gc
    // `tlab::Tlab::note_share_refill_sized`).
    // gcj j3/yg3: default ON (`=0` kill) -- the serial Cheney young copy tenures a survivor young to-space
    // cannot take instead of aborting the process, as the parallel arm does (gc
    // `gen_heap::cheney_overflow_destination`; `cheney_overflow_promotions=` on `[GC] par_evac:`).
    // gcj yg3-residuals R3: default ON (`=0` kill) -- HotSpot Serial's young geometry: each semi reserves eden +
    // survivor, old gen is -Xmx - Y, the trigger is eden on top of the last cycle's young carry, and a copy keeps
    // at most one survivor young (`gen_heap::gen_serial_young_geometry_enabled`, `[GC] young_geometry:`).
    // gcj j3/yg3 (2026-10-20; default OFF until 2026-10-26): with gen-young-mirror-defer, a scan that deferred young user
    // mirrors asks the moving young phase to follow their loaders' mirror_pin rows (the licensed moving
    // System.gc() machinery) instead of forcing the in-place sweep (vm `memory::roots::young_mirror_promise`,
    // gc `gen_heap::set_young_side_table_rows_follow`).
    // gcj yg3-residuals R2: default ON since 2026-10-26, `=0` kills (restores the forced in-place sweep).
    E { group: Group::GC, token: "gen-young-mirror-defer-moving-follows", on_key: Some("CRATONVM_GEN_YOUNG_MIRROR_DEFER_MOVING_FOLLOWS"), off_key: None, off_word: Some("0"), since: "2026-10-20" },
    // JIT round 23 wave 6 (lane L4): correctness, default ON, kill "0". A pinned in-place young cycle that follows the
    // deferred young mirrors' rows (gen-young-mirror-defer-moving-follows) seeds its plan by the named rule, so a dead
    // loader beside a conservative word is not rooted (GenR5W5ConcUnloadProbe unloads as HotSpot does).
    // JIT round 16 wave 14, lane compat2: all three default ON (kill switch "0"); see
    // native-collections `stream_parallel_stage_enabled`, `stream_source_facts_enabled` and
    // `collections_sort_virtual_enabled`.
    // nio21 wave 11 L4: linked invokedynamic call sites and ldc MethodType records are never evicted from the resolution cache (JVMS 5.4.3.6: linked once); they go with their class. Default ON, kill "0".
    // gci i3/jd (opt-in): a class that reports no loader resolves its MethodType descriptor types in its own namespace (method_type_walk), not through the bootstrap forName0.
    E { group: Group::LOADER, token: "method-type-no-loader-resolves-in-vm", on_key: Some("CRATONVM_METHOD_TYPE_NO_LOADER_RESOLVES_IN_VM"), off_key: None, off_word: None, since: "2026-10-12" },
    // JIT round 17 wave 1, lane streams: all six default ON (kill switch "0"); see
    // native-collections `compat_real_pipeline_spliterator_enabled`, `prim_stage_spl_facts`,
    // `primitive_iterator_jdk_adapter`, `lazy_head_spliterator_enabled` and native-builtins
    // service_loader.rs `stream_support_parallel_requested`, `stream_support_source_facts_enabled`.
    // JIT round 17 wave 2, lane streams2: default ON (kill switch "0") except the opt-in
    // compat-string-stream-bytecode; see native-builtins lang_math.rs
    // `string_stream_natives_yield_to_bytecode`, lang_string.rs `string_lines_units_stream_enabled`,
    // service_loader.rs `stream_support_null_npe_enabled` and native-collections
    // `stream_zero_facts_enabled`, `stream_lazy_stage_estimate_enabled`,
    // `stream_lazy_unordered_stage_enabled`.
    // Round 17 wave 3, lane streams3 (STREAMS2-1): flipped to default ON, kill switch "0".
    // zki (page zgc-zt25hunt7b-zr2cp-grow-cpu-is-mutator-time-before-the-collections item A): on a real JDK image, both modes, the wrapper Intrinsic rows the JIT has no door for (Double/Float/Short/Byte/Character boxing and unboxing, the cross-type conversions, isNaN / isInfinite / compare / hashCode / equals) are left unregistered so the JDK bodies run and are compiled: Double.valueOf(D) went from 1.3-2.2 us to tens of ns a call. Default ON, "0" = the natives again.
    // JIT round 17 wave 3, lane streams3: all six default ON (kill switch "0"); see
    // native-collections `list_stream_iface_kind`, `cow_set_stream_jdk_body_enabled`,
    // `strict_elements_stream_jdk_pipeline_enabled`, `primitive_stream_iterator_kind`,
    // `source_facts_failure_to_raise` and native-builtins panama.rs
    // `compat_segment_elements_stream_support_enabled`.
    // JIT round 17 wave 8, lane streams4: default ON (kill switch "0"); see
    // native-collections `stream_shared_parallel_enabled`.
    // JIT round 18 wave 10, lane streams5: default ON (kill switch "0"); see
    // native-collections `stream_stage_display_class_enabled`,
    // `stream_shared_parallel_primitive_enabled`, `stream_slice_arg_check_enabled`.
    // Same lane: default ON (kill switch "0"); native-collections `stream_concat_hotspot_enabled`.
    // JIT round 18 wave 11, lane streams5: default ON (kill switch "0"); readers are the
    // native-collections `stream_*_enabled` functions of the same names.
    // JIT round 19 wave 11, lane hoist11: HotSpot parity, default ON (kill switch "0"); an indy
    // concat `"" + b` (lone `boolean`, no literal text) answers the interned "true"/"false" literal as
    // HotSpot's `unaryConcat` (`String.valueOf(boolean)`) does (vm `invokedynamic::unary_boolean_concat_operand`).
    // gci i2/wr (gch-h1ak-first-thread-start-initialises-java-util-logging): the first Thread.start() pre-warms java.util.logging; opt-in since vi4-ju C (`runtime_flag_on`; HotSpot does not pre-warm).
    // (gci i2/jm's `jimage-direct-map` row is dev's IO row since the 2026-10-15 merge.)
    // JIT round 20, lane walkcompat (r20-compatible-refarray-swriter-date-gap-is-not-a-stand-in): under
    // `--compatible` the `jdk-print-writer` row also drops the `System.lineSeparator()` stand-in, so
    // `PrintWriter.newLine` / `BufferedWriter.newLine` read the static the JDK body reads (perf; default ON, kill "0").
    // JIT round 20 wave 5, lane walkcompat (r20-compatible-refarray-swriter-date-gap-is-not-a-stand-in): under
    // `--compatible` the `jdk-date` row also drops the 11 `java.sql` Date/Time/Timestamp stand-ins
    // (`RETIRED_SHADOW_SQL_DATETIME_TRIPLES`; R16 `date` 9.7x `--real-jdk`). Perf; default ON, kill "0".
    // jr20 wave 9 / gcE (gch-h1ak first-thread logging page): opt-in since the w9c gate (`=1` restores the launcher's one-shot `java.util.logging` pre-warm on the first application `Thread.start()`, which runs `LogManager.<clinit>` where HotSpot does not). Without it: GC21ELogManagerInitPoint SAME 6/6 and the concurrent first-use race GC21ELogFirstUseRace SAME 120/120 (no hang).
    // Interpreter round i7 wave 138, lane L2 (`i137-orch-jdk-only-soft-and-phantom-refersto-cost-4-us`):
    // under `--real-jdk` over a real JDK the registry drops the `CompatFamily::bridges` triples
    // (`Reference` / `SoftReference` / `PhantomReference.refersTo`, ...) that `--compatible` drops with
    // the family row; they never answer under `--real-jdk` but kept the JDK bodies interpreted.
    // Performance; default ON since i7 wave 144.
    // default ON since interpreter round i7 (A/B w142lto)
    E { group: Group::TEST, token: "force-win-build", on_key: Some("CRATONVM_FORCE_WIN_BUILD"), off_key: None, off_word: None, since: "2026-05-20" },
    E { group: Group::TEST, token: "segv", on_key: Some("CRATONVM_TEST_SEGV"), off_key: None, off_word: None, since: "2026-06-01" },
    E { group: Group::DBG, token: "access", on_key: Some("CRATONVM_DBG_ACCESS"), off_key: None, off_word: None, since: "2026-11-20" },
    E { group: Group::DBG, token: "ir-linear-scan", on_key: Some("CRATONVM_DBG_IR_LINEAR_SCAN"), off_key: None, off_word: None, since: "2026-11-20" },
    E { group: Group::DBG, token: "modload", on_key: Some("CRATONVM_DBG_MODLOAD"), off_key: None, off_word: None, since: "2026-11-20" },
    E { group: Group::DBG, token: "gpu-trace-bytes", on_key: Some("CRATONVM_GPU_TRACE_BYTES"), off_key: None, off_word: None, since: "2026-11-20" },
    E { group: Group::LOADER, token: "eager-bootstrap-all", on_key: Some("CRATONVM_EAGER_BOOTSTRAP_ALL"), off_key: None, off_word: None, since: "2026-11-20" },
    E { group: Group::JIT, token: "gpu-host-callback", on_key: Some("CRATONVM_GPU_HOST_CALLBACK"), off_key: None, off_word: None, since: "2026-11-20" },
    E { group: Group::JIT, token: "gpu-native-stream-pool", on_key: Some("CRATONVM_GPU_NATIVE_STREAM_POOL"), off_key: None, off_word: None, since: "2026-11-20" },
    E { group: Group::JIT, token: "ir-lazy-ref-homes", on_key: Some("CRATONVM_JIT_IR_LAZY_REF_HOMES"), off_key: None, off_word: Some("0"), since: "2026-11-20" },
    E { group: Group::JIT, token: "ir-shadow-frame-block-lazy-clear", on_key: Some("CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK_LAZY_CLEAR"), off_key: None, off_word: Some("0"), since: "2026-11-20" },
    E { group: Group::JIT, token: "ir-speculate", on_key: Some("CRATONVM_JIT_IR_SPECULATE"), off_key: None, off_word: Some("0"), since: "2026-11-20" },
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
/// `docs/feature-designs/jit-ir-optimizations.md`.
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
/// filter: it drops synthetic-stub natives and nothing else. Real-JDK mode is
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
              the class-loading or dispatch half of the Real-JDK contract",
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
// Real-JDK command-line surface
// ───────────────────────────────────────────────────────────────────────────

/// One command-line flag, as the launcher's parser and its `--help` both need
/// it.
///
/// These are *not* environment variables and deliberately add none: the
/// fifteen-variable surface this module exists to hold down is a promise, and
/// `--real-jdk` is a runtime policy chosen per invocation, not a knob a parent
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
        flag: "--real-jdk",
        takes_value: false,
        help: "Real JDK, reject compatibility stubs and fabricated classes (default).",
    },
    CliFlag {
        flag: "--compatible",
        takes_value: false,
        help: "Real JDK with fast-boot compatibility substitutions.",
    },
    CliFlag {
        flag: "--fast-boot",
        takes_value: false,
        help: "Fast-boot optimizations for real JDK; alias for --compatible.",
    },
    CliFlag {
        flag: "--synthetic-jdk",
        takes_value: false,
        help: "Standalone synthetic library; implies --compatible, conflicts with --real-jdk.",
    },
    CliFlag {
        flag: "--real-jdk-report",
        takes_value: true,
        help: "Write the JSON violation/counter report.",
    },
    CliFlag {
        flag: "--dump-class-origins",
        takes_value: true,
        help: "Write the class-origin census.",
    },
    CliFlag {
        flag: "--trace-real-jdk",
        takes_value: false,
        help: "Log every violation as it happens.",
    },
    CliFlag {
        flag: "--explain-real-jdk",
        takes_value: false,
        help: "Print the long-form explanation for each violation.",
    },
];

/// Flags that `--real-jdk` cannot be combined with.
///
/// `--synthetic-jdk` selects a class library that is nothing but
/// substitutions, which is the one combination that cannot mean anything
/// (contract §1, §6).
///
/// `--compatible` and `--fast-boot` name the opposite *policy*, and a pair that
/// states both policies has no reading — so the launcher refuses instead of letting
/// argument order decide.
pub const JDK_ONLY_CONFLICTS_WITH: &[&str] = &["--synthetic-jdk", "--compatible", "--fast-boot"];

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
            // JIT round 18 wave 11 (lane osrfollow,
            // `r18-unroll-minus-token-leaves-the-ir-unroller-on-FIXED-20261006.md`):
            // a row whose two keys gate two different readers, the positive
            // one read default-ON, carries `off_word` -- unsetting that key
            // would leave its reader on, so write the word it reads as false.
            match e.off_word {
                Some(w) => overrides.insert(on_key, Some(OsString::from(w))),
                None => overrides.insert(on_key, None),
            };
        }
    } else if let Some(k) = e.on_key {
        // Unsetting a following child restores its parent instead of disabling it.
        let off_word = e.off_word.or_else(|| {
            FOLLOWS.iter().any(|&(child, _)| child == k).then_some("0")
        });
        match off_word {
            // Default-ON knob: unsetting it would leave it on, so write the
            // value its own parser reads as false.
            Some(w) => overrides.insert(k, Some(OsString::from(w))),
            None => overrides.insert(k, None),
        };
    }
}

/// Keys whose value is data (a path, a directory), not an on/off word (JIT
/// round 23 wave 3, lane L6; page
/// `r23-w2-review-exit-report-file-token-FIXED-20261021.md`). Two spellings are
/// first-class for each: the key itself (`CRATONVM_EXIT_REPORT_FILE=rep.txt`,
/// never reported as a legacy name) and the grouped `token=<value>`
/// (`CRATONVM_DBG=exit-report-file=rep.txt`). The bare token, which would mean
/// the value `1`, is refused ([`resolve`] lists it with the unknown tokens).
pub const VALUED_KEYS: &[&str] = &[
    "CRATONVM_EXIT_REPORT_FILE",
];

/// Does `e` set a [`VALUED_KEYS`] key?
fn entry_is_valued(e: &E) -> bool {
    e.on_key.is_some_and(|k| VALUED_KEYS.contains(&k))
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
            let (name, value, has_value) = match t.split_once('=') {
                Some((n, v)) => (n.trim(), v, true),
                None => (t, "1", false),
            };
            let name_lc = name.to_ascii_lowercase();

            if name_lc == "all" {
                // A valued key has no `1` to take ([`VALUED_KEYS`]).
                for e in entries(group).filter(|e| !(on && entry_is_valued(e))) {
                    apply(&mut overrides, e, on, "1");
                }
                continue;
            }
            match lookup(group, &name_lc) {
                // A valued key ([`VALUED_KEYS`]) named without `=<value>`:
                // enabling it would write the value `1` (a report file called
                // `1`), so it fails closed like a typo, naming both spellings.
                Some(e) if on && !has_value && entry_is_valued(e) => {
                    unknown_tokens.push(format!(
                        "{}={} (takes a value: {}={}=<value>, or {}=<value>)",
                        group.var(),
                        raw_token.trim(),
                        group.var(),
                        name,
                        e.on_key.unwrap_or("?"),
                    ));
                }
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
        // A valued key set directly is a supported spelling, not a legacy one:
        // its value (a path, a directory) is the point, and the advice line on
        // stderr is what `CRATONVM_EXIT_REPORT_FILE` exists to avoid.
        .filter(|k| !VALUED_KEYS.contains(k))
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
// (`docs/feature-designs/generative-gc-architecture.md`).
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
    // vm-cli build script `env!` values (compile time, not the run-time environment).
    "BUILD_COMPILER",
    "BUILD_TIME",
    // zt t8/cl4: a concat! prefix in a unit test, not a read.
    "CLASS_MIRROR_",
    "COMPATIBILITY_JDK_ONLY",
    "DBG_FLAGREADS",
    "DIFF_HOTSPOT",
    "FUZZ_BOOTCP",
    "FDLIMIT_TEST_CHILD",
    "ICC_PROFILE_DIR",
    "IMAGE_ARENA_TEST_CHILD_FILE",
    "JDK25_HOME",
    "JIT_BASELINE_FAST",
    "JIT_BASELINE_NO_SPEC",
    "LOOKUP_DEFINE_JDK_BODY",
    "NONEXISTENT_VAR_12345",
    "NO_MISPLACED_FLAG_WARNING",
    "P5CP_DUMP",
    "P5CP_REPS",
    "REAL_RAF",
    "REGBENCH_OUT",
    "REGBENCH_ROUNDS",
    "REGEN_HEADER",
    "REQUIRE_E2E",
    "RUN_EXTENDED_INTERPRETER_TESTS",
    "SOMETHING_BRAND_NEW",
    "SPRING_BOOT_FATJAR",
    "TEST_CLASSES_DIR",
    "TEST_JAVA_HOME",
    "TEST_PIPE_FLOOD",
    "ZGC_MARKEND_LOG",
];

/// Variables this repository's own scripts export around a VM run and the VM
/// never reads, without their `CRATONVM_` prefix: `regression-suite/run.sh`'s
/// and CI's `ARGS`, `scripts/cratonvm-prefix-args.sh`'s `PREFIX_BIN` /
/// `PREFIX_ARGS`, the Tomcat harness's `EXE`, `compact-tlab-soak.sh`'s
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
mod unclaimed_env_tests;

#[cfg(test)]
mod tests;
