// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Process-lifetime cache for the `CRATONVM_*` debug/trace environment
//! variables that are read from interpreter and JIT hot paths.
//!
//! `std::env::var` / `std::env::var_os` are surprisingly expensive on every
//! platform:
//!
//! * On Linux they take a global mutex in `libc::getenv` plus an `OsString`
//!   allocation and a `String` UTF-8 validation (for `var`).
//! * On Windows `GetEnvironmentVariableW` is a syscall (~500 ns) that copies
//!   the value into a wide buffer; `var` then decodes the UTF-16 to UTF-8.
//!
//! Reading any of these flags on every JIT entry, frame push/pop, branch or
//! exception throw burns measurable CPU and serialises threads on the libc
//! env-lock. Since the flags are only meant to be set once at process start
//! (`CRATONVM_FRAME_TRACE=1 java …`), we cache the parsed boolean (or value)
//! the first time it is read and serve every subsequent query from the
//! cached `OnceLock`. Setting the variable after the first read will have
//! no effect — same semantics as `runtime::exceptions::iae_trace_enabled`,
//! which has used this pattern since the original audit.
//!
//! Each helper is `#[inline]` so the cold first-call cost (one `getenv`
//! plus the `OnceLock::get_or_init` CAS) is paid exactly once per flag,
//! and steady-state cost collapses to a relaxed load of the `OnceLock`.

use std::sync::OnceLock;

/// Build a boolean predicate that returns `true` iff the named env var is
/// **set** (any value, including the empty string), matching the semantics
/// of `std::env::var_os(NAME).is_some()`.
macro_rules! cached_is_set {
    ($name:ident, $env:literal) => {
        #[inline]
        pub fn $name() -> bool {
            static CACHE: OnceLock<bool> = OnceLock::new();
            *CACHE.get_or_init(|| std::env::var_os($env).is_some())
        }
    };
}

/// Build a boolean predicate matching `std::env::var(NAME).is_ok()` —
/// behaviourally identical to `is_set` on every platform we target, but
/// kept as a separate macro so the call sites that previously used `var`
/// instead of `var_os` keep their exact semantics (e.g. the var being
/// unset returns `false`; an invalid-UTF-8 OsString would technically
/// have differed, but we don't set our flags to non-UTF-8 in practice).
macro_rules! cached_is_ok {
    ($name:ident, $env:literal) => {
        #[inline]
        pub fn $name() -> bool {
            static CACHE: OnceLock<bool> = OnceLock::new();
            *CACHE.get_or_init(|| std::env::var($env).is_ok())
        }
    };
}

// ── Flags read from the JIT entry / dispatch hot path ───────────────────

/// `CRATONVM_DISABLE_JIT` — kill-switch that forces interpreter-only
/// execution. Read at every `jit_invoke_dispatch` call, every OSR
/// candidate, and several other JIT entry points. Semantics: treat empty
/// or `"0"` as disabled, anything else as enabled — same as the original
/// inline check.
#[inline]
pub fn disable_jit() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| match std::env::var("CRATONVM_DISABLE_JIT") {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => false,
    })
}

/// `CRATONVM_DISABLE_INTRINSICS` — kill-switch that prevents the interpreter
/// from ever populating a `CachedInvokeTarget::Intrinsic` inline-cache entry,
/// forcing every call through the ordinary native/bytecode dispatch path.
///
/// This is the off-switch for the intrinsic-table differential tests: running
/// a program once with intrinsics on and once with this set to `1` must
/// produce identical output. Semantics match `disable_jit()` — empty or `"0"`
/// is treated as disabled (i.e. intrinsics stay enabled), anything else
/// disables intrinsics.
#[inline]
pub fn intrinsics_disabled() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| match std::env::var("CRATONVM_DISABLE_INTRINSICS") {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => false,
    })
}

cached_is_set!(jit_dispatch_dbg, "CRATONVM_DBG_JIT_DISPATCH");
cached_is_set!(jit_mic_dbg, "CRATONVM_DBG_JIT_MIC");
cached_is_set!(jit_entry_dbg, "CRATONVM_DBG_JIT_ENTRY");
cached_is_set!(letsgo_dbg, "CRATONVM_DBG_LETSGO");

// ── Frame-trace and interpreter hot-path flags ──────────────────────────

cached_is_set!(frame_trace, "CRATONVM_FRAME_TRACE");
cached_is_set!(iae_trace_os, "CRATONVM_IAE_TRACE");
cached_is_set!(bd_debug, "CRATONVM_BD_DEBUG");
cached_is_set!(nocode_dbg, "CRATONVM_DBG_NOCODE");
cached_is_set!(nsme_dbg, "CRATONVM_DBG_NSME");
cached_is_set!(cce_dbg, "CRATONVM_DBG_CCE");
cached_is_set!(lambda_dbg, "CRATONVM_DBG_LAMBDA");
cached_is_set!(resume_pc_dbg, "CRATONVM_DBG_RESUME_PC");
/// `CRATONVM_DBG_JETTY` — trace dispatch into the `org/eclipse/jetty/start/`
/// launcher package (every invokevirtual/invokespecial receiver + args) so
/// the boot-test NPE at `Main.start(Main.java:397)` can be pinpointed.
cached_is_set!(dbg_jetty, "CRATONVM_DBG_JETTY");

// ── Flags read via `env::var(...).is_ok()` ──────────────────────────────

cached_is_ok!(trace_sb_filter, "CRATONVM_TRACE_SB_FILTER");
cached_is_ok!(nsee_trace, "CRATONVM_NSEE_TRACE");
cached_is_ok!(iae_trace, "CRATONVM_IAE_TRACE");
cached_is_ok!(athrow_dbg, "CRATONVM_DBG_ATHROW");
cached_is_ok!(npe_invoke_dbg, "CRATONVM_DBG_NPE_INVOKE");

/// `CRATONVM_DBG_CHARSET=1` — targeted diagnostic for the bare
/// `NullPointerException: charset` blocker (keycloak26 Picocli /
/// Hazelcast JAXP-XPath). When set, the `Athrow` opcode handler and
/// `throw_runtime_error` dump the full live Java thread stack
/// (`class.method:pc` for every frame) the moment an NPE whose message
/// is exactly `charset` is thrown — pinpointing the JDK method and app
/// call site that dereferenced a null `Charset`.
cached_is_ok!(charset_dbg, "CRATONVM_DBG_CHARSET");

// ── `CRATONVM_STRICT_SWALLOWS` is read for its value, not just presence ──

/// `CRATONVM_STRICT_SWALLOWS=1` switches several diagnostic sites into a
/// hard-fail mode for swallowed exceptions. Cached as a single bool —
/// only the literal `"1"` is honoured (matching the existing call sites
/// `... .as_deref() == Some("1")`).
#[inline]
pub fn strict_swallows() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| match std::env::var("CRATONVM_STRICT_SWALLOWS") {
        Ok(v) => v == "1",
        Err(_) => false,
    })
}
