// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The canonical `CRATONVM_*` environment surface.
//!
//! # The problem this solves
//!
//! `docs/internal/flag-census.md` counted **692** distinct `CRATONVM_*`
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
    /// census: no token here can change a program's result.
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
}

/// Every knob in the VM, exactly once.
///
/// Generated from the census scan; `types/tests/flag_surface.rs` asserts it
/// stays complete and `tools/flag-census/check-surface.sh` asserts the code and
/// the reference docs agree with it.
///
/// One row per line, deliberately. rustfmt would break each entry across five
/// lines, turning a 541-line table that `grep` can answer questions about into
/// a 2700-line one that it cannot.
#[rustfmt::skip]
pub const INVENTORY: &[E] = &[
    E { group: Group::DBG, token: "a2", on_key: Some("CRATONVM_DBG_A2"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "access", on_key: Some("CRATONVM_DBG_ACCESS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "active-profiles-identity-trace", on_key: Some("CRATONVM_ACTIVE_PROFILES_IDENTITY_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "aio", on_key: Some("CRATONVM_DBG_AIO"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "aioobe", on_key: Some("CRATONVM_DBG_AIOOBE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "aioobe2", on_key: Some("CRATONVM_DBG_AIOOBE2"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "aioobe3", on_key: Some("CRATONVM_DBG_AIOOBE3"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "altrace", on_key: Some("CRATONVM_DBG_ALTRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "ann-proxy-dispatch-trace", on_key: Some("CRATONVM_ANN_PROXY_DISPATCH_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "ann-trace", on_key: Some("CRATONVM_ANN_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "annproxy-wrap", on_key: Some("CRATONVM_DBG_ANNPROXY_WRAP"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "anonalloc", on_key: Some("CRATONVM_DBG_ANONALLOC"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "aqs-trace", on_key: Some("CRATONVM_DBG_AQS_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "args", on_key: Some("CRATONVM_DBG_ARGS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "arraycopy", on_key: Some("CRATONVM_DBG_ARRAYCOPY"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "arrlen", on_key: Some("CRATONVM_DBG_ARRLEN"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "arrstore", on_key: Some("CRATONVM_DBG_ARRSTORE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "asserteq", on_key: Some("CRATONVM_DBG_ASSERTEQ"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "assertj-arr", on_key: Some("CRATONVM_DBG_ASSERTJ_ARR"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "athrow", on_key: Some("CRATONVM_DBG_ATHROW"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "atomic-updater", on_key: Some("CRATONVM_DBG_ATOMIC_UPDATER"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "badrecv", on_key: Some("CRATONVM_DBG_BADRECV"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "badref", on_key: Some("CRATONVM_DBG_BADREF"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "bb", on_key: Some("CRATONVM_DBG_BB"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "bblp", on_key: Some("CRATONVM_DBG_BBLP"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "bd-debug", on_key: Some("CRATONVM_BD_DEBUG"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "blocked-access", on_key: Some("CRATONVM_DBG_BLOCKED_ACCESS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "blockgc", on_key: Some("CRATONVM_DBG_BLOCKGC"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "bufunder", on_key: Some("CRATONVM_DBG_BUFUNDER"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "bug03", on_key: Some("CRATONVM_DBG_BUG03"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "bytecode-dump", on_key: Some("CRATONVM_DBG_BYTECODE_DUMP"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "caller", on_key: Some("CRATONVM_DBG_CALLER"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "capval", on_key: Some("CRATONVM_DBG_CAPVAL"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "catalina", on_key: Some("CRATONVM_DBG_CATALINA"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "cause", on_key: Some("CRATONVM_DBG_CAUSE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "cce", on_key: Some("CRATONVM_DBG_CCE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "cce-bt", on_key: Some("CRATONVM_DBG_CCE_BT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "ccecache", on_key: Some("CRATONVM_DBG_CCECACHE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "ccsprobe", on_key: Some("CRATONVM_DBG_CCSPROBE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "cellcorrupt", on_key: Some("CRATONVM_DBG_CELLCORRUPT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "charset", on_key: Some("CRATONVM_DBG_CHARSET"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "classpath", on_key: Some("CRATONVM_DBG_CLASSPATH"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "cleaners", on_key: None, off_key: Some("CRATONVM_DBG_NO_CLEANERS"), off_word: None },
    E { group: Group::DBG, token: "clone", on_key: Some("CRATONVM_DBG_CLONE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "coerce", on_key: Some("CRATONVM_DBG_COERCE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "compact-inline", on_key: Some("CRATONVM_DBG_COMPACT_INLINE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "compact-legacy", on_key: Some("CRATONVM_DBG_COMPACT_LEGACY"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "compactvalue", on_key: Some("CRATONVM_DBG_COMPACTVALUE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "component-type", on_key: Some("CRATONVM_DBG_COMPONENT_TYPE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "corrupt-frames", on_key: Some("CRATONVM_DBG_CORRUPT_FRAMES"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "ctor-fix", on_key: Some("CRATONVM_DBG_CTOR_FIX"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "debug-sfi", on_key: Some("CRATONVM_DEBUG_SFI"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "debug-stack-tag", on_key: Some("CRATONVM_DEBUG_STACK_TAG"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "debug-stackwalk", on_key: Some("CRATONVM_DEBUG_STACKWALK"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "define", on_key: Some("CRATONVM_DBG_DEFINE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "deflate", on_key: Some("CRATONVM_DBG_DEFLATE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "deopt", on_key: Some("CRATONVM_DBG_DEOPT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "deopt-eager", on_key: Some("CRATONVM_DEOPT_EAGER"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "deopt-eager-bci", on_key: Some("CRATONVM_DEOPT_EAGER_BCI"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "deopt-verify", on_key: Some("CRATONVM_DEOPT_VERIFY"), off_key: None, off_word: None },
    // Default-ON: the launcher prints one line when it sees a legacy per-flag
    // variable set directly. `CRATONVM_DBG=-deprecations` silences it.
    E { group: Group::DBG, token: "deprecations", on_key: None, off_key: Some("CRATONVM_QUIET_DEPRECATIONS"), off_word: None },
    E { group: Group::DBG, token: "desctrace", on_key: Some("CRATONVM_DBG_DESCTRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "diag-hib32", on_key: Some("CRATONVM_DIAG_HIB32"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "diag-jar-list", on_key: Some("CRATONVM_DIAG_JAR_LIST"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "diag-jboss-services", on_key: Some("CRATONVM_DIAG_JBOSS_SERVICES"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "diag-jca", on_key: Some("CRATONVM_DIAG_JCA"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "diag-method-invoke-null", on_key: Some("CRATONVM_DIAG_METHOD_INVOKE_NULL"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "diag-properties", on_key: Some("CRATONVM_DIAG_PROPERTIES"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "diag-serviceloader", on_key: Some("CRATONVM_DIAG_SERVICELOADER"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "dopriv", on_key: Some("CRATONVM_DBG_DOPRIV"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "dropped-stubs", on_key: Some("CRATONVM_DBG_DROPPED_STUBS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "dump-jit", on_key: Some("CRATONVM_DBG_DUMP_JIT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "dupcall-filter", on_key: Some("CRATONVM_DBG_DUPCALL_FILTER"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "dupclass", on_key: Some("CRATONVM_DBG_DUPCLASS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "dupclass-bt", on_key: Some("CRATONVM_DBG_DUPCLASS_BT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "dupclass-filter", on_key: Some("CRATONVM_DBG_DUPCLASS_FILTER"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "dupx-methods", on_key: Some("CRATONVM_DBG_DUPX_METHODS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "ecwatch", on_key: Some("CRATONVM_DBG_ECWATCH"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "ecwatch-native", on_key: Some("CRATONVM_DBG_ECWATCH_NATIVE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "enable-native-ring", on_key: Some("CRATONVM_ENABLE_NATIVE_RING"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "eqe", on_key: Some("CRATONVM_DBG_EQE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "exec", on_key: Some("CRATONVM_DBG_EXEC"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "exec-frame-trace", on_key: Some("CRATONVM_EXEC_FRAME_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "exit", on_key: Some("CRATONVM_DBG_EXIT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "fbcglib", on_key: Some("CRATONVM_DBG_FBCGLIB"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "fbref", on_key: Some("CRATONVM_DBG_FBREF"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "field-get", on_key: Some("CRATONVM_DBG_FIELD_GET"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "field-watch", on_key: Some("CRATONVM_DBG_FIELD_WATCH"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "fieldaddr", on_key: Some("CRATONVM_DBG_FIELDADDR"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "force-moving", on_key: Some("CRATONVM_DBG_FORCE_MOVING"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "forname-trace", on_key: Some("CRATONVM_FORNAME_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "frame-trace", on_key: Some("CRATONVM_FRAME_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "fsp", on_key: Some("CRATONVM_DBG_FSP"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "fullstack-scan", on_key: Some("CRATONVM_DBG_FULLSTACK_SCAN"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "fwdguard", on_key: Some("CRATONVM_DBG_FWDGUARD"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "g1-dbg-headers", on_key: Some("CRATONVM_G1_DBG_HEADERS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "g1-dbg-pins", on_key: Some("CRATONVM_G1_DBG_PINS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "g1-dbg-reach", on_key: Some("CRATONVM_G1_DBG_REACH"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "g1-dbg-rootcensus", on_key: Some("CRATONVM_G1_DBG_ROOTCENSUS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "g1-dbg-zero", on_key: Some("CRATONVM_G1_DBG_ZERO"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "g1diag", on_key: Some("CRATONVM_DBG_G1DIAG"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "gc-array-guard-bt", on_key: Some("CRATONVM_GC_ARRAY_GUARD_BT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "gc-overhead", on_key: Some("CRATONVM_DBG_GC_OVERHEAD"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "gc-stats", on_key: Some("CRATONVM_GC_STATS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "gc-stress", on_key: Some("CRATONVM_DBG_GC_STRESS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "gc-verify-stale", on_key: Some("CRATONVM_GC_VERIFY_STALE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "gcpart", on_key: Some("CRATONVM_DBG_GCPART"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "gcpause", on_key: Some("CRATONVM_DBG_GCPAUSE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "gcphase", on_key: Some("CRATONVM_DBG_GCPHASE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "gcwrite", on_key: Some("CRATONVM_DBG_GCWRITE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "getresources", on_key: Some("CRATONVM_DBG_GETRESOURCES"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "gocbf", on_key: Some("CRATONVM_DBG_GOCBF"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "gpu-trace-bytes", on_key: Some("CRATONVM_GPU_TRACE_BYTES"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "gse", on_key: Some("CRATONVM_DBG_GSE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "h2parserread", on_key: Some("CRATONVM_DBG_H2PARSERREAD"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "h2trace", on_key: Some("CRATONVM_DBG_H2TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "hang-sample", on_key: Some("CRATONVM_DBG_HANG_SAMPLE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "hangwalk", on_key: Some("CRATONVM_DBG_HANGWALK"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "heap-stale", on_key: Some("CRATONVM_DBG_HEAP_STALE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "heap-trace", on_key: Some("CRATONVM_DBG_HEAP_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "heapcopy", on_key: Some("CRATONVM_DBG_HEAPCOPY"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "heartbeat", on_key: Some("CRATONVM_DBG_HEARTBEAT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "hm-trace", on_key: Some("CRATONVM_HM_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "hmput", on_key: Some("CRATONVM_DBG_HMPUT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "hotpath-counts", on_key: Some("CRATONVM_DBG_HOTPATH_COUNTS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "hs-itr-dbg", on_key: Some("CRATONVM_HS_ITR_DBG"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "httpsrv", on_key: Some("CRATONVM_DBG_HTTPSRV"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "iae-trace", on_key: Some("CRATONVM_IAE_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "iae-trace2", on_key: Some("CRATONVM_IAE_TRACE2"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "imse", on_key: Some("CRATONVM_DBG_IMSE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "indy-all", on_key: Some("CRATONVM_DBG_INDY_ALL"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "indy-generic", on_key: Some("CRATONVM_DBG_INDY_GENERIC"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "inline-fr", on_key: Some("CRATONVM_DBG_INLINE_FR"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "interrupt", on_key: Some("CRATONVM_DBG_INTERRUPT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "intrinsic-stats", on_key: Some("CRATONVM_INTRINSIC_STATS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "isolated-cnf", on_key: Some("CRATONVM_DBG_ISOLATED_CNF"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "invoke-coerce", on_key: Some("CRATONVM_DBG_INVOKE_COERCE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "invoke-virtual-entry-trace", on_key: Some("CRATONVM_INVOKE_VIRTUAL_ENTRY_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "invokestatic-loader-trace", on_key: Some("CRATONVM_INVOKESTATIC_LOADER_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "invokestats", on_key: Some("CRATONVM_DBG_INVOKESTATS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "invspecial", on_key: Some("CRATONVM_DBG_INVSPECIAL"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "ir-call", on_key: Some("CRATONVM_DBG_IR_CALL"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "ir-long", on_key: Some("CRATONVM_DBG_IR_LONG"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "irslot", on_key: Some("CRATONVM_DBG_IRSLOT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "isinstance", on_key: Some("CRATONVM_DBG_ISINSTANCE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jar", on_key: Some("CRATONVM_DBG_JAR"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jetty", on_key: Some("CRATONVM_DBG_JETTY"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jetty2", on_key: Some("CRATONVM_DBG_JETTY2"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jit-alloc", on_key: Some("CRATONVM_DBG_JIT_ALLOC"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jit-bisect-only", on_key: Some("CRATONVM_JIT_BISECT_ONLY"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jit-bisect-skip", on_key: Some("CRATONVM_JIT_BISECT_SKIP"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jit-code", on_key: Some("CRATONVM_DBG_JIT_CODE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jit-compiled", on_key: Some("CRATONVM_DBG_JIT_COMPILED"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jit-disasm", on_key: Some("CRATONVM_DBG_JIT_DISASM"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jit-dispatch", on_key: Some("CRATONVM_DBG_JIT_DISPATCH"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jit-entry", on_key: Some("CRATONVM_DBG_JIT_ENTRY"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jit-gen", on_key: Some("CRATONVM_DBG_JIT_GEN"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jit-ldc", on_key: Some("CRATONVM_DBG_JIT_LDC"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jit-method-stats", on_key: Some("CRATONVM_DBG_JIT_METHOD_STATS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jit-mic", on_key: Some("CRATONVM_DBG_JIT_MIC"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jit-names", on_key: Some("CRATONVM_DBG_JIT_NAMES"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jit-putfield", on_key: Some("CRATONVM_DBG_JIT_PUTFIELD"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jit-safepoints", on_key: Some("CRATONVM_DBG_JIT_SAFEPOINTS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jitc", on_key: Some("CRATONVM_DBG_JITC"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "jlm", on_key: Some("CRATONVM_DBG_JLM"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "kcbool", on_key: Some("CRATONVM_DBG_KCBOOL"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "lambda", on_key: Some("CRATONVM_DBG_LAMBDA"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "lambda-dispatch", on_key: Some("CRATONVM_DBG_LAMBDA_DISPATCH"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "lambda-generic", on_key: Some("CRATONVM_DBG_LAMBDA_GENERIC"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "layout", on_key: Some("CRATONVM_DBG_LAYOUT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "ldc-classref-trace", on_key: Some("CRATONVM_LDC_CLASSREF_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "letsgo", on_key: Some("CRATONVM_DBG_LETSGO"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "lhm-evict", on_key: Some("CRATONVM_DBG_LHM_EVICT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "licm", on_key: Some("CRATONVM_DBG_LICM"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "linker", on_key: Some("CRATONVM_DBG_LINKER"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "loadclass", on_key: Some("CRATONVM_DBG_LOADCLASS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "loader-trace", on_key: Some("CRATONVM_DBG_LOADER_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "logprov", on_key: Some("CRATONVM_DBG_LOGPROV"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "longroot", on_key: Some("CRATONVM_DBG_LONGROOT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "lookup", on_key: Some("CRATONVM_DBG_LOOKUP"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "mcl", on_key: Some("CRATONVM_DBG_MCL"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "memwatch", on_key: Some("CRATONVM_DBG_MEMWATCH"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "method-invoke-box", on_key: Some("CRATONVM_DBG_METHOD_INVOKE_BOX"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "mh-adapter", on_key: Some("CRATONVM_DBG_MH_ADAPTER"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "mh-dispatch", on_key: Some("CRATONVM_DBG_MH_DISPATCH"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "mh-stack", on_key: Some("CRATONVM_DBG_MH_STACK"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "mic-prof", on_key: Some("CRATONVM_DBG_MIC_PROF"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "minvoke", on_key: Some("CRATONVM_DBG_MINVOKE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "mirrorpin", on_key: Some("CRATONVM_DBG_MIRRORPIN"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "modprov", on_key: Some("CRATONVM_DBG_MODPROV"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "modstatic", on_key: Some("CRATONVM_DBG_MODSTATIC"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "monenter", on_key: Some("CRATONVM_DBG_MONENTER"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "monexit", on_key: Some("CRATONVM_DBG_MONEXIT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "moving-young-band-dbg", on_key: Some("CRATONVM_MOVING_YOUNG_BAND_DBG"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "moving-young-coverage-dbg", on_key: Some("CRATONVM_MOVING_YOUNG_COVERAGE_DBG"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "moving-young-fallbacks", on_key: Some("CRATONVM_MOVING_YOUNG_FALLBACKS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "moving-young-no-band-verify", on_key: Some("CRATONVM_MOVING_YOUNG_NO_BAND_VERIFY"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "moving-young-verify", on_key: Some("CRATONVM_MOVING_YOUNG_VERIFY"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "msc", on_key: Some("CRATONVM_DBG_MSC"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "mtroots", on_key: Some("CRATONVM_DBG_MTROOTS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "ncdfe", on_key: Some("CRATONVM_DBG_NCDFE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "needs-exact-trace", on_key: Some("CRATONVM_NEEDS_EXACT_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "net", on_key: Some("CRATONVM_DBG_NET"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "netty-queue", on_key: Some("CRATONVM_DBG_NETTY_QUEUE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "nextint", on_key: Some("CRATONVM_DBG_NEXTINT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "nio-bind", on_key: Some("CRATONVM_DBG_NIO_BIND"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "nocode", on_key: Some("CRATONVM_DBG_NOCODE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "nonmoving-reclaim", on_key: None, off_key: Some("CRATONVM_DBG_NO_NONMOVING_RECLAIM"), off_word: None },
    E { group: Group::DBG, token: "npe-invoke", on_key: Some("CRATONVM_DBG_NPE_INVOKE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "npe-none", on_key: Some("CRATONVM_DBG_NPE_NONE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "npe-stack", on_key: Some("CRATONVM_DBG_NPE_STACK"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "npe-trace", on_key: Some("CRATONVM_DBG_NPE_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "nsee-trace", on_key: Some("CRATONVM_NSEE_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "nsme", on_key: Some("CRATONVM_DBG_NSME"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "null-native", on_key: Some("CRATONVM_DBG_NULL_NATIVE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "nullthis", on_key: Some("CRATONVM_DBG_NULLTHIS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "obj-equals", on_key: Some("CRATONVM_DBG_OBJ_EQUALS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "objects", on_key: Some("CRATONVM_DBG_OBJECTS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "obsreg", on_key: Some("CRATONVM_DBG_OBSREG"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "oobfield", on_key: Some("CRATONVM_DBG_OOBFIELD"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "oop-span-probe", on_key: Some("CRATONVM_OOP_SPAN_PROBE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "osr", on_key: Some("CRATONVM_DBG_OSR"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "osr-exit-after", on_key: Some("CRATONVM_OSR_EXIT_AFTER"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "osr-exit-test", on_key: Some("CRATONVM_OSR_EXIT_TEST"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "osr-meta", on_key: Some("CRATONVM_DBG_OSR_META"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "overlay", on_key: Some("CRATONVM_DBG_OVERLAY"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "overlay-all", on_key: Some("CRATONVM_DBG_OVERLAY_ALL"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "parklat", on_key: Some("CRATONVM_DBG_PARKLAT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "pb", on_key: Some("CRATONVM_DBG_PB"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "pbe", on_key: Some("CRATONVM_DBG_PBE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "pbstart", on_key: Some("CRATONVM_DBG_PBSTART"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "picocli-style", on_key: Some("CRATONVM_DBG_PICOCLI_STYLE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "popint", on_key: Some("CRATONVM_DBG_POPINT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "precise", on_key: Some("CRATONVM_DBG_PRECISE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "proxy", on_key: Some("CRATONVM_DBG_PROXY"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "prune", on_key: None, off_key: Some("CRATONVM_DBG_NO_PRUNE"), off_word: None },
    // Was the bare `CRATONVM_DBG`, which is now the group variable itself. It
    // gated exactly one call site (`quarkus_staticinit.rs`), so it becomes an
    // ordinary topic. `CRATONVM_DBG=1` no longer enables it — that spelling now
    // reports `1` as an unknown token, which is the point.
    E { group: Group::DBG, token: "quarkus-staticinit", on_key: Some("CRATONVM_DBG_QUARKUS_STATICINIT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "quicken-stats", on_key: Some("CRATONVM_QUICKEN_STATS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "raf-getfd", on_key: Some("CRATONVM_DBG_RAF_GETFD"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "raf-init", on_key: Some("CRATONVM_DBG_RAF_INIT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "rbc6", on_key: Some("CRATONVM_DBG_RBC6"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "re5", on_key: Some("CRATONVM_DBG_RE5"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "refersto", on_key: Some("CRATONVM_DBG_REFERSTO"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "reflection-factory", on_key: Some("CRATONVM_DBG_REFLECTION_FACTORY"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "refproc", on_key: None, off_key: Some("CRATONVM_DBG_NO_REFPROC"), off_word: None },
    E { group: Group::DBG, token: "refproc-remark", on_key: Some("CRATONVM_DBG_REFPROC_REMARK"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "remap-trace", on_key: Some("CRATONVM_DBG_REMAP_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "replovr", on_key: Some("CRATONVM_DBG_REPLOVR"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "resolve-shim", on_key: Some("CRATONVM_DBG_RESOLVE_SHIM"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "resource-timing", on_key: Some("CRATONVM_DBG_RESOURCE_TIMING"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "resume-pc", on_key: Some("CRATONVM_DBG_RESUME_PC"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "retransform", on_key: Some("CRATONVM_DBG_RETRANSFORM"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "rootsnap", on_key: Some("CRATONVM_DBG_ROOTSNAP"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "rset-audit", on_key: Some("CRATONVM_DBG_RSET_AUDIT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "rset-audit-young-scan", on_key: Some("CRATONVM_DBG_RSET_AUDIT_YOUNG_SCAN"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "rvas", on_key: Some("CRATONVM_DBG_RVAS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "s111-dbg", on_key: Some("CRATONVM_S111_DBG"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "sbload", on_key: Some("CRATONVM_DBG_SBLOAD"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "sc-close", on_key: Some("CRATONVM_DBG_SC_CLOSE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "sc-read", on_key: Some("CRATONVM_DBG_SC_READ"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "sc-write", on_key: Some("CRATONVM_DBG_SC_WRITE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "scalar-deopt", on_key: Some("CRATONVM_DBG_SCALAR_DEOPT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "scalar-new", on_key: Some("CRATONVM_DBG_SCALAR_NEW"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "seed-all-old", on_key: Some("CRATONVM_DBG_SEED_ALL_OLD"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "seedhunt", on_key: Some("CRATONVM_DBG_SEEDHUNT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "sel", on_key: Some("CRATONVM_DBG_SEL"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "selector", on_key: Some("CRATONVM_DBG_SELECTOR"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "sfi-null-trace", on_key: Some("CRATONVM_SFI_NULL_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "shadow", on_key: Some("CRATONVM_DBG_SHADOW"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "shadow-depth", on_key: Some("CRATONVM_DBG_SHADOW_DEPTH"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "shadow-reload", on_key: Some("CRATONVM_DBG_SHADOW_RELOAD"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "shadow-sentinel", on_key: Some("CRATONVM_SHADOW_SENTINEL"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "shadow-watch", on_key: Some("CRATONVM_SHADOW_WATCH"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "shadow2", on_key: Some("CRATONVM_DBG_SHADOW2"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "shadow2-filter", on_key: Some("CRATONVM_DBG_SHADOW2_FILTER"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "sleep-trace", on_key: Some("CRATONVM_DBG_SLEEP_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "sock", on_key: Some("CRATONVM_DBG_SOCK"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "sock-bytes", on_key: Some("CRATONVM_DBG_SOCK_BYTES"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "soe", on_key: Some("CRATONVM_DBG_SOE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "soft-exit", on_key: Some("CRATONVM_SOFT_EXIT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "sp-stats", on_key: Some("CRATONVM_SP_STATS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "sp-trace", on_key: Some("CRATONVM_SP_TRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "sp-verify", on_key: Some("CRATONVM_SP_VERIFY"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "spid", on_key: Some("CRATONVM_DBG_SPID"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "spring-dbg", on_key: Some("CRATONVM_SPRING_DBG"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "stackless", on_key: Some("CRATONVM_DBG_STACKLESS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "stale-objref", on_key: Some("CRATONVM_DBG_STALE_OBJREF"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "stale-objref-cycles", on_key: Some("CRATONVM_DBG_STALE_OBJREF_CYCLES"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "stale-recv", on_key: Some("CRATONVM_DBG_STALE_RECV"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "stalelong", on_key: Some("CRATONVM_DBG_STALELONG"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "straystack", on_key: Some("CRATONVM_DBG_STRAYSTACK"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "streamsupp", on_key: Some("CRATONVM_DBG_STREAMSUPP"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "sttrace", on_key: Some("CRATONVM_DBG_STTRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "stw-census", on_key: Some("CRATONVM_DBG_STW_CENSUS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "stw-expected-ids", on_key: Some("CRATONVM_DBG_STW_EXPECTED_IDS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "stw-native-ring", on_key: Some("CRATONVM_DBG_STW_NATIVE_RING"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "surefire-ipc-dbg", on_key: Some("CRATONVM_SUREFIRE_IPC_DBG"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "sweep-census", on_key: Some("CRATONVM_DBG_SWEEP_CENSUS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "sweep-edges", on_key: Some("CRATONVM_DBG_SWEEP_EDGES"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "sweep-zero", on_key: Some("CRATONVM_DBG_SWEEP_ZERO"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "symbolize", on_key: Some("CRATONVM_SYMBOLIZE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "symbolize-dbg", on_key: Some("CRATONVM_SYMBOLIZE_DBG"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "threadreg-perf", on_key: Some("CRATONVM_DBG_THREADREG_PERF"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "threadstart", on_key: Some("CRATONVM_DBG_THREADSTART"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "tier-enqueue", on_key: Some("CRATONVM_DBG_TIER_ENQUEUE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "tlabmiss", on_key: Some("CRATONVM_DBG_TLABMISS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "tls-auth", on_key: Some("CRATONVM_DBG_TLS_AUTH"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "tls-hs", on_key: Some("CRATONVM_DBG_TLS_HS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "tls-pls", on_key: Some("CRATONVM_DBG_TLS_PLS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "tls-sock", on_key: Some("CRATONVM_DBG_TLS_SOCK"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "tls-srv", on_key: Some("CRATONVM_DBG_TLS_SRV"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "toarray", on_key: Some("CRATONVM_DBG_TOARRAY"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "tohex", on_key: Some("CRATONVM_DBG_TOHEX"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "trace-arrays-hashcode", on_key: Some("CRATONVM_TRACE_ARRAYS_HASHCODE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "trace-classvalue", on_key: Some("CRATONVM_TRACE_CLASSVALUE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "trace-pti-args", on_key: Some("CRATONVM_TRACE_PTI_ARGS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "trace-sb-filter", on_key: Some("CRATONVM_TRACE_SB_FILTER"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "trace-unimplemented", on_key: Some("CRATONVM_TRACE_UNIMPLEMENTED"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "track-native", on_key: Some("CRATONVM_TRACK_NATIVE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "uclreg", on_key: Some("CRATONVM_DBG_UCLREG"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "uclres", on_key: Some("CRATONVM_DBG_UCLRES"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "ucltrace", on_key: Some("CRATONVM_DBG_UCLTRACE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "ueh-debug", on_key: Some("CRATONVM_UEH_DEBUG"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "uncaught", on_key: Some("CRATONVM_DBG_UNCAUGHT"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "underflow", on_key: Some("CRATONVM_DBG_UNDERFLOW"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "unpark-miss", on_key: Some("CRATONVM_DBG_UNPARK_MISS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "unpin-ring", on_key: Some("CRATONVM_DBG_UNPIN_RING"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "unroll", on_key: Some("CRATONVM_DBG_UNROLL"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "urlcl", on_key: Some("CRATONVM_DBG_URLCL"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "ute", on_key: Some("CRATONVM_DBG_UTE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "validate-new", on_key: Some("CRATONVM_DBG_VALIDATE_NEW"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "vdisp", on_key: Some("CRATONVM_DBG_VDISP"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "verify-error", on_key: Some("CRATONVM_DBG_VERIFY_ERROR"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "verify-inline-frame-record", on_key: Some("CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "verify-oop-maps", on_key: Some("CRATONVM_DBG_VERIFY_OOP_MAPS"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "visitfile", on_key: Some("CRATONVM_DBG_VISITFILE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "vm-state", on_key: Some("CRATONVM_DBG_VM_STATE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "watch-cause-self", on_key: Some("CRATONVM_DBG_WATCH_CAUSE_SELF"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "watch-cell", on_key: Some("CRATONVM_DBG_WATCH_CELL"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "watchaddr", on_key: Some("CRATONVM_DBG_WATCHADDR"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "watchref", on_key: Some("CRATONVM_DBG_WATCHREF"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "weakref", on_key: Some("CRATONVM_DBG_WEAKREF"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "wf", on_key: Some("CRATONVM_DBG_WF"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "wf-npe", on_key: Some("CRATONVM_DBG_WF_NPE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "xnio-tcp", on_key: Some("CRATONVM_DBG_XNIO_TCP"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "xt-jit-root-scan", on_key: Some("CRATONVM_DBG_XT_JIT_ROOT_SCAN"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "youngscan", on_key: Some("CRATONVM_DBG_YOUNGSCAN"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "youngstate", on_key: Some("CRATONVM_DBG_YOUNGSTATE"), off_key: None, off_word: None },
    E { group: Group::DBG, token: "zero-ranges", on_key: Some("CRATONVM_DBG_ZERO_RANGES"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "aaload-licm", on_key: None, off_key: Some("CRATONVM_DISABLE_AALOAD_LICM"), off_word: None },
    E { group: Group::JIT, token: "alloc-class-cache", on_key: None, off_key: Some("CRATONVM_NO_JIT_ALLOC_CLASS_CACHE"), off_word: None },
    E { group: Group::JIT, token: "allow-packages", on_key: Some("CRATONVM_JIT_ALLOW_PACKAGES"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "arith-licm", on_key: None, off_key: Some("CRATONVM_DISABLE_ARITH_LICM"), off_word: None },
    E { group: Group::JIT, token: "bce", on_key: None, off_key: Some("CRATONVM_JIT_NO_BCE"), off_word: None },
    E { group: Group::JIT, token: "bg-compile", on_key: Some("CRATONVM_BG_COMPILE"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "c2-first-call", on_key: Some("CRATONVM_JIT_C2_FIRST_CALL"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "c2-supersede", on_key: Some("CRATONVM_C2_SUPERSEDE"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "callee-oop-flush", on_key: None, off_key: Some("CRATONVM_JIT_NO_CALLEE_OOP_FLUSH"), off_word: None },
    E { group: Group::JIT, token: "code-cache-max-mb", on_key: Some("CRATONVM_JIT_CODE_CACHE_MAX_MB"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "conservative-locals", on_key: None, off_key: Some("CRATONVM_NO_CONSERVATIVE_LOCALS"), off_word: None },
    E { group: Group::JIT, token: "ctor-direct-call", on_key: None, off_key: Some("CRATONVM_NO_CTOR_DIRECT_CALL"), off_word: None },
    E { group: Group::JIT, token: "deny", on_key: Some("CRATONVM_JIT_DENY"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "deopt-real", on_key: Some("CRATONVM_DEOPT_REAL"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "direct-callee-calls", on_key: Some("CRATONVM_JIT_DIRECT_CALLEE_CALLS"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "dispatch-cache-direct-entry", on_key: Some("CRATONVM_JIT_DISPATCH_CACHE_DIRECT_ENTRY"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "dispatch-cache-virtual-direct-entry", on_key: Some("CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "dup-x1", on_key: None, off_key: Some("CRATONVM_JIT_NO_DUP_X1"), off_word: None },
    E { group: Group::JIT, token: "dup-x2", on_key: None, off_key: Some("CRATONVM_JIT_NO_DUP_X2"), off_word: None },
    E { group: Group::JIT, token: "dupx", on_key: None, off_key: Some("CRATONVM_JIT_NO_DUPX"), off_word: None },
    E { group: Group::JIT, token: "dupx-eager-canon", on_key: Some("CRATONVM_JIT_DUPX_EAGER_CANON"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "enable-callee-saved-gpr-locals", on_key: Some("CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "enable-inline-new", on_key: Some("CRATONVM_JIT_ENABLE_INLINE_NEW"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "full-self-call-spill", on_key: Some("CRATONVM_JIT_FULL_SELF_CALL_SPILL"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "getfield-helper", on_key: Some("CRATONVM_JIT_GETFIELD_HELPER"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "helpful-npe-opcodes", on_key: Some("CRATONVM_HELPFUL_NPE_OPCODES"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "inclusive-bce", on_key: Some("CRATONVM_JIT_INCLUSIVE_BCE"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "inline-allow-static", on_key: Some("CRATONVM_INLINE_ALLOW_STATIC"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "inline-getfield", on_key: Some("CRATONVM_JIT_INLINE_GETFIELD"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "inline-new", on_key: None, off_key: Some("CRATONVM_JIT_DISABLE_INLINE_NEW"), off_word: None },
    E { group: Group::JIT, token: "inline-putfield", on_key: None, off_key: Some("CRATONVM_NO_JIT_INLINE_PUTFIELD"), off_word: None },
    E { group: Group::JIT, token: "inline-self-guard", on_key: Some("CRATONVM_JIT_INLINE_SELF_GUARD"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "inline-tlab-new", on_key: None, off_key: Some("CRATONVM_NO_JIT_INLINE_TLAB_NEW"), off_word: None },
    E { group: Group::JIT, token: "intrinsics", on_key: None, off_key: Some("CRATONVM_DISABLE_INTRINSICS"), off_word: None },
    E { group: Group::JIT, token: "ir-branchy", on_key: None, off_key: Some("CRATONVM_NO_IR_BRANCHY"), off_word: None },
    E { group: Group::JIT, token: "ir-call", on_key: Some("CRATONVM_JIT_IR_CALL"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "ir-call-special", on_key: Some("CRATONVM_JIT_IR_CALL_SPECIAL"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "ir-call-virtual", on_key: Some("CRATONVM_JIT_IR_CALL_VIRTUAL"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "ir-deopt-resume", on_key: Some("CRATONVM_IR_DEOPT_RESUME"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "ir-direct-call", on_key: Some("CRATONVM_JIT_IR_DIRECT_CALL"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "ir-fp", on_key: Some("CRATONVM_JIT_IR_FP"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "ir-long", on_key: Some("CRATONVM_JIT_IR_LONG"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "ir-selfrec-direct", on_key: Some("CRATONVM_JIT_IR_SELFREC_DIRECT"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "kernel-reg-locals", on_key: Some("CRATONVM_JIT_KERNEL_REG_LOCALS"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "kernel-reg-osr", on_key: Some("CRATONVM_JIT_KERNEL_REG_OSR"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "licm", on_key: Some("CRATONVM_JIT_LICM"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "local-liveness", on_key: None, off_key: Some("CRATONVM_NO_LOCAL_LIVENESS"), off_word: None },
    E { group: Group::JIT, token: "long-intrinsics", on_key: None, off_key: Some("CRATONVM_JIT_NO_LONG_INTRINSICS"), off_word: None },
    E { group: Group::JIT, token: "longroot-strict", on_key: Some("CRATONVM_LONGROOT_STRICT"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "main-inline", on_key: Some("CRATONVM_JIT_MAIN_INLINE"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "native-ec-multiply", on_key: Some("CRATONVM_NATIVE_EC_MULTIPLY"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "native-matcher-find", on_key: Some("CRATONVM_NATIVE_MATCHER_FIND"), off_key: None, off_word: Some("0") },
    E { group: Group::JIT, token: "native-pbe-keyfactory", on_key: Some("CRATONVM_NATIVE_PBE_KEYFACTORY"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "native-string-regex", on_key: Some("CRATONVM_NATIVE_STRING_REGEX"), off_key: None, off_word: Some("0") },
    E { group: Group::JIT, token: "old-sweep-jit", on_key: Some("CRATONVM_OLD_SWEEP_JIT"), off_key: None, off_word: Some("0") },
    E { group: Group::JIT, token: "osr", on_key: Some("CRATONVM_JIT_OSR"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "osr-newarray", on_key: Some("CRATONVM_OSR_NEWARRAY"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "precise-coverage-pin", on_key: Some("CRATONVM_PRECISE_COVERAGE_PIN"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "precise-inline-frame-record", on_key: None, off_key: Some("CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD"), off_word: None },
    E { group: Group::JIT, token: "precise-jit-maps", on_key: None, off_key: Some("CRATONVM_NO_PRECISE_JIT_MAPS"), off_word: None },
    E { group: Group::JIT, token: "precise-reg-spill", on_key: None, off_key: Some("CRATONVM_NO_PRECISE_REG_SPILL"), off_word: None },
    E { group: Group::JIT, token: "range-scan-legacy", on_key: Some("CRATONVM_JIT_RANGE_SCAN_LEGACY"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "reassoc", on_key: Some("CRATONVM_JIT_REASSOC"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "rootsnap-cache", on_key: Some("CRATONVM_ROOTSNAP_CACHE"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "rootsnap-cache-survive-gc", on_key: Some("CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "safepoint-polls", on_key: Some("CRATONVM_JIT_SAFEPOINT_POLLS"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "safepoint-reg-spill", on_key: Some("CRATONVM_JIT_SAFEPOINT_REG_SPILL"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "scalar-deopt", on_key: Some("CRATONVM_SCALAR_DEOPT"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "scalar-new", on_key: Some("CRATONVM_JIT_SCALAR_NEW"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "scalar-replacement", on_key: None, off_key: Some("CRATONVM_DISABLE_SCALAR_REPLACEMENT"), off_word: None },
    E { group: Group::JIT, token: "scan-cache", on_key: None, off_key: Some("CRATONVM_NO_JIT_SCAN_CACHE"), off_word: None },
    E { group: Group::JIT, token: "self-cache-inherit", on_key: None, off_key: Some("CRATONVM_JIT_NO_SELF_CACHE_INHERIT"), off_word: None },
    E { group: Group::JIT, token: "shadow-nopush", on_key: Some("CRATONVM_SHADOW_NOPUSH"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "shadow-noreload", on_key: Some("CRATONVM_SHADOW_NORELOAD"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "shadow-pin", on_key: Some("CRATONVM_SHADOW_PIN"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "shadow-raw-reload", on_key: Some("CRATONVM_SHADOW_RAW_RELOAD"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "shadow-savebase", on_key: None, off_key: Some("CRATONVM_SHADOW_NO_SAVEBASE"), off_word: None },
    E { group: Group::JIT, token: "shadow-stack", on_key: Some("CRATONVM_SHADOW_STACK"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "slot-mirror", on_key: None, off_key: Some("CRATONVM_JIT_NO_SLOT_MIRROR"), off_word: None },
    E { group: Group::JIT, token: "sp-coalesce", on_key: None, off_key: Some("CRATONVM_SP_NO_COALESCE"), off_word: None },
    E { group: Group::JIT, token: "spec-bce", on_key: None, off_key: Some("CRATONVM_JIT_NO_SPEC_BCE"), off_word: None },
    E { group: Group::JIT, token: "stack-bang", on_key: Some("CRATONVM_JIT_STACK_BANG"), off_key: Some("CRATONVM_JIT_NO_STACK_BANG"), off_word: None },
    E { group: Group::JIT, token: "strict-jit-roots", on_key: Some("CRATONVM_STRICT_JIT_ROOTS"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "threshold", on_key: Some("CRATONVM_JIT_THRESHOLD"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "tier-c1-threshold", on_key: Some("CRATONVM_TIER_C1_THRESHOLD"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "tier-c2-min-invocations", on_key: Some("CRATONVM_TIER_C2_MIN_INVOCATIONS"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "tier-c2-threshold", on_key: Some("CRATONVM_TIER_C2_THRESHOLD"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "tier-osr-backedge", on_key: Some("CRATONVM_TIER_OSR_BACKEDGE"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "tier-osr-threshold", on_key: Some("CRATONVM_TIER_OSR_THRESHOLD"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "tier-pgo", on_key: Some("CRATONVM_TIER_PGO"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "tiered", on_key: Some("CRATONVM_TIER_ENABLED"), off_key: None, off_word: Some("0") },
    E { group: Group::JIT, token: "unban-junitcore", on_key: Some("CRATONVM_JIT_UNBAN_JUNITCORE"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "unroll", on_key: Some("CRATONVM_JIT_UNROLL"), off_key: Some("CRATONVM_DISABLE_UNROLL"), off_word: None },
    E { group: Group::JIT, token: "virtual-tierup", on_key: Some("CRATONVM_JIT_VIRTUAL_TIERUP"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "xt-helper-window-scan", on_key: Some("CRATONVM_XT_HELPER_WINDOW_SCAN"), off_key: None, off_word: None },
    E { group: Group::JIT, token: "xt-jit-root-scan", on_key: Some("CRATONVM_XT_JIT_ROOT_SCAN"), off_key: None, off_word: None },
    E { group: Group::GC, token: "card-table-only", on_key: Some("CRATONVM_CARD_TABLE_ONLY"), off_key: None, off_word: None },
    E { group: Group::GC, token: "compact-ref-fields", on_key: Some("CRATONVM_COMPACT_REF_FIELDS"), off_key: None, off_word: None },
    E { group: Group::GC, token: "compressed-oops", on_key: Some("CRATONVM_COMPRESSED_OOPS"), off_key: None, off_word: None },
    E { group: Group::GC, token: "default-heap-ergonomics", on_key: Some("CRATONVM_DEFAULT_HEAP_ERGONOMICS"), off_key: None, off_word: None },
    E { group: Group::GC, token: "default-heap-max-mb", on_key: Some("CRATONVM_DEFAULT_HEAP_MAX_MB"), off_key: None, off_word: None },
    E { group: Group::GC, token: "g1-evac-retry", on_key: None, off_key: Some("CRATONVM_G1_NO_EVAC_RETRY"), off_word: None },
    E { group: Group::GC, token: "g1-parallel-evac", on_key: Some("CRATONVM_G1_PARALLEL_EVAC"), off_key: None, off_word: None },
    E { group: Group::GC, token: "g1-workers", on_key: Some("CRATONVM_G1_WORKERS"), off_key: None, off_word: None },
    E { group: Group::GC, token: "gpu-zerocopy", on_key: None, off_key: Some("CRATONVM_GPU_NO_ZEROCOPY"), off_word: None },
    E { group: Group::GC, token: "max-inflated-bytes", on_key: Some("CRATONVM_MAX_INFLATED_BYTES"), off_key: None, off_word: None },
    E { group: Group::GC, token: "moving-young", on_key: Some("CRATONVM_MOVING_YOUNG"), off_key: Some("CRATONVM_NO_MOVING_YOUNG"), off_word: None },
    E { group: Group::GC, token: "overhead-limit", on_key: Some("CRATONVM_GC_OVERHEAD_LIMIT"), off_key: None, off_word: None },
    E { group: Group::GC, token: "par-min-bytes", on_key: Some("CRATONVM_GC_PAR_MIN_BYTES"), off_key: None, off_word: None },
    E { group: Group::GC, token: "par-threads", on_key: Some("CRATONVM_GC_PAR_THREADS"), off_key: None, off_word: None },
    E { group: Group::GC, token: "promotion-guard", on_key: None, off_key: Some("CRATONVM_NO_GC_PROMOTION_GUARD"), off_word: None },
    E { group: Group::GC, token: "promotion-oom-guard-broad", on_key: Some("CRATONVM_PROMOTION_OOM_GUARD_BROAD"), off_key: None, off_word: None },
    E { group: Group::GC, token: "selective-promote", on_key: None, off_key: Some("CRATONVM_NO_SELECTIVE_PROMOTE"), off_word: None },
    E { group: Group::GC, token: "stress", on_key: Some("CRATONVM_GC_STRESS"), off_key: None, off_word: None },
    E { group: Group::GC, token: "sweep-anchor-stride", on_key: Some("CRATONVM_GC_SWEEP_ANCHOR_STRIDE"), off_key: None, off_word: None },
    E { group: Group::GC, token: "tlab-gc-trigger", on_key: Some("CRATONVM_TLAB_GC_TRIGGER"), off_key: None, off_word: None },
    E { group: Group::GC, token: "weakref-clear", on_key: Some("CRATONVM_WEAKREF_CLEAR"), off_key: None, off_word: None },
    E { group: Group::GC, token: "youngscan-stride", on_key: Some("CRATONVM_YOUNGSCAN_STRIDE"), off_key: None, off_word: None },
    E { group: Group::REAL, token: "agroal", on_key: Some("CRATONVM_REAL_AGROAL"), off_key: Some("CRATONVM_SYNTHETIC_AGROAL"), off_word: None },
    // `CRATONVM_REAL` itself is the group variable, so it is not a row here.
    // It already was a comma-separated token list before this refactor —
    // `vm::runtime::env_cache::RealSelector::parse` reads `all`, `jca` and bare
    // internal class names straight out of it — which is where the grouped
    // variable syntax came from. That parse still runs on the raw value.
    //
    // `off_word` is unset on the four twin rows below because `off_key` already
    // expresses "off": the synthetic variable beats the default-ON real one at
    // the single call site that reads both.
    E { group: Group::REAL, token: "annotations", on_key: Some("CRATONVM_REAL_ANNOTATIONS"), off_key: Some("CRATONVM_SYNTHETIC_ANNOTATIONS"), off_word: None },
    E { group: Group::REAL, token: "aqs", on_key: Some("CRATONVM_REAL_AQS"), off_key: Some("CRATONVM_SYNTHETIC_AQS"), off_word: None },
    E { group: Group::REAL, token: "buffered-writer", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_BUFFERED_WRITER"), off_word: None },
    E { group: Group::REAL, token: "dsa", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_DSA"), off_word: None },
    E { group: Group::REAL, token: "ec", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_EC"), off_word: None },
    E { group: Group::REAL, token: "eqe", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_EQE"), off_word: None },
    E { group: Group::REAL, token: "filewriter", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_FILEWRITER"), off_word: None },
    E { group: Group::REAL, token: "forkjoinpool", on_key: Some("CRATONVM_REAL_FORKJOINPOOL"), off_key: None, off_word: None },
    E { group: Group::REAL, token: "jca", on_key: Some("CRATONVM_REAL_JCA"), off_key: None, off_word: None },
    E { group: Group::REAL, token: "msc-real-start", on_key: Some("CRATONVM_MSC_REAL_START"), off_key: None, off_word: Some("off") },
    E { group: Group::REAL, token: "net-sockets", on_key: Some("CRATONVM_REAL_NET_SOCKETS"), off_key: None, off_word: None },
    E { group: Group::REAL, token: "pqc", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_PQC"), off_word: None },
    E { group: Group::REAL, token: "proxy", on_key: Some("CRATONVM_REAL_PROXY"), off_key: None, off_word: Some("0") },
    E { group: Group::REAL, token: "proxy-strict", on_key: Some("CRATONVM_REAL_PROXY_STRICT"), off_key: None, off_word: None },
    E { group: Group::REAL, token: "proxy-super", on_key: Some("CRATONVM_REAL_PROXY_SUPER"), off_key: None, off_word: Some("0") },
    E { group: Group::REAL, token: "quarkus-arc", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_QUARKUS_ARC"), off_word: None },
    E { group: Group::REAL, token: "quarkus-start", on_key: Some("CRATONVM_REAL_QUARKUS_START"), off_key: None, off_word: None },
    E { group: Group::REAL, token: "raf", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_RAF"), off_word: None },
    E { group: Group::REAL, token: "rsa", on_key: None, off_key: Some("CRATONVM_SYNTHETIC_RSA"), off_word: None },
    E { group: Group::REAL, token: "stax-factory", on_key: Some("CRATONVM_REAL_STAX_FACTORY"), off_key: None, off_word: Some("0") },
    E { group: Group::REAL, token: "stubs", on_key: None, off_key: Some("CRATONVM_NO_STUBS"), off_word: None },
    E { group: Group::REAL, token: "use-wildfly-reflect-shim", on_key: Some("CRATONVM_USE_WILDFLY_REFLECT_SHIM"), off_key: None, off_word: None },
    E { group: Group::REAL, token: "use-wildfly-synth-bytecode", on_key: Some("CRATONVM_USE_WILDFLY_SYNTH_BYTECODE"), off_key: None, off_word: None },
    E { group: Group::REAL, token: "vertx", on_key: Some("CRATONVM_REAL_VERTX"), off_key: Some("CRATONVM_SYNTHETIC_VERTX"), off_word: None },
    E { group: Group::LOADER, token: "allow-jsr-ret", on_key: Some("CRATONVM_ALLOW_JSR_RET"), off_key: None, off_word: None },
    E { group: Group::LOADER, token: "aware-resolution", on_key: Some("CRATONVM_LOADER_AWARE_RESOLUTION"), off_key: None, off_word: None },
    E { group: Group::LOADER, token: "boot-module-registry", on_key: Some("CRATONVM_BOOT_MODULE_REGISTRY"), off_key: None, off_word: Some("off") },
    E { group: Group::LOADER, token: "cl-bootstrap-scoped", on_key: Some("CRATONVM_CL_BOOTSTRAP_SCOPED"), off_key: None, off_word: Some("0") },
    E { group: Group::LOADER, token: "fwd-resolve-strict", on_key: Some("CRATONVM_FWD_RESOLVE_STRICT"), off_key: None, off_word: None },
    E { group: Group::LOADER, token: "jar-mmap", on_key: None, off_key: Some("CRATONVM_DISABLE_JAR_MMAP"), off_word: None },
    E { group: Group::LOADER, token: "lenient-clinit", on_key: Some("CRATONVM_LENIENT_CLINIT"), off_key: None, off_word: None },
    E { group: Group::LOADER, token: "longrewrite-loose", on_key: Some("CRATONVM_LONGREWRITE_LOOSE"), off_key: None, off_word: None },
    E { group: Group::LOADER, token: "resolve-cache-cap", on_key: Some("CRATONVM_RESOLVE_CACHE_CAP"), off_key: None, off_word: None },
    E { group: Group::LOADER, token: "unload", on_key: Some("CRATONVM_LOADER_UNLOAD"), off_key: None, off_word: Some("0") },
    E { group: Group::IO, token: "canon-openfile", on_key: Some("CRATONVM_CANON_OPENFILE"), off_key: None, off_word: None },
    E { group: Group::IO, token: "http-max-body", on_key: Some("CRATONVM_HTTP_MAX_BODY"), off_key: None, off_word: None },
    E { group: Group::IO, token: "netty-queue-bridge", on_key: Some("CRATONVM_NETTY_QUEUE_BRIDGE"), off_key: None, off_word: Some("0") },
    E { group: Group::IO, token: "resolve-outbound-host", on_key: Some("CRATONVM_RESOLVE_OUTBOUND_HOST"), off_key: None, off_word: None },
    E { group: Group::IO, token: "select-max-block-ms", on_key: Some("CRATONVM_SELECT_MAX_BLOCK_MS"), off_key: None, off_word: None },
    E { group: Group::IO, token: "selector-connect-probe", on_key: None, off_key: Some("CRATONVM_NO_SELECTOR_CONNECT_PROBE"), off_word: None },
    E { group: Group::IO, token: "socket-capture", on_key: Some("CRATONVM_SOCKET_CAPTURE"), off_key: None, off_word: None },
    E { group: Group::IO, token: "uri-strict-chars", on_key: Some("CRATONVM_URI_STRICT_CHARS"), off_key: None, off_word: Some("0") },
    E { group: Group::IO, token: "zip-max-entry-bytes", on_key: Some("CRATONVM_ZIP_MAX_ENTRY_BYTES"), off_key: None, off_word: None },
    E { group: Group::THREADS, token: "assert-single-os-thread", on_key: Some("CRATONVM_ASSERT_SINGLE_OS_THREAD"), off_key: None, off_word: None },
    E { group: Group::THREADS, token: "async-handoff-sleep-floor-ms", on_key: Some("CRATONVM_ASYNC_HANDOFF_SLEEP_FLOOR_MS"), off_key: None, off_word: None },
    E { group: Group::THREADS, token: "async-submit-grace-ms", on_key: Some("CRATONVM_ASYNC_SUBMIT_GRACE_MS"), off_key: None, off_word: None },
    E { group: Group::THREADS, token: "async-worker-sleep-floor-ms", on_key: Some("CRATONVM_ASYNC_WORKER_SLEEP_FLOOR_MS"), off_key: None, off_word: None },
    E { group: Group::THREADS, token: "await-shortcircuit", on_key: None, off_key: Some("CRATONVM_AWAIT_NO_SHORTCIRCUIT"), off_word: None },
    E { group: Group::THREADS, token: "default-watchdog", on_key: None, off_key: Some("CRATONVM_DISABLE_DEFAULT_WATCHDOG"), off_word: None },
    E { group: Group::THREADS, token: "default-watchdog-sec", on_key: Some("CRATONVM_DEFAULT_WATCHDOG_SEC"), off_key: None, off_word: None },
    E { group: Group::THREADS, token: "eqe-sync-execute", on_key: Some("CRATONVM_EQE_SYNC_EXECUTE"), off_key: None, off_word: None },
    E { group: Group::THREADS, token: "exec-depth-ceiling", on_key: Some("CRATONVM_EXEC_DEPTH_CEILING"), off_key: None, off_word: None },
    E { group: Group::THREADS, token: "inherit-thread-ccl", on_key: Some("CRATONVM_INHERIT_THREAD_CCL"), off_key: None, off_word: Some("0") },
    E { group: Group::THREADS, token: "inherit-tl-workaround", on_key: Some("CRATONVM_INHERIT_TL_WORKAROUND"), off_key: None, off_word: Some("0") },
    E { group: Group::THREADS, token: "lock-order-check", on_key: Some("CRATONVM_LOCK_ORDER_CHECK"), off_key: None, off_word: None },
    E { group: Group::THREADS, token: "thread-start-grace-ms", on_key: Some("CRATONVM_THREAD_START_GRACE_MS"), off_key: None, off_word: None },
    E { group: Group::SECURITY, token: "aot-hmac-key", on_key: Some("CRATONVM_AOT_HMAC_KEY"), off_key: None, off_word: None },
    E { group: Group::SECURITY, token: "block-private-nets", on_key: Some("CRATONVM_BLOCK_PRIVATE_NETS"), off_key: None, off_word: None },
    E { group: Group::SECURITY, token: "confine-io", on_key: Some("CRATONVM_CONFINE_IO"), off_key: None, off_word: None },
    E { group: Group::SECURITY, token: "harden-manifest-classpath", on_key: Some("CRATONVM_HARDEN_MANIFEST_CLASSPATH"), off_key: None, off_word: None },
    E { group: Group::SECURITY, token: "require-policy", on_key: Some("CRATONVM_REQUIRE_POLICY"), off_key: None, off_word: None },
    E { group: Group::SECURITY, token: "trust-pem", on_key: Some("CRATONVM_TRUST_PEM"), off_key: None, off_word: None },
    E { group: Group::SECURITY, token: "untrusted-code", on_key: Some("CRATONVM_UNTRUSTED_CODE"), off_key: None, off_word: None },
    E { group: Group::COMPAT, token: "eager-streams", on_key: Some("CRATONVM_EAGER_STREAMS"), off_key: None, off_word: None },
    E { group: Group::COMPAT, token: "foreign-attach", on_key: Some("CRATONVM_FOREIGN_ATTACH"), off_key: None, off_word: None },
    E { group: Group::COMPAT, token: "jboss-boot-log-file", on_key: Some("CRATONVM_JBOSS_BOOT_LOG_FILE"), off_key: None, off_word: None },
    E { group: Group::COMPAT, token: "jboss-brute-force-jars", on_key: Some("CRATONVM_JBOSS_BRUTE_FORCE_JARS"), off_key: None, off_word: None },
    E { group: Group::COMPAT, token: "jboss-logger-base-emit", on_key: Some("CRATONVM_JBOSS_LOGGER_BASE_EMIT"), off_key: None, off_word: None },
    E { group: Group::COMPAT, token: "jboss-mp-root", on_key: Some("CRATONVM_JBOSS_MP_ROOT"), off_key: None, off_word: None },
    E { group: Group::COMPAT, token: "lazy-streams", on_key: Some("CRATONVM_LAZY_STREAMS"), off_key: None, off_word: None },
    E { group: Group::COMPAT, token: "mockito-legacy-selectors", on_key: Some("CRATONVM_MOCKITO_LEGACY_SELECTORS"), off_key: None, off_word: None },
    E { group: Group::COMPAT, token: "strict-swallows", on_key: Some("CRATONVM_STRICT_SWALLOWS"), off_key: None, off_word: None },
    E { group: Group::COMPAT, token: "tomcat-mapper-natives", on_key: Some("CRATONVM_TOMCAT_MAPPER_NATIVES"), off_key: None, off_word: Some("0") },
    E { group: Group::TEST, token: "force-win-build", on_key: Some("CRATONVM_FORCE_WIN_BUILD"), off_key: None, off_word: None },
    E { group: Group::TEST, token: "jdk", on_key: Some("CRATONVM_TEST_JDK"), off_key: None, off_word: None },
    E { group: Group::TEST, token: "segv", on_key: Some("CRATONVM_TEST_SEGV"), off_key: None, off_word: None },
    E { group: Group::TEST, token: "soak-iters", on_key: Some("CRATONVM_SOAK_ITERS"), off_key: None, off_word: None },
    E { group: Group::TEST, token: "soak-k", on_key: Some("CRATONVM_SOAK_K"), off_key: None, off_word: None },
    E { group: Group::TEST, token: "soak-method", on_key: Some("CRATONVM_SOAK_METHOD"), off_key: None, off_word: None },
    E { group: Group::TEST, token: "soak-timeout-secs", on_key: Some("CRATONVM_SOAK_TIMEOUT_SECS"), off_key: None, off_word: None },
    E { group: Group::TEST, token: "soak-xmx", on_key: Some("CRATONVM_SOAK_XMX"), off_key: None, off_word: None },
    E { group: Group::TEST, token: "var", on_key: Some("CRATONVM_TEST_VAR"), off_key: None, off_word: None },
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

    #[test]
    fn every_token_is_unique() {
        let mut seen: Vec<(Group, &str)> = INVENTORY.iter().map(|e| (e.group, e.token)).collect();
        let before = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            before,
            seen.len(),
            "two entries claim the same (group, token); merge them into one \
             entry carrying both an on_key and an off_key instead"
        );
    }

    #[test]
    fn every_legacy_key_is_claimed_once() {
        let mut keys: Vec<&str> = INVENTORY
            .iter()
            .flat_map(|e| [e.on_key, e.off_key].into_iter().flatten())
            .collect();
        let before = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(
            before,
            keys.len(),
            "a legacy variable is claimed by more than one token, so its \
             canonical spelling is ambiguous"
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

    #[test]
    fn the_documented_no_ops_now_work() {
        // Each of these was documented for months and read by nothing: the
        // default was flipped and only the opt-out half was renamed.
        // docs/internal/flag-census.md section 3 has the full list.
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

    #[test]
    fn the_whole_surface_is_fifteen_variables() {
        // This is the number the refactor exists to hold down. Raising it wants
        // an argument, not a merge.
        assert_eq!(Group::ALL.len() + SCALARS.len(), 15);
    }
}
