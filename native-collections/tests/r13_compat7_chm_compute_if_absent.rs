// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company.
//
//! Round 13 wave 13 (lane compat7): `ConcurrentHashMap.computeIfAbsent` walks
//! a colliding bin as the JDK body does -- the lock-free first-node test, then
//! one walk -- instead of the reservation protocol's five walks
//! (`CRATONVM_COMPAT_CHM_CIA_SINGLE_WALK`, proposal C6-2; page
//! `r13w10-compat5-map-view-residuals-CLOSED-20260929.md`, "Round 13 wave 13").
//!
//! The keys are a user class, so each `hashCode()` / `equals()` is a logged
//! `invoke_virtual`; the mock answers from a script consumed one call at a
//! time (`Ok(None)` once it is empty). A key compared with ITSELF never calls
//! `equals` (`==` first, as the JDK), so every lookup here uses a distinct key
//! object and the script decides equality.

mod common;

#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use common::{build_registry, call, new_concurrent_hashmap, MockCtx};
use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::{ObjectRef, Value};

const CHM: &str = "java/util/concurrent/ConcurrentHashMap";
const PUT: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
const COMPUTE_IF_ABSENT: &str =
    "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;";

fn user_object(ctx: &mut MockCtx, class: &str) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(class).unwrap();
    ctx.alloc_object(cid, 1)
}

fn obj(o: ObjectRef) -> Value {
    Value::Object(Some(o))
}

fn calls_of(ctx: &MockCtx, method: &str) -> usize {
    ctx.invoke_virtual_log()
        .iter()
        .filter(|(_, m, _, _)| m == method)
        .count()
}

fn fresh_ctx(vm: usize) -> MockCtx {
    let mut ctx = MockCtx::new();
    ctx.set_vm_identity(vm);
    ctx
}

const FALSE: i32 = 0;
const TRUE: i32 = 1;

/// A `ConcurrentHashMap` holding three user keys whose `hashCode()` is 5 (one
/// bin, in this order), each mapped to a fresh value.
fn colliding_chm(reg: &NativeMethodRegistry, ctx: &mut MockCtx) -> (ObjectRef, Vec<Value>) {
    let chm = new_concurrent_hashmap(reg, ctx);
    let mut values = Vec::new();
    for i in 0..3 {
        let k = user_object(ctx, "test/R13Compat7Key");
        let v = obj(user_object(ctx, "test/R13Compat7Value"));
        // hashCode(k), then `k.equals(..)` (false) for each key already in.
        let mut script = vec![Ok(Some(Value::Int(5)))];
        script.extend((0..i).map(|_| Ok(Some(Value::Int(FALSE)))));
        ctx.set_invoke_virtual_results(script);
        call(reg, ctx, CHM, "put", PUT, &[obj(chm), obj(k), v]).unwrap();
        values.push(v);
    }
    ctx.set_invoke_virtual_results(Vec::new());
    (chm, values)
}

fn chm_size(reg: &NativeMethodRegistry, ctx: &mut MockCtx, chm: ObjectRef) -> Option<Value> {
    call(reg, ctx, CHM, "size", "()I", &[obj(chm)]).unwrap()
}

#[test]
fn r13_compat7_chm_compute_if_absent_of_an_absent_key_walks_the_bin_once() {
    let reg = build_registry();
    let mut ctx = fresh_ctx(0x6c07_0001);
    let (chm, _) = colliding_chm(&reg, &mut ctx);
    let k = user_object(&mut ctx, "test/R13Compat7Key");
    let f = user_object(&mut ctx, "test/R13Compat7Function");
    let v = obj(user_object(&mut ctx, "test/R13Compat7Value"));
    ctx.clear_invoke_virtual_log();
    // hashCode(k); the first-node test `k.equals(k0)`; the walk `k.equals(k0)`,
    // `k.equals(k1)`, `k.equals(k2)` (all false); then `f.apply(k)` -> v.
    let mut script = vec![Ok(Some(Value::Int(5)))];
    script.extend((0..4).map(|_| Ok(Some(Value::Int(FALSE)))));
    script.push(Ok(Some(v)));
    ctx.set_invoke_virtual_results(script);
    let got = call(
        &reg,
        &mut ctx,
        CHM,
        "computeIfAbsent",
        COMPUTE_IF_ABSENT,
        &[obj(chm), obj(k), obj(f)],
    )
    .unwrap();
    assert_eq!(got, Some(v), "the mapper's value is the answer");
    assert_eq!(calls_of(&ctx, "hashCode"), 1);
    assert_eq!(
        calls_of(&ctx, "equals"),
        4,
        "HotSpot: the first-node test plus one walk of three nodes; the reservation \
         protocol walked the bin five times"
    );
    assert_eq!(calls_of(&ctx, "apply"), 1);
    assert_eq!(chm_size(&reg, &mut ctx, chm), Some(Value::Int(4)));
}

#[test]
fn r13_compat7_chm_compute_if_absent_of_a_present_key_repeats_the_first_node_test() {
    let reg = build_registry();
    let mut ctx = fresh_ctx(0x6c07_0002);
    let (chm, values) = colliding_chm(&reg, &mut ctx);
    // A key object equal to the SECOND key (the script says so).
    let k = user_object(&mut ctx, "test/R13Compat7Key");
    let f = user_object(&mut ctx, "test/R13Compat7Function");
    ctx.clear_invoke_virtual_log();
    // hashCode(k); the first-node test (false); the walk from the first node:
    // `k.equals(k0)` false, `k.equals(k1)` true.
    ctx.set_invoke_virtual_results(vec![
        Ok(Some(Value::Int(5))),
        Ok(Some(Value::Int(FALSE))),
        Ok(Some(Value::Int(FALSE))),
        Ok(Some(Value::Int(TRUE))),
    ]);
    let got = call(
        &reg,
        &mut ctx,
        CHM,
        "computeIfAbsent",
        COMPUTE_IF_ABSENT,
        &[obj(chm), obj(k), obj(f)],
    )
    .unwrap();
    assert_eq!(got, Some(values[1]), "the present key's value, the mapper not run");
    assert_eq!(
        calls_of(&ctx, "equals"),
        3,
        "HotSpot compares the first node twice (lock-free, then in the walk)"
    );
    assert_eq!(calls_of(&ctx, "apply"), 0);
    assert_eq!(chm_size(&reg, &mut ctx, chm), Some(Value::Int(3)));
}
