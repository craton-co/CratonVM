// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Receiver proofs and the colored field barrier exercised together.

use super::*;
use crate::ir::IrBuilder;
use crate::x64::{ZgcColoredFieldLoadTestOverride, zgc_read_gate_flip_test_lock};
use cratonvm_types::narrow_oop::{
    COLORED_SLOT_METADATA_MASK, COLORED_SLOT_REMAPPED, COLORED_SLOT_TAG,
};

#[repr(C)]
struct HelperState {
    calls: usize,
    reply: i64,
}

unsafe extern "C" fn checked_field(vm: i64, _object: i64, _index: i64) -> i64 {
    // SAFETY: the fixture passes its live HelperState as the VM context.
    let state = unsafe { &mut *(vm as *mut HelperState) };
    state.calls += 1;
    state.reply
}

