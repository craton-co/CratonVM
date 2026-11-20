// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 7: out-of-line field-read tails for implicit-null sites.
//!
//! An inline `Op::Load` whose receiver null check is implicit (the first
//! receiver dereference faults on null) used to keep its checked-helper call
//! inline, behind a `JMP done`, because the fault's recovery pc had to be the
//! helper entry. The site now defers like every other admitted field read
//! ([`DeferredGetfieldTail`]), and the recovery pc is registered here, at the
//! tail's helper entry, when the tail is emitted after the body.
//!
//! The tail runs in exactly the state the inline helper ran in: the fault
//! leaves every register as it was before the faulting instruction, and the
//! tail's receiver source (`base_src`) was captured at the site before it.

use super::*;

impl Lowerer<'_> {
    /// [`Self::emit_deferred_getfield_tails`], one tail at a time, recording
    /// `(site, helper entry)` in `page_poll_sites` for every tail that owns an
    /// implicit-null fault site.
    ///
    /// A tail with an implicit-null site is built without a legacy arm
    /// (`legacy_patch: None`), so its first emitted byte is the helper entry
    /// that `slow` patches land on. A tail that broke that rule would register
    /// a recovery pc inside the legacy read, so it refuses the compile.
    pub(in crate::ir_lower) fn emit_deferred_getfield_tails_with_fault_entries(&mut self) {
        let tails = std::mem::take(&mut self.deferred_getfield_tails);
        for mut tail in tails {
            let site = tail.implicit_nn_site.take();
            if site.is_some() && tail.legacy_patch.is_some() {
                self.latch_bailout(Bailout::new(BailoutReason::Internal(
                    "ir_lower: an implicit-null field tail has a legacy arm",
                )));
                return;
            }
            let entry = self.buf.pos();
            self.deferred_getfield_tails.push(tail);
            self.emit_deferred_getfield_tails();
            if let Some(site) = site {
                self.page_poll_sites.push((site, entry));
            }
        }
    }
}
