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

use std::collections::HashSet;
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

/// `CRATONVM_JIT_THRESHOLD` — invocation count at which a method becomes
/// JIT-compilation eligible (default 500). Read once and cached. Used by both
/// the interpreter's per-method upgrade gate and the dispatch-helper gate so a
/// single knob controls JIT eagerness. Raising it keeps short-lived / call-heavy
/// code interpreted (HotSpot's interpreter-first behaviour); genuinely-hot
/// compute loops cross any reasonable threshold and still compile. Invalid /
/// unset values fall back to 500. A value of 0 is clamped to 1 (0 would compile
/// on the first call, defeating warmup).
#[inline]
pub fn jit_invocation_threshold() -> u32 {
    static CACHE: OnceLock<u32> = OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("CRATONVM_JIT_THRESHOLD")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map(|v| v.max(1))
            .unwrap_or(500)
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
// Opt-in: enable small-method inlining in the main JIT tier-up compile path.
// Default-OFF until a `try_emit_inline_body` miscompile (Spring boot enum CCE)
// is root-caused. See `try_jit_upgrade_with_gate`.
cached_is_set!(jit_main_inline, "CRATONVM_JIT_MAIN_INLINE");
cached_is_set!(jit_mic_dbg, "CRATONVM_DBG_JIT_MIC");
cached_is_set!(jit_entry_dbg, "CRATONVM_DBG_JIT_ENTRY");
cached_is_set!(jit_putfield_diag, "CRATONVM_DBG_JIT_PUTFIELD");
cached_is_set!(letsgo_dbg, "CRATONVM_DBG_LETSGO");
/// `CRATON_JIT_PFO_TRACE` / `CRATON_JIT_PFI_TRACE` / `CRATON_JIT_NEWARRAY_TRACE`
/// — suspect-pattern tracing in the JIT putfield/newarray helpers. These sit on
/// the hottest allocation/store paths (`jit_putfield_object` runs once per
/// reference field store), where an uncached `env::var_os` is a per-call
/// environment-block scan under the process env lock — measured as a
/// multi-second cost on allocation-heavy runs (bintrees18: ~69M ref stores).
cached_is_set!(jit_pfo_trace, "CRATON_JIT_PFO_TRACE");
cached_is_set!(jit_pfi_trace, "CRATON_JIT_PFI_TRACE");
cached_is_set!(jit_newarray_trace, "CRATON_JIT_NEWARRAY_TRACE");

// ── Frame-trace and interpreter hot-path flags ──────────────────────────

cached_is_set!(frame_trace, "CRATONVM_FRAME_TRACE");
cached_is_set!(iae_trace_os, "CRATONVM_IAE_TRACE");
cached_is_set!(bd_debug, "CRATONVM_BD_DEBUG");
cached_is_set!(nocode_dbg, "CRATONVM_DBG_NOCODE");
cached_is_set!(nsme_dbg, "CRATONVM_DBG_NSME");
cached_is_set!(cce_dbg, "CRATONVM_DBG_CCE");
/// `CRATONVM_DBG_OVERLAY` — a native `set_field`/`get_field` whose value tag
/// is destructively cross-type with the bound class's declared field
/// descriptor at that slot (a primitive written to a reference field, or a
/// reference written to a primitive field). This is the "synthetic overlay
/// bound to a real JDK class" corruption: the slot's real descriptor coerces
/// the overlay value to null / numeric and silently loses it (the
/// LinkedList$ListItr cursor bug). The hunter logs class + slot + value +
/// descriptor + native caller so every instance can be enumerated in one run.
cached_is_set!(overlay_corruption_dbg, "CRATONVM_DBG_OVERLAY");
cached_is_set!(lambda_dbg, "CRATONVM_DBG_LAMBDA");
cached_is_set!(resume_pc_dbg, "CRATONVM_DBG_RESUME_PC");
/// `CRATONVM_DBG_BADRECV` — on a getfield/putfield/array/invoke receiver whose
/// pointer is not a valid heap address (the H2 TestScript SEGV: a corrupted
/// `Object(Some(ptr=6))` reaching `get_field` → header read faults), log the
/// Java frame stack + field + Rust backtrace and raise an NPE instead of
/// dereferencing the wild pointer, so the origin can be localized.
cached_is_set!(badrecv_dbg, "CRATONVM_DBG_BADRECV");
/// `CRATONVM_DBG_FIELDADDR` — trace put/get of specific instance fields
/// (object address + resolved slot) to localize a write that does not reach
/// the read site (e.g. the EnhancedQueueExecutor constructor field gap).
cached_is_set!(field_addr_dbg, "CRATONVM_DBG_FIELDADDR");
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

// ── `CRATONVM_REAL` — synthetic-stub differential switch ─────────────────

/// Parsed, process-lifetime view of the `CRATONVM_REAL` (and legacy
/// `CRATONVM_REAL_JCA`) selector. Decides, per class, whether a
/// `NativeKind::SyntheticStub` registration should be bypassed in favour of
/// running the class's real bytecode — so fakes can be differentially
/// compared against the genuine implementation.
///
/// Parsed exactly once from the environment by [`real_bytecode_selector`].
/// When neither env var is set, every field is empty/false and
/// [`RealSelector::prefers_real`] returns `false` for all classes — so the
/// dispatcher behaves byte-for-byte as it does today.
pub struct RealSelector {
    /// `CRATONVM_REAL` contained the `all` token — prefer real for every class.
    all_flag: bool,
    /// Exact internal-form class names listed in `CRATONVM_REAL`
    /// (e.g. `java/util/stream/Collectors`).
    exact_classes: HashSet<String>,
    /// The `jca` group alias was requested (via the `jca` token in
    /// `CRATONVM_REAL`, or the legacy `CRATONVM_REAL_JCA` var being set):
    /// prefer real for the `java/security/`, `javax/crypto/`, and
    /// `sun/security/` families.
    jca_group: bool,
}

impl RealSelector {
    /// Parse a `RealSelector` from the two env vars. `real` is the raw value
    /// of `CRATONVM_REAL` (if present); `jca_legacy` is whether the legacy
    /// `CRATONVM_REAL_JCA` var is set (non-empty).
    fn parse(real: Option<&str>, jca_legacy: bool) -> Self {
        let mut all_flag = false;
        let mut exact_classes = HashSet::new();
        let mut jca_group = jca_legacy;
        if let Some(raw) = real {
            for tok in raw.split(',') {
                let tok = tok.trim();
                if tok.is_empty() {
                    continue;
                }
                match tok {
                    "all" => all_flag = true,
                    "jca" => jca_group = true,
                    other => {
                        exact_classes.insert(other.to_string());
                    }
                }
            }
        }
        RealSelector {
            all_flag,
            exact_classes,
            jca_group,
        }
    }

    /// `true` iff real bytecode should be preferred over a synthetic-stub
    /// native for `class_name` (internal form, e.g.
    /// `java/util/stream/Collectors`). O(1)-ish: a bool, a hash-set membership
    /// test, and at most three `starts_with` checks.
    #[inline]
    pub fn prefers_real(&self, class_name: &str) -> bool {
        if self.all_flag {
            return true;
        }
        if self.exact_classes.contains(class_name) {
            return true;
        }
        if self.jca_group
            && (class_name.starts_with("java/security/")
                || class_name.starts_with("javax/crypto/")
                || class_name.starts_with("sun/security/"))
        {
            return true;
        }
        false
    }
}

/// Once-initialized accessor for the `CRATONVM_REAL` / `CRATONVM_REAL_JCA`
/// differential switch. Parses the env vars exactly once; every subsequent
/// call serves the cached [`RealSelector`].
#[inline]
pub fn real_bytecode_selector() -> &'static RealSelector {
    static CACHE: OnceLock<RealSelector> = OnceLock::new();
    CACHE.get_or_init(|| {
        let real = std::env::var("CRATONVM_REAL").ok();
        let jca_legacy = std::env::var_os("CRATONVM_REAL_JCA")
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        RealSelector::parse(real.as_deref(), jca_legacy)
    })
}
