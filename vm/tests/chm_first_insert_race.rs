// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Concurrent FIRST inserts into one fresh `ConcurrentHashMap` must not lose a
//! key.
//!
//! `new ConcurrentHashMap<>()` defers its native storage to the first insert,
//! and `chm_segment_for_mut` installed it with a plain store. Two threads
//! inserting first each built storage and the later store won; the other
//! thread's key, already reserved in the losing storage, vanished, and
//! `computeIfAbsent` returned null. MEASURED 2026-09-12 with this fixture on
//! the release binary: 1070 of 2000 rounds bad for `computeIfAbsent`, 808 of
//! 2000 for `put`; HotSpot 0 and 0. It surfaced as Tomcat's
//! `TestRateLimitFilter` `expected:<200> but was:<0>`, a client thread killed
//! by an NPE in `TimeBucketCounterBase.increment` on its first request.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const CLASS: &str = "cratonvm/ChmFirstInsertRace";

fn test_resources_dir() -> String {
    if let Some(generated) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        if std::path::Path::new(generated).is_dir() {
            return generated.to_owned();
        }
    }
    format!("{}/tests/resources", env!("CARGO_MANIFEST_DIR"))
}

/// The fixture starts threads and waits on a `CountDownLatch`, which needs a
/// class library: `VmConfig::default()` is synthetic-JDK mode, and without the
/// `synthetic-jdk` Cargo feature that mode has none, so `Thread.<init>` throws
/// before the race is ever reached. Same gate as `interpreter_tests.rs`. Run:
///
/// ```text
/// cargo test --release -p cratonvm-vm --features synthetic-jdk --test chm_first_insert_race
/// ```
fn library_available(test: &str) -> bool {
    if !cratonvm_vm::config::SYNTHETIC_JDK_COMPILED_IN {
        eprintln!(
            "Skipping {test}: the fixture needs java.lang.Thread; rebuild with \
             `--features synthetic-jdk`"
        );
        return false;
    }
    true
}

fn bad_rounds(method: &str, rounds: i32) -> i32 {
    let mut vm = Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]));
    match vm.invoke(CLASS, method, "(II)I", &[Value::Int(rounds), Value::Int(4)]) {
        Ok(Some(Value::Int(n))) => n,
        other => panic!("{CLASS}.{method} did not return an int: {other:?}"),
    }
}

#[test]
fn concurrent_first_compute_if_absent_never_returns_null_or_loses_a_key() {
    if !library_available("concurrent_first_compute_if_absent_never_returns_null_or_loses_a_key") {
        return;
    }
    const ROUNDS: i32 = 300;
    let bad = bad_rounds("badRounds", ROUNDS);
    assert_eq!(
        bad, 0,
        "{bad} of {ROUNDS} rounds of four concurrent first computeIfAbsent calls on a fresh \
         ConcurrentHashMap returned null or lost a key -- the lazy segment install is racing again"
    );
}

#[test]
fn concurrent_first_put_never_loses_an_entry() {
    if !library_available("concurrent_first_put_never_loses_an_entry") {
        return;
    }
    const ROUNDS: i32 = 300;
    let bad = bad_rounds("putBadRounds", ROUNDS);
    assert_eq!(
        bad, 0,
        "{bad} of {ROUNDS} rounds of four concurrent first puts on a fresh ConcurrentHashMap lost \
         an entry -- the lazy segment install is racing again"
    );
}
