// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `CRATONVM_DBG_STALE_OBJREF` — hard-panic assertion for stale native
//! `ObjectRef` reads (gated, default-inert).
//!
//! See docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md
//! and docs/internal/wildfly-stale-objectref-debug-assertion-scoping.md for
//! the full writeup of the bug class this catches and the design rationale.
//!
//! In one sentence: a native Rust function that captures a raw `ObjectRef`
//! (from an arg, a `get_field`, or a `ctx.*` allocator), then makes a call
//! that can trigger a moving GC, then reuses the *original* local — reading
//! whatever now occupies that address — corrupts silently today. When this
//! flag is set, the `Generational` heap backend keeps each minor GC cycle's
//! just-evacuated (all-garbage) young from-space arena intact for one *extra*
//! cycle instead of zeroing it immediately (see
//! [`crate::gen_heap::GenerationalHeap`]'s `quarantine` field), so a stale
//! reference into it still carries a valid forwarding pointer
//! (`ObjectHeader::is_forwarded()`) rather than reading back all-zero.
//! [`crate::gen_heap::GenerationalHeap::get_header`] turns that into a hard
//! panic instead of silently returning a header describing the wrong
//! object (or a zeroed one).
//!
//! Scope: this only instruments the `Generational` backend (the default —
//! see `vm/src/config.rs`'s `GcAlgorithm::default()`) and only the
//! `get_header`-mediated accessors (`get_field`/`set_field`/`class_id_of`/
//! `array_length`/`identity_hash_code`/etc. — i.e. exactly what
//! `NativeContext` methods and ordinary interpreter bytecode dispatch use).
//! It does NOT cover G1 or ZGC (their evacuation paths were not touched —
//! extending this to them is follow-up work, same "needs its own dedicated
//! session" caveat as the rest of this bug class's GC-level work), and it
//! does NOT cover a stale reference read through
//! [`crate::vm_heap::VmHeap::load_and_forward`] (a handful of specific
//! self-healing call sites in `vm/src/runtime/interpreter.rs` /
//! `vm/src/vm/vm_exec.rs` that do their own raw header read rather than
//! going through `get_header`) or through JIT-compiled code's guarded-inline
//! `getfield` fast path (which bypasses `get_header` for addresses inside
//! the published young-from/young-to/old-gen bounds — the separate quarantine
//! arena here is deliberately never published into those bounds, so it falls
//! through to the checked path instead, but that checked path's own
//! validation was not audited as part of this change).

use std::sync::OnceLock;

/// Cached `CRATONVM_DBG_STALE_OBJREF` gate. Read once per process; setting
/// the variable after the first read has no effect (same convention as
/// every other `CRATONVM_DBG_*` flag — see `vm/src/runtime/env_cache.rs`).
#[inline]
pub fn enabled() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| std::env::var_os("CRATONVM_DBG_STALE_OBJREF").is_some())
}
