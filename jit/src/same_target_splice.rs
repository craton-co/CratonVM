// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Share a bimorphic site's receiver-independent leaf body. Both inputs have
//! already passed the VM's receiver-specific dispatch and native-shadow checks.
//! The class-set guard admits only these exact classes; nulls, overrides and
//! later-loaded classes keep the original call. It establishes no exact-class
//! fact inside the shared body (`IrBuilder::cha_guarded_site`).

use super::IrGuardedBody;

pub(super) fn receiver_classes(first: &IrGuardedBody, second: &IrGuardedBody) -> Option<Vec<u32>> {
    let (a, ca) = first;
    let (b, cb) = second;
    let leaf = |s: &super::InlineSite| {
        !s.callee_is_static
            && !s.callee_is_synchronized
            && s.invoke_targets.is_empty()
            && s.nested_sites.is_empty()
            && s.elided_invoke_pcs.is_empty()
            && s.ir_indy_rows.is_empty()
            && s.ir_indy_capture_rows.is_empty()
            && s.ir_indy_capture_sites.is_empty()
            && s.ir_lambda_impl_rows.is_empty()
    };
    // A class id distinguishes identical names defined by different loaders.
    // Keep receiver-specialized call trees separate: a nested target may have
    // been resolved using the first receiver as an exact argument class.
    if a.class_id == 0
        || a.class_id != b.class_id
        || a.class_name != b.class_name
        || a.method_name != b.method_name
        || a.descriptor != b.descriptor
        || a.callee_code_len != b.callee_code_len
        || a.callee_code != b.callee_code
        || !leaf(a)
        || !leaf(b)
    {
        return None;
    }
    // The builder revalidates the set; ClassWordOf sign-extends its int.
    (*ca != 0 && *cb != 0 && ca != cb && *ca <= i32::MAX as u32 && *cb <= i32::MAX as u32)
        .then_some(vec![*ca, *cb])
}

#[cfg(test)]
mod tests;
