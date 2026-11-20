// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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




