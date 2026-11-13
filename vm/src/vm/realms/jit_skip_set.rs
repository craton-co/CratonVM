// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
//! Version every write to the method-entry seal set, including removal and
//! unload. Entry records can memoize a lookup without missing a sealing site.

use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use rustc_hash::FxHashSet;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

type Seals = FxHashSet<(Arc<str>, Arc<str>, Arc<str>)>;

pub struct JitSkipSet {
    entries: RwLock<Seals>,
    // Even: a published version. Odd: a writer holds the set. Zero is the
    // entry record's never-probed stamp, so published versions start at two.
    epoch: AtomicU64,
}

impl Default for JitSkipSet {
    fn default() -> Self {
        Self {
            entries: RwLock::new(Seals::default()),
            epoch: AtomicU64::new(2),
        }
    }
}

impl JitSkipSet {
    #[inline]
    pub fn read(&self) -> RwLockReadGuard<'_, Seals> {
        self.entries.read()
    }

    pub fn write(&self) -> JitSkipSetWriteGuard<'_> {
        let entries = self.entries.write();
        self.epoch.fetch_add(1, Ordering::AcqRel);
        JitSkipSetWriteGuard {
            entries,
            epoch: &self.epoch,
        }
    }

    #[inline]
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }
}

pub struct JitSkipSetWriteGuard<'a> {
    entries: RwLockWriteGuard<'a, Seals>,
    epoch: &'a AtomicU64,
}

impl Deref for JitSkipSetWriteGuard<'_> {
    type Target = Seals;
    fn deref(&self) -> &Seals {
        &self.entries
    }
}

impl DerefMut for JitSkipSetWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Seals {
        &mut self.entries
    }
}

impl Drop for JitSkipSetWriteGuard<'_> {
    fn drop(&mut self) {
        // Publish while still holding the write guard. A reader either sees
        // this completed version or blocks until it can read that version.
        self.epoch.fetch_add(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_writer_withdraws_the_previous_version_before_mutating() {
        let set = JitSkipSet::default();
        let key = (Arc::from("Probe"), Arc::from("call"), Arc::from("()V"));
        let first = set.epoch();
        assert_eq!(first & 1, 0);
        {
            let mut writer = set.write();
            assert_eq!(set.epoch() & 1, 1);
            assert_ne!(set.epoch(), first);
            writer.insert(key.clone());
        }
        let sealed = set.epoch();
        assert_eq!(sealed & 1, 0);
        assert!(set.read().contains(&key));
        set.write().remove(&key);
        assert_ne!(set.epoch(), sealed);
        assert!(!set.read().contains(&key));
    }
}
