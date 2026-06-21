// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! GC-Interpreter integration — REMOVED.
//!
//! This module previously contained a stand-alone, parallel "GC integration"
//! model (TLAB, write barriers, safepoint manager, and a root scanner) that was
//! **never wired into the real garbage collector**. Its allocation path drove a
//! fake address allocator — a global `AtomicUsize` counter handing out fictitious
//! `0x1000_0000+` "addresses" that mapped to no actual heap memory — so it could
//! never have served as a real allocator. None of its public items were imported,
//! path-qualified, or referenced by any caller anywhere in the workspace (the only
//! reference was the `pub mod gc_integration;` declaration itself).
//!
//! Because it was both unused and actively misleading (a plausible-looking but
//! fake GC that a future reader could mistake for the real one), the entire model
//! and its fake allocator have been removed. The real garbage collector lives in
//! the `gc` crate (e.g. `gc::tlab`, `gc::shadow_stack`); the interpreter wires into
//! it via `vm::runtime::alloc_fastpath` and `vm::runtime::jit_integration`.
//!
//! This file is intentionally left empty (aside from this notice) so the existing
//! `pub mod gc_integration;` declaration in `vm/src/runtime/mod.rs` continues to
//! resolve. The module declaration may be dropped separately.
