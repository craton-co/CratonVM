// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The ON arm of `CRATONVM_JIT_CHA`: class-hierarchy analysis binds an
//! `invokeinterface` site that has no profile evidence at all.
//!
//! `NOTES-cha.md` recorded this as an open gap: "No end-to-end test that
//! `CRATONVM_JIT_CHA=0` actually changes a compile. What is tested is the
//! default and the two off-spellings." A flag test that only reads the flag
//! proves the flag parses, not that the resolver behind it does anything.
//!
//! # Why this fixture can only be answered by CHA
//!
//! `ChaUniqueImplementor.run` calls `Op.apply` through a field held at the
//! INTERFACE type, and both arms run with `CRATONVM_TIER_PGO_RECEIVERS=0`, so
//! the site has no receiver samples — before CHA, `plan_inline` refused it with
//! `NoProfileEvidence`. `Op` has exactly one concrete implementor loaded, so the
//! hierarchy decides the callee where the profile cannot.
//!
//! **The receiver switch is load-bearing, and it was found the hard way.** The
//! first version of this test relied on never calling `enable_profiling`, on
//! the belief that profiling is off by default. It is not, for receivers:
//! `CRATONVM_TIER_PGO_RECEIVERS` has been default-ON since 2026-09-02 and is a
//! separate switch from the branch-profiling `PROFILING_ENABLED` that
//! `enable_profiling` sets. The first run read
//! `speculative_sites: 1, hierarchy_bound_sites: 0, observed_calls_inlined: 509`
//! — the PROFILE had bound the site, CHA never got the chance, and the test
//! failed for the right reason about the wrong fixture. Turning receivers off
//! in BOTH arms leaves CHA as the one variable.
//!
//! That also says something about CHA in production worth knowing: with
//! receiver profiling on by default, most warm sites are profiled by the time
//! they compile. CHA's contribution is the sites that are NOT — compiled before
//! evidence accrues, or unreached during warmup. A soak reading
//! `hierarchy_bound_sites` should expect it to be small next to
//! `speculative_inlined_sites`, and small is not the same as broken.
//!
//! `InlineDecisionTally::hierarchy_bound_sites` counts exactly the plans whose
//! verdict is `InlineVerdict::UniqueConcreteMethod`, so it is a direct read on
//! "CHA planned something" rather than on "something got inlined".
//!
//! # Why a separate test binary from the off arm
//!
//! `env_cache::jit_cha` is a `MemoSlot`, latched on first read. Two arms in one
//! process would both see whichever value the first latched, and the second
//! would pass or fail for a reason that has nothing to do with its own
//! override. One process per arm is the only honest split — the same reasoning
//! `jit_deopt_sink_resume_off_arm.rs` gives.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const CLASS: &str = "cratonvm/ChaUniqueImplementor";
const METHOD: &str = "run";
const DESC: &str = "(I)I";

/// CHA is default-ON, and this names it anyway. An arm that relies on the
/// default cannot tell "the flag is on" from "the flag is ignored", and it is
/// the pair with the off arm that makes this an A/B rather than an outcome.
///
/// `CRATONVM_TIER_PGO_RECEIVERS=0` is identical in both arms. It is what makes
/// CHA the only thing that can bind this site; see the module doc for how it
/// was discovered to be necessary.
const OVERRIDES: &[(&str, Option<&str>)] = &[
    ("CRATONVM_TIER_PGO_RECEIVERS", Some("0")),
    ("CRATONVM_JIT_CHA", Some("1")),
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

/// Call until the method has a compiled body, and hand back its inline tally.
///
/// Returns `None` if it never compiled, which the caller turns into a failure
/// rather than a silent pass: "nothing was bound" and "nothing was compiled"
/// are the same number and completely different facts.
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
fn class_hierarchy_analysis_binds_an_unprofiled_interface_site() {
    if !class_files_available() {
        eprintln!("skipping: ChaUniqueImplementor.class not built");
        return;
    }
    cratonvm_types::flags::with_process_overrides(OVERRIDES, body);
}

fn body() {
    let mut vm = Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]));

    // The result has to be right whatever the inliner decided. A wrong answer
    // here would mean CHA bound the WRONG callee, which is the failure that
    // matters most and the one a tally cannot show.
    match vm.invoke(CLASS, METHOD, DESC, &[Value::Int(21)]) {
        Ok(Some(Value::Int(42))) => {}
        other => panic!("the fixture must double its argument however it was compiled: {other:?}"),
    }

    let tally = warm_and_read_tally(&mut vm).expect(
        "ChaUniqueImplementor.run never compiled — this arm is about what a COMPILE does, so \
         proceeding would assert zero bound sites and pass for the wrong reason",
    );

    assert!(
        tally.hierarchy_bound_sites > 0,
        "class-hierarchy analysis planned nothing at a site with exactly one loaded \
         implementor and no profile evidence. Tally: {tally:?}",
    );
}
