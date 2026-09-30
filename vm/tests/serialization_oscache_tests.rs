// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP0.2 — integration tests for `ObjectStreamClass.lookup` / cache.
//!
//! Exercises the VM-side `OscCache` directly (pure-Rust assertions on
//! cache identity and lifecycle). The heavier cross-crate acceptance
//! tests — "look up ArrayList.class and see a non-null
//! `serializableConstructor`", "decl-order fields", etc. — would
//! require bootstrapping a real-JDK-mode VM with a classpath, which
//! the other WP0.2 deliverables (apps/junit4 + apps/serializable_smoke)
//! cover end-to-end. Here we focus on the cache invariants the rest of
//! the work depends on: identity, single-build, concurrency.
//!
//! Test checklist — WP0.2 acceptance:
//!   1. Two consecutive `get_or_insert_with` calls on the same
//!      ClassId return the same ObjectRef (identity contract).
//!   2. Distinct ClassIds produce distinct descriptors.
//!   3. Concurrent inserts on the same ClassId resolve to the same
//!      winning entry.
//!   4. `clear()` drops every entry.
//!   5. `SharedVm` exposes the cache as `osc_cache` — smoke-test the
//!      field accessor path that the `NativeContextImpl` will use.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::thread;

use cratonvm_vm::classloading::ClassId;
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::runtime::serialization::OscCache;
use cratonvm_vm::types::ObjectRef;
use cratonvm_vm::vm::SharedVm;

// ---------------------------------------------------------------------------
// Fake ObjectRef helper
// ---------------------------------------------------------------------------
//
// The cache stores and returns `ObjectRef` by copy; it never
// dereferences. So we can fabricate one from an integer for unit-test
// purposes, provided we maintain the `from_raw` invariants (non-null,
// 8-byte aligned).

fn fake_ref(n: usize) -> ObjectRef {
    // SAFETY: we never dereference this pointer; the cache stores and
    // returns it by value only.
    unsafe { ObjectRef::from_raw((n * 8 + 8) as *mut u8) }
}

// ---------------------------------------------------------------------------
// 1. Identity: two lookups return the same instance
// ---------------------------------------------------------------------------

#[test]
fn oscache_lookup_is_identity_stable() {
    let cache = OscCache::new();
    let cid = ClassId::new(42);
    let a = cache.get_or_insert_with(cid, || fake_ref(1));
    let b = cache.get_or_insert_with(cid, || panic!("builder should not run on second lookup"));
    assert_eq!(a, b);
    assert_eq!(cache.len(), 1);
}

#[test]
fn oscache_get_after_insert_returns_same_ref() {
    let cache = OscCache::new();
    let cid = ClassId::new(7);
    let r = fake_ref(99);
    cache.insert_if_absent(cid, r);
    assert_eq!(cache.get(cid), Some(r));
}

// ---------------------------------------------------------------------------
// 2. Distinct classes: independent entries
// ---------------------------------------------------------------------------

#[test]
fn oscache_distinct_classes_have_distinct_entries() {
    let cache = OscCache::new();
    cache.insert_if_absent(ClassId::new(1), fake_ref(1));
    cache.insert_if_absent(ClassId::new(2), fake_ref(2));
    assert_eq!(cache.len(), 2);
    assert_ne!(
        cache.get(ClassId::new(1)),
        cache.get(ClassId::new(2)),
        "each class must have its own descriptor"
    );
}

// ---------------------------------------------------------------------------
// 3. Concurrency: two threads race, both see the winner
// ---------------------------------------------------------------------------

#[test]
fn oscache_concurrent_inserts_agree_on_winner() {
    let cache = Arc::new(OscCache::new());
    let cid = ClassId::new(123);

    // Kick off four threads, each racing to install its own desc for
    // the same class. After all join, every one must have observed
    // the same winning ref.
    let mut handles = Vec::new();
    for i in 0..4 {
        let c = Arc::clone(&cache);
        handles.push(thread::spawn(move || {
            c.get_or_insert_with(cid, || fake_ref(i + 1))
        }));
    }
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let winner = results[0];
    for r in &results[1..] {
        assert_eq!(
            *r, winner,
            "concurrent inserts must converge on the same winner"
        );
    }
    assert_eq!(cache.len(), 1);
}

// ---------------------------------------------------------------------------
// 4. clear()
// ---------------------------------------------------------------------------

#[test]
fn oscache_clear_drops_everything() {
    let cache = OscCache::new();
    cache.insert_if_absent(ClassId::new(1), fake_ref(1));
    cache.insert_if_absent(ClassId::new(2), fake_ref(2));
    assert_eq!(cache.len(), 2);
    cache.clear();
    assert!(cache.is_empty());
    assert!(cache.get(ClassId::new(1)).is_none());
    assert!(cache.get(ClassId::new(2)).is_none());
}

// ---------------------------------------------------------------------------
// 5. SharedVm wiring — the cache is reachable via the canonical field
// ---------------------------------------------------------------------------

#[test]
fn sharedvm_exposes_oscache_field() {
    // Smoke: constructing a SharedVm with the default synthetic-jdk
    // config must initialize `osc_cache` to an empty cache.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    assert!(shared.classes.osc_cache.is_empty());
    assert_eq!(shared.classes.osc_cache.len(), 0);

    // And it must behave as a regular OscCache: insert + get roundtrip.
    let cid = ClassId::new(555);
    let r = fake_ref(7);
    let installed = shared.classes.osc_cache.insert_if_absent(cid, r);
    assert_eq!(installed, r);
    assert_eq!(shared.classes.osc_cache.get(cid), Some(r));
    assert_eq!(shared.classes.osc_cache.len(), 1);
}

#[test]
fn sharedvm_oscache_is_independent_per_vm() {
    // Two VMs each keep their own cache.
    let v1 = Arc::new(SharedVm::new(VmConfig::default()));
    let v2 = Arc::new(SharedVm::new(VmConfig::default()));
    v1.classes
        .osc_cache
        .insert_if_absent(ClassId::new(1), fake_ref(1));
    assert_eq!(v1.classes.osc_cache.len(), 1);
    assert_eq!(v2.classes.osc_cache.len(), 0);
}
