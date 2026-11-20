// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Race-free synchronization between generational owed-cycle state and [`GcBarrier`]'s poll page.

use std::sync::atomic::Ordering;

use crate::threading::gc_barrier::GcBarrier;

impl GcBarrier {
    /// Synchronize the barrier's poll page with the owed-cycle latch under
    /// `loop_poll_words`, re-reading `owed()` inside the lock so concurrent
    /// latch/take and STW raise/lower transitions serialize in one total order.
    pub(crate) fn sync_owed_poll_page(&self, owed: &dyn Fn() -> bool) {
        let _words = self.loop_poll_words.lock();
        let owed_active = owed();
        self.owed_poll_armed.store(owed_active, Ordering::SeqCst);
        let want_stop = owed_active || self.stw_requested.load(Ordering::SeqCst);
        if self.page_poll_base != 0 {
            self.set_poll_page_stop(want_stop);
        }
    }
}

#[cfg(test)]
mod tests;
