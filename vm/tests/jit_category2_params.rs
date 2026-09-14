// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT early-compile category-2 (long/double) PARAMETER regression.
//!
//! The eager early-compile path in `interpreter::execute` passed
//! `param_slots = args.len()` to `x64::compile`, which lays parameters out
//! sequentially — one JVM local slot per argument. A `long`/`double`
//! parameter occupies TWO JVM slots, so every parameter after the first
//! category-2 one was read from the wrong (un-populated) slot:
//!
//!   * `secondLong(100L, 23L)` returned 0   (read `locals[2]`, never written)
//!   * `addLongs(100L, 23L)`   returned 100  (`a + 0`)
//!   * `addDoubles(1.5, 2.25)` returned 1.5
//!
//! It surfaced via `Long::sum` method references and any `(long,long)->long`
//! /`(double,double)->double` lambda (the synthetic SAM body is just such a
//! method), and broke `Map.merge(k, v, Long::sum)`.
//!
//! Fix: the early-compile path bails when the descriptor has category-2
//! parameters (`count_param_slots_jvm_spec != count_param_slots`); such
//! methods run interpreted and JIT later through the hot-path
//! `try_compile`, which uses `compute_param_jvm_slots` and is correct.
//!
//! Fixture: `vm/tests/resources/cratonvm/JitCategory2.java`.

#![allow(clippy::unwrap_used)]

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn class_files_available() -> bool {
    let path = format!("{}/cratonvm/JitCategory2.class", test_resources_dir());
    std::path::Path::new(&path).exists()
}

macro_rules! require_class_files {
    () => {
        if !class_files_available() {
            eprintln!("Skipping: JitCategory2.class not available (javac not on PATH?)");
            return;
        }
    };
}

fn test_vm() -> Vm {
    let cfg = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    Vm::new(cfg)
}

fn call_long(method: &str) -> i64 {
    let mut vm = test_vm();
    match vm.invoke("cratonvm/JitCategory2", method, "()J", &[]) {
        Ok(Some(Value::Long(v))) => v,
        other => panic!("{method}() expected Ok(Some(Long)), got: {other:?}"),
    }
}

#[test]
fn cold_long_params_not_dropped() {
    require_class_files!();
    // a + b — the 2nd long must not be read as 0.
    assert_eq!(
        call_long("addOnce"),
        123,
        "addLongs(100,23): 2nd long dropped"
    );
    // returns the 2nd param directly — the canonical "2nd cat-2 arg lost" probe.
    assert_eq!(
        call_long("secondOnce"),
        23,
        "secondLong(100,23): 2nd long read as 0"
    );
    // 3rd of three longs — slot index 4, must survive.
    assert_eq!(
        call_long("thirdOnce"),
        3,
        "threeLongs(1,2,3): 3rd long dropped"
    );
}

#[test]
fn hot_long_params_not_dropped() {
    require_class_files!();
    // 500 * 123 — any per-call dropped arg shifts the aggregate.
    assert_eq!(
        call_long("driveAdd"),
        61_500,
        "driveAdd aggregate wrong (long arg dropped)"
    );
    // 500 * 23.
    assert_eq!(
        call_long("driveSecond"),
        11_500,
        "driveSecond aggregate wrong (2nd long read as 0)"
    );
}

#[test]
fn double_params_not_dropped() {
    require_class_files!();
    // 500 * 3.75 = 1875.0
    let bits = call_long("driveDoubleBits");
    assert_eq!(
        f64::from_bits(bits as u64),
        1875.0,
        "driveDoubleBits: 2nd double param dropped (got {})",
        f64::from_bits(bits as u64),
    );
}
