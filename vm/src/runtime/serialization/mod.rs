// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Java serialization runtime support.
//!
//! This module provides the VM-side infrastructure backing
//! `java.io.ObjectStreamClass`. The native-method bridge that populates
//! the ObjectStreamClass instance lives in
//! `native-builtins::serialization::register_object_stream_class`; the
//! *cache* of `ObjectStreamClass` mirror objects keyed by ClassId lives
//! here, where it can be built once per class and reused across every
//! subsequent `ObjectStreamClass.lookup(cls)` call (JLS matches
//! real-HotSpot semantics: the descriptor is effectively a singleton
//! per class).
//!
//! Keeping the cache in the `vm` crate (rather than in
//! `native-builtins`) has two benefits:
//!
//! 1. The cache survives a cold-boot of the native registry. A VM
//!    restart of the registry (as occurs in some per-test harnesses)
//!    does *not* evict already-built descriptors.
//! 2. Other VM subsystems (e.g. RMI marshalling when that lands) can
//!    consult the cache without going through the native-bridge trait.
//!
//! The cache is sharded behind a single `RwLock` — writes are rare (one
//! per new Serializable class ever seen) and reads are fast (two-hop
//! `FxHashMap` lookup).

pub mod oscache;

pub use oscache::{OscCache, OscCacheHandle};
