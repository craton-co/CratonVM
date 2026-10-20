// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The OFF arm of `CRATONVM_JIT_CHA`, in its own process.
//!
//! `cha_unique_implementor_binds.rs` asserts that class-hierarchy analysis
//! binds an unprofiled `invokeinterface` site. On its own that is one outcome
//! from one binary, and an outcome alone does not say the switch produced it.
//! This file runs the SAME fixture under the SAME overrides with only
//! `CRATONVM_JIT_CHA` flipped, and asserts CHA planned nothing — so the pair is
//! a real A/B: same fixture, one switch, two outcomes.
//!
//! It is also what keeps the kill switch honest. `NOTES-cha.md` records that
//! the flag's only tests were "the default and the two off-spellings", i.e.
//! that it parses. A default-ON knob whose OFF arm no test drives through a
//! compile is a knob that can quietly stop working, and the next person to
//! reach for it in a bisect finds out the hard way.
//!
//! # Why a separate test binary
//!
//! `env_cache::jit_cha` is a `MemoSlot`, latched on first read. Two arms in one
//! process would both see whichever value the first latched. See the ON arm.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const CLASS: &str = "cratonvm/ChaUniqueImplementor";
const METHOD: &str = "run";
const DESC: &str = "(I)I";

/// The ON arm's overrides with exactly one row changed. Anything else differing
/// between the two would make the A/B measure that instead.
const OVERRIDES: &[(&str, Option<&str>)] = &[
    ("CRATONVM_TIER_PGO_RECEIVERS", Some("0")),
    ("CRATONVM_JIT_CHA", Some("0")),
];

fn test_resources_dir() -> String {
    if let Some(generated) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        if std::path::Path::new(generated).is_dir() {
            return generated.to_owned();
        }
    }
    format!("{}/tests/resources", env!("CARGO_MANIFEST_DIR"))
}

fn class_files_available() -> bool {
    std::path::Path::new(&format!(
        "{}/cratonvm/ChaUniqueImplementor.class",
        test_resources_dir()
    ))
    .exists()
}

fn warm_and_read_tally(vm: &mut Vm) -> Option<cratonvm_jit::InlineDecisionTally> {
    const ATTEMPTS: usize = 400;
    const CALLS_PER_ATTEMPT: i32 = 200;
    for _ in 0..ATTEMPTS {
        for i in 0..CALLS_PER_ATTEMPT {
            let _ = vm.invoke(CLASS, METHOD, DESC, &[Value::Int(i)]);
        }
        let class_id = vm
            .shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(CLASS)?;
        if let Some(compiled) = vm
            .shared
            .jit
            .jit_cache
            .read()
            .get(CLASS, METHOD, DESC, class_id)
        {
            return Some(compiled.inline_tally.clone());
        }
    }
    None
}

#[test]
fn with_class_hierarchy_analysis_off_the_same_site_is_not_bound() {
    if !class_files_available() {
        eprintln!("skipping: ChaUniqueImplementor.class not built");
        return;
    }
    cratonvm_types::flags::with_process_overrides(OVERRIDES, body);
}

fn body() {
    let mut vm = Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]));

    // Correct with CHA off too: `=0` must cost an optimisation, never an answer.
    match vm.invoke(CLASS, METHOD, DESC, &[Value::Int(21)]) {
        Ok(Some(Value::Int(42))) => {}
        other => panic!("the fixture must double its argument however it was compiled: {other:?}"),
    }

    let tally = warm_and_read_tally(&mut vm).expect(
        "ChaUniqueImplementor.run never compiled — an arm that never compiles reads zero bound \
         sites and would pass for a reason that has nothing to do with the switch",
    );

    assert_eq!(
        tally.hierarchy_bound_sites, 0,
        "CRATONVM_JIT_CHA=0 must withhold the unique-concrete resolver, so nothing may be bound \
         by the hierarchy. Tally: {tally:?}",
    );
    // And the site really was a candidate, so the zero above is a refusal and not
    // an absence. Without this, a fixture change that removed the call would
    // turn this into a test of nothing.
    assert!(
        tally.candidates > 0,
        "the interface site must still have been considered. Tally: {tally:?}",
    );
    assert_eq!(
        tally.inlined_sites, 0,
        "with neither a receiver profile nor CHA there is no evidence to inline on. Tally: \
         {tally:?}",
    );
}
