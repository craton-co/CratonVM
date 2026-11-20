// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use std::sync::atomic::{AtomicU64, Ordering};

pub const MEMO_EMPTY: u64 = 0;

/// A one-word memo of "which native, if any, does this call site dispatch to".
///
/// The resolution methods live in `cratonvm_native_api::NativeCallSiteExt` so
/// `types` and `reader` can embed `NativeCallSite` without depending on
/// `native-api`.
pub struct NativeCallSite {
    pub memo: AtomicU64,
    #[cfg(debug_assertions)]
    pub triple_witness: AtomicU64,
}

impl NativeCallSite {
    #[inline]
    pub const fn new() -> Self {
        Self {
            memo: AtomicU64::new(MEMO_EMPTY),
            #[cfg(debug_assertions)]
            triple_witness: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn invalidate(&self) {
        self.memo.store(MEMO_EMPTY, Ordering::Relaxed);
    }
}

impl Default for NativeCallSite {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for NativeCallSite {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            memo: AtomicU64::new(self.memo.load(Ordering::Relaxed)),
            #[cfg(debug_assertions)]
            triple_witness: AtomicU64::new(self.triple_witness.load(Ordering::Relaxed)),
        }
    }
}
