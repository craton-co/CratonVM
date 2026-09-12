// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `Stream.distinct()` and the `Set.of`/`Map.ofEntries` duplicate check must be
//! hash-bucketed, not all-pairs `equals` scans.
//!
//! Both sit on the first `Calendar.getInstance` per locale:
//! `CLDRLocaleProviderAdapter.createLanguageTagSet` builds `Set.of(<1152 tags>)`
//! and `isSupportedLocale` then runs a `distinct()` over the same tags.
//!
//! The native that shadows `java.util.stream.Stream.distinct` kept a `Vec` of
//! survivors and asked each new element's candidates `equals` one by one, so
//! `n` distinct elements cost `n(n-1)/2` Java calls. Nobody sees that on a
//! ten-element stream. `CLDRCalendarDataProviderImpl` inherits
//! `LocaleServiceProvider.isSupportedLocale`, which walks
//! `LocaleProviderAdapter.toLocaleArray` — a `distinct()` over the 1152 CLDR
//! language tags — so the FIRST `Calendar.getInstance` per locale took 1.5 s
//! (HotSpot: 11 ms), and Tomcat's `TestAccessLogValve` gave up waiting 1000 ms
//! for a `%{begin:...SSS}t` log line that was still being formatted.
//!
//! The call count is the assertion, not a wall clock: an `equals` tally cannot
//! be made green or red by host load.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const CLASS: &str = "cratonvm/DistinctEqualsCount";

fn test_resources_dir() -> String {
    if let Some(generated) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        if std::path::Path::new(generated).is_dir() {
            return generated.to_owned();
        }
    }
    format!("{}/tests/resources", env!("CARGO_MANIFEST_DIR"))
}

fn int_of(vm: &mut Vm, method: &str, desc: &str, args: &[Value]) -> i32 {
    match vm.invoke(CLASS, method, desc, args) {
        Ok(Some(Value::Int(n))) => n,
        other => panic!("{CLASS}.{method}{desc} did not return an int: {other:?}"),
    }
}

#[test]
fn distinct_over_well_spread_hashes_does_not_compare_every_pair() {
    let mut vm = Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]));
    const N: i32 = 2000;
    let calls = int_of(&mut vm, "spreadEqualsCalls", "(I)I", &[Value::Int(N)]);
    assert!(
        calls >= 0,
        "distinct() kept the wrong number of {N} pairwise-unequal keys"
    );
    // All-pairs would be N(N-1)/2 = 1 999 000. A hash-bucketed dedup only asks
    // `equals` of same-hash candidates, and these hashes are distinct.
    assert!(
        calls < N,
        "distinct() made {calls} equals() calls over {N} keys with distinct hashes — it is \
         comparing across hash buckets again, which is the quadratic scan that made the first \
         Calendar.getInstance per locale cost 1.5 s"
    );
}

#[test]
fn distinct_keeps_first_occurrences_in_encounter_order_with_nulls() {
    let mut vm = Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]));
    assert_eq!(int_of(&mut vm, "firstOccurrenceOrder", "()I", &[]), 1203);
}

#[test]
fn distinct_follows_hashset_when_equals_and_hash_code_disagree() {
    let mut vm = Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]));
    assert_eq!(int_of(&mut vm, "equalButDifferentHash", "()I", &[]), 2);
}

#[test]
fn distinct_is_correct_when_every_hash_collides() {
    let mut vm = Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]));
    assert_eq!(
        int_of(
            &mut vm,
            "collidingHashesKept",
            "(II)I",
            &[Value::Int(300), Value::Int(17)]
        ),
        17
    );
}

/// Same shape, one layer down: `Set.of(E...)`'s duplicate check compared every
/// pair, and `createLanguageTagSet` passes it the same 1152 tags.
#[test]
fn set_of_array_does_not_compare_every_pair() {
    let mut vm = Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]));
    const N: i32 = 2000;
    let calls = int_of(&mut vm, "setOfEqualsCalls", "(I)I", &[Value::Int(N)]);
    assert!(calls >= 0, "Set.of kept the wrong number of {N} pairwise-unequal keys");
    assert!(
        calls < N,
        "Set.of made {calls} equals() calls over {N} keys with distinct hashes — its duplicate \
         check is comparing across hash buckets again"
    );
}

#[test]
fn set_of_still_rejects_a_duplicate() {
    let mut vm = Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]));
    assert_eq!(int_of(&mut vm, "setOfRejectsSameHashDuplicate", "()I", &[]), 1);
}

#[test]
fn distinct_dedups_strings_by_value() {
    let mut vm = Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]));
    assert_eq!(int_of(&mut vm, "stringsByValue", "()I", &[]), 2);
}
