// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT runtime helper functions — called from JIT-compiled code via absolute CALL.
//!
//! Decomposed from `vm/src/jit/helpers.rs` into modular runtime components:
//! - `thread.rs`: JIT thread state, stack guards, mic_prof, disp_census, callee guards.
//! - `alloc.rs`: Object, array, multi-array allocations, TLAB bump-pointer, ZGC tracking.
//! - `access.rs`: Field access, static field access, array loads/stores, write barriers.
//! - `typecheck.rs`: Checkcast, instanceof, site refusals, exception throwing & handling.
//! - `dispatch.rs`: Invoke dispatch, monomorphic inline caching, indy bridge, lambda bridges.
//! - `fast_native.rs`: Direct fast-native implementations (arraycopy, VarHandle).
//! - `deopt.rs`: Callee deoptimization, not-entrant stubs, uncommon traps.
//! - `table.rs`: JIT runtime helper table construction and miscellaneous entry points.

include!("thread.rs");
include!("alloc.rs");
include!("access.rs");
include!("typecheck.rs");
include!("dispatch.rs");
include!("fast_native.rs");
include!("deopt.rs");
include!("table.rs");

// j27 wave 7 (lane T1LAM3): a trap out of a stale body makes that body not
// entrant (`CRATONVM_JIT_STALE_TRAP_NOT_ENTRANT`, opt-in); and the one way the
// VM takes the stash's trapping body (`taken_trapping_body_retained`).
pub(crate) mod stale_trap_not_entrant;

/// Full concatenated source code of all JIT runtime helper components, preserving
/// the exact source layout of the original `vm/src/jit/helpers.rs` monolith for
/// source-pinning tests across the workspace.
pub const HELPERS_SRC: &str = concat!(
    include_str!("thread.rs"),
    include_str!("alloc.rs"),
    include_str!("access.rs"),
    include_str!("typecheck.rs"),
    include_str!("dispatch.rs"),
    include_str!("fast_native.rs"),
    include_str!("deopt.rs"),
    include_str!("table.rs"),
);

/// The split helper files, relative to `vm/src/jit/helpers/`, in exactly the
/// order [`HELPERS_SRC`] concatenates them. Keep the two lists in step.
#[cfg(test)]
pub(crate) const HELPERS_PARTS: [&str; 8] = [
    "thread.rs",
    "alloc.rs",
    "access.rs",
    "typecheck.rs",
    "dispatch.rs",
    "fast_native.rs",
    "deopt.rs",
    "table.rs",
];

/// Working-tree read of the helper sources: the same text as [`HELPERS_SRC`],
/// but read from disk at run time rather than baked in at compile time.
///
/// For the source witnesses that read `vm/src/jit/helpers.rs` at run time on
/// purpose (`source-witness-tests-read-the-working-tree`: an unbuilt edit must
/// still fail them). `CARGO_MANIFEST_DIR` is `<repo>/vm`.
#[cfg(test)]
pub(crate) fn read_helpers_src_from_disk() -> String {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/jit/helpers");
    let mut out = String::new();
    for part in HELPERS_PARTS {
        let path = dir.join(part);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        out.push_str(&text);
    }
    out
}
