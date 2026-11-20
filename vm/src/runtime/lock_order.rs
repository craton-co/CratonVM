// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Global lock acquisition order (L0-L10) — re-export.
//!
//! The definitions live in [`cratonvm_types::lock_order`], one crate below
//! both `vm` and `cratonvm-gc`. They used to live here, which meant
//! `VmHeap`'s interior locks — L8 in the hierarchy, defined in the separate
//! `cratonvm-gc` crate — had no way to name [`LockLevel`] without inverting
//! the `vm -> gc` dependency edge. Moving the core down to `cratonvm-types`
//! removes that obstacle: `gc` can now announce L8 with [`enter_level`], or
//! wrap its interior locks in [`OrderedPlMutex`] / [`OrderedPlRwLock`]
//! directly.
//!
//! Nothing else changed. `crate::runtime::lock_order::X` still resolves for
//! every `X` it resolved for before, the per-thread tracker is still a
//! single `thread_local!` (it moved, it was not duplicated), and the
//! hierarchy table, the enforcement rules and the
//! `CRATONVM_LOCK_ORDER_CHECK` opt-in are unchanged — see
//! [`cratonvm_types::lock_order`] for all of it.
//!
//! Current wiring in `SharedVm` (see `crate::vm::realms`):
//!
//! - `shared.classes.class_manager` — [`OrderedPlRwLock`] at
//!   [`LockLevel::ClassManager`] (L10).
//! - `shared.mem.ref_processor` — [`OrderedPlMutex`] at
//!   [`LockLevel::RefProcessor`] (L7).
//! - `shared.natives.native_methods` — L9 is reserved; the registry is
//!   immutable after `SharedVm::new` and holds no lock.
//! - `shared.mem.heap` — L8, now *unblocked* but not yet wrapped: doing so
//!   would change locking behaviour and is left to a follow-up.

pub use cratonvm_types::lock_order::*;

#[cfg(test)]
mod tests;
