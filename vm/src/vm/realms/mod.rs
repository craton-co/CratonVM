// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `SharedVm` realms — cohesive sub-structs of the former god object.
//!
//! `SharedVm` used to declare 86 fields, roughly half of them independently
//! locked, in one flat struct. Every subsystem reached into every other one
//! through it, which made the global lock hierarchy
//! ([`crate::runtime::lock_order`]) hard to reason about and the crate hard
//! to navigate.
//!
//! The state is now grouped into *realms*: plain sub-structs owned by
//! `SharedVm`, one per subsystem. `SharedVm` itself is down to 16 fields —
//! six realms plus VM identity, config, the `System.in/out/err` handles,
//! system properties, the self-`Weak`, the bootstrap init-level state
//! machine, and the optional GPU offload registry.
//!
//! Nothing else changed. Field types, lock types (`OrderedPlRwLock`,
//! `OrderedPlMutex`, `parking_lot::*`, `std::sync::*`), lock levels, the
//! initialisation order inside `SharedVm::new`, and every acquisition site
//! are identical modulo the extra path segment. Access is
//! `shared.<realm>.<field>`.
//!
//! # Realms and the lock hierarchy
//!
//! The realms are declared on `SharedVm` in **descending lock-level order**,
//! so the L10..L0 hierarchy of [`crate::runtime::lock_order`] is visible in
//! the struct layout rather than buried in an 800-line field list:
//!
//! | Realm           | Accessor  | Fields | Lock levels owned |
//! |-----------------|-----------|-------:|-------------------|
//! | [`ClassRealm`]  | `classes` |     23 | **L10** — `class_manager` is the `OrderedPlRwLock` at [`LockLevel::ClassManager`] |
//! | [`NativeRealm`] | `natives` |      6 | **L9** (`native_methods` — reserved; a bare registry, immutable after `SharedVm::new`, so no lock instance), **L2** (`native_memory`) |
//! | [`HeapRealm`]   | `mem`     |     19 | **L8** (`heap` interior locks, defined in `cratonvm-gc`, not yet wrapped), **L7** (`ref_processor`, `OrderedPlMutex`), **L3** (`cleaner_thread.pending_actions`) |
//! | [`ThreadRealm`] | `threads` |      7 | **L6** (`monitors`), **L5** (`thread_registry`) |
//! | [`DebugRealm`]  | `debug`   |     12 | **L4** (`flight_recorder`) |
//! | [`JitRealm`]    | `jit`     |      9 | — compile-time state; holds no level in the hierarchy |
//!
//! [`LockLevel::ClassManager`]: crate::runtime::lock_order::LockLevel::ClassManager
//!
//! ## How structural is this?
//!
//! Honestly: *descriptive, not enforcing*. Splitting the struct does not by
//! itself make an inversion unrepresentable — the levels are still asserted
//! at runtime by the ordered-lock wrappers in
//! [`crate::runtime::lock_order`], and that remains the mechanism that
//! catches a violation.
//!
//! What the split does buy:
//!
//! * "which realm owns which level" is a one-line answer, and the answer is
//!   checked against the declaration order of `SharedVm` every time someone
//!   reads it;
//! * a *cross-realm* acquisition is now syntactically visible at the call
//!   site — `shared.classes.class_manager_write()` inside a
//!   `shared.threads.monitors` critical section reads as two different
//!   subsystems, where previously both were indistinguishable
//!   `shared.<field>` accesses;
//! * the realms are the natural unit at which to add level-typed wrappers
//!   later: each realm owns a contiguous band of the hierarchy, so a future
//!   `impl ClassRealm { fn with_classes<R>(…) }`-style API could enforce
//!   entry order per realm instead of per field.
//!
//! Making the hierarchy *statically* unrepresentable-if-violated would need
//! level-indexed capability tokens threaded through every acquisition, which
//! is a much larger change than this restructuring and is deliberately not
//! attempted here.

pub mod class_realm;
pub mod debug_realm;
pub mod heap_realm;
pub mod jit_realm;
pub mod native_realm;
pub mod thread_realm;

pub use class_realm::ClassRealm;
pub use debug_realm::DebugRealm;
pub use heap_realm::HeapRealm;
pub use jit_realm::JitRealm;
pub use native_realm::NativeRealm;
pub use thread_realm::ThreadRealm;
