// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `SharedVm` realms — cohesive sub-structs of the former god object.
//!
//! `SharedVm` used to declare 86 fields, roughly half of them independently
//! locked, in a single flat struct. Every subsystem reached into every other
//! one through it, which made the global lock hierarchy
//! ([`crate::runtime::lock_order`]) hard to reason about and the crate hard
//! to navigate.
//!
//! The state is now grouped into *realms*: plain sub-structs owned by
//! `SharedVm`, one per subsystem. Nothing else changed — field types, lock
//! types (`OrderedPlRwLock`, `OrderedPlMutex`, `parking_lot::*`,
//! `std::sync::*`), lock levels, the initialisation order inside
//! `SharedVm::new`, and every acquisition site are identical modulo the
//! extra path segment. Access is `shared.<realm>.<field>`.
//!
//! # Realms and the lock hierarchy
//!
//! The realms are declared on `SharedVm` in **descending lock-level order**,
//! so the L10..L0 hierarchy is visible in the struct layout:
//!
//! | Realm           | Accessor  | Lock levels owned | Notes |
//! |-----------------|-----------|-------------------|-------|
//! | [`ClassRealm`]  | `classes` | L10               | `class_manager` is the `OrderedPlRwLock` at [`LockLevel::ClassManager`] |
//! | [`NativeRealm`] | `natives` | L9, L2            | `native_methods` (L9; a bare registry, immutable after `SharedVm::new`, so no lock instance), `native_memory` (L2) |
//! | [`HeapRealm`]   | `gc`      | L8, L7, L3        | heap interior locks (L8, defined in `cratonvm-gc`), `ref_processor` (L7 `OrderedPlMutex`), `cleaner_thread.pending_actions` (L3) |
//! | [`ThreadRealm`] | `threads` | L6, L5            | `monitors` (L6), `thread_registry` (L5) |
//! | [`JitRealm`]    | `jit`     | —                 | compile-time state; holds no level in the hierarchy |
//! | [`DebugRealm`]  | `debug`   | L4                | `flight_recorder` (L4) |
//!
//! [`LockLevel::ClassManager`]: crate::runtime::lock_order::LockLevel::ClassManager
//!
//! The grouping is *descriptive*, not enforcing. The levels are still
//! asserted at runtime by the ordered-lock wrappers in
//! [`crate::runtime::lock_order`], and splitting the struct does not by
//! itself prevent an inversion. What it does buy is that "which realm owns
//! which level" is now a one-line answer instead of an 800-line struct read,
//! and that a cross-realm acquisition is syntactically visible at the call
//! site (`shared.classes.…` under `shared.threads.…`) rather than hidden
//! behind two same-looking `shared.<field>` accesses.

pub mod class_realm;
pub mod debug_realm;
pub mod jit_realm;
pub mod thread_realm;

pub use class_realm::ClassRealm;
pub use debug_realm::DebugRealm;
pub use jit_realm::JitRealm;
pub use thread_realm::ThreadRealm;
