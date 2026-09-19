// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! How many classes this process has DEFINED, and which ones.
//!
//! # Why this exists
//!
//! `runtime::diagnostics::classes_loaded` was declared, initialised, reset,
//! formatted into the diagnostic report and unit-tested — and incremented by
//! nothing. It therefore reported `Classes loaded: 0` on every run, which reads
//! as an answer rather than as an absent instrument. That silence is load
//! bearing: `JitCache::invalidate_for_class` runs on class DEFINITION and
//! nowhere else, and it sat at ~1% of an H2 update profile taken minutes past
//! warm-up, with no way to ask what was still defining classes at that point.
//!
//! The counter here is unconditional and is one relaxed add on a path that is
//! not supposed to be hot — and if it turns out to be hot, that is precisely
//! the finding this instrument exists to make.
//!
//! `CRATONVM_DBG_DEFINE_CENSUS=1` additionally keeps a per-name tally and dumps
//! it hottest-first, so "what is defining classes" is answered by name rather
//! than by inference.

use std::sync::atomic::{AtomicU64, Ordering};

static DEFINES: AtomicU64 = AtomicU64::new(0);

/// Bound on distinct names retained by the census, so a workload that mints a
/// fresh name per definition (hidden classes, lambda proxies, generated
/// proxies) cannot grow it without limit. Counting continues past the cap; only
/// new *names* stop being admitted, and the dump says how many it dropped.
const CENSUS_NAME_CAP: usize = 8192;

#[allow(clippy::type_complexity)]
static CENSUS: std::sync::OnceLock<
    parking_lot::Mutex<(std::collections::HashMap<String, u64>, u64)>,
> = std::sync::OnceLock::new();

/// Record one class definition. Called from the single choke point every
/// `define_class*` entry point funnels through.
#[inline]
pub fn note(name: &str) {
    DEFINES.fetch_add(1, Ordering::Relaxed);
    if !crate::loader_flags().dbg_define_census {
        return;
    }
    note_slow(name);
}

#[cold]
fn note_slow(name: &str) {
    let cell = CENSUS.get_or_init(|| parking_lot::Mutex::new((Default::default(), 0)));
    let mut guard = cell.lock();
    let (map, dropped) = &mut *guard;
    if let Some(slot) = map.get_mut(name) {
        *slot += 1;
    } else if map.len() < CENSUS_NAME_CAP {
        map.insert(name.to_string(), 1);
    } else {
        *dropped += 1;
    }
}

/// Total class definitions since VM start.
///
/// This is DEFINITIONS, not distinct classes: a name defined by two loaders
/// counts twice, and so does a redefinition. That is the quantity the JIT-cache
/// invalidation and the class-manager write lock actually pay for.
pub fn total() -> u64 {
    DEFINES.load(Ordering::Relaxed)
}

/// Dump the per-name tally, hottest first. No-op unless
/// `CRATONVM_DBG_DEFINE_CENSUS` is set.
pub fn dump() {
    if !crate::loader_flags().dbg_define_census {
        return;
    }
    let Some(cell) = CENSUS.get() else {
        eprintln!("[define-census] {} definitions, no names recorded", total());
        return;
    };
    let (map, dropped) = &*cell.lock();
    let mut rows: Vec<(&String, &u64)> = map.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    eprintln!(
        "[define-census] {} definitions over {} distinct names ({} definitions of names dropped at the {} cap)",
        total(),
        map.len(),
        dropped,
        CENSUS_NAME_CAP
    );
    for (name, count) in rows.iter().take(30) {
        eprintln!("[define-census]   {count:>9} {name}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counter must move on a definition — the whole point is that its
    /// predecessor did not, and a replacement that also reads zero would be
    /// the same defect with a new name.
    #[test]
    fn note_increments_the_total() {
        let before = total();
        note("com/example/Probe");
        assert_eq!(
            total(),
            before + 1,
            "define_census::note did not increment; a counter that never moves \
             reports a confident zero, which is exactly what it replaced"
        );
    }
}
