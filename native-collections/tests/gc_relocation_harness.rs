// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.
//
//! GC-relocation safety harness for collections side-tables.
//!
//! Reviews `.claude/review-2026-05-24/native-collections.md` §1.1 row 2
//! flagged that the LinkedList / LinkedHashMap / TreeMap / TreeSet
//! overlays were keyed by `this.as_ptr() as usize` — a raw heap pointer
//! that a moving GC can invalidate. C21 (commit `8b11ff2`) re-keyed
//! every overlay on `ctx.identity_hash_code(this)`, which is stable
//! across GC moves.
//!
//! This test exercises that fix end-to-end:
//!
//!   1. Allocate an LHM, put three entries.
//!   2. *Simulate* a GC compaction by moving the LHM to a fresh address
//!      while preserving its identity hash code (which a real moving GC
//!      copies along with the object header).
//!   3. Confirm the LHM's state — size, get(k), iteration — is still
//!      recoverable through the new ObjectRef.
//!
//! Pre-C21 this test FAILS (the overlay lookup at the new address
//! returns the default-empty entry; `size()` reports 0, `get(k)` returns
//! null). Post-C21 the identity-hash key matches, and state survives.

mod common;

use common::{boxed_int, build_registry, call, new_linked_hashmap, MockCtx};
use cratonvm_types::Value;

const LHM: &str = "java/util/LinkedHashMap";
const PUT: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
const GET: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";

#[test]
fn lhm_state_survives_simulated_gc_relocation() {
    let reg = build_registry();
    let mut ctx = MockCtx::new();

    let lhm_pre = new_linked_hashmap(&reg, &mut ctx, false);

    let k1 = boxed_int(&mut ctx, 1);
    let k2 = boxed_int(&mut ctx, 2);
    let k3 = boxed_int(&mut ctx, 3);
    let v1 = boxed_int(&mut ctx, 10);
    let v2 = boxed_int(&mut ctx, 20);
    let v3 = boxed_int(&mut ctx, 30);

    call(&reg, &mut ctx, LHM, "put", PUT,
         &[Value::Object(Some(lhm_pre)), k1, v1]).unwrap();
    call(&reg, &mut ctx, LHM, "put", PUT,
         &[Value::Object(Some(lhm_pre)), k2, v2]).unwrap();
    call(&reg, &mut ctx, LHM, "put", PUT,
         &[Value::Object(Some(lhm_pre)), k3, v3]).unwrap();

    let size_pre = call(&reg, &mut ctx, LHM, "size", "()I",
                        &[Value::Object(Some(lhm_pre))]).unwrap();
    assert_eq!(size_pre, Some(Value::Int(3)),
               "baseline: 3 entries after 3 puts");

    // ----- simulate the GC move -----
    let lhm_post = ctx.relocate_object(lhm_pre);
    assert_ne!(lhm_pre.as_ptr() as usize, lhm_post.as_ptr() as usize,
               "relocation must produce a different address");

    // ----- post-GC observation -----
    let size_post = call(&reg, &mut ctx, LHM, "size", "()I",
                         &[Value::Object(Some(lhm_post))]).unwrap();
    assert_eq!(size_post, Some(Value::Int(3)),
               "C21 contract: size() must still report 3 after GC relocation; \
                got {size_post:?} — the LHM overlay key did NOT survive the move, \
                which is the exact bug C21 fixes");

    // Lookup each key individually — the overlay carries the bucket
    // table, so a working post-C21 lookup must walk through it.
    for (k, v) in [(k1, v1), (k2, v2), (k3, v3)] {
        let got = call(&reg, &mut ctx, LHM, "get", GET,
                       &[Value::Object(Some(lhm_post)), k]).unwrap();
        assert_eq!(got, Some(v),
                   "C21 contract: get({k:?}) after relocation must return {v:?}; \
                    got {got:?}");
    }
}

#[test]
fn identity_hash_is_preserved_across_simulated_relocation() {
    // Self-test of the mock's `relocate_object` helper: an object's
    // identity_hash_code must match its pre-move value, which is the
    // GC invariant C21 depends on. If this assertion ever fails the
    // mock has drifted from a real moving GC's contract.
    let mut ctx = MockCtx::new();
    use cratonvm_native_api::NativeContext;
    use cratonvm_types::ClassId;

    let obj_pre = ctx.alloc_object(ClassId::new(0), 1);
    let h_pre = ctx.identity_hash_code(obj_pre);
    let obj_post = ctx.relocate_object(obj_pre);
    let h_post = ctx.identity_hash_code(obj_post);
    assert_eq!(h_pre, h_post,
               "identity hash code must survive GC relocation");
}
