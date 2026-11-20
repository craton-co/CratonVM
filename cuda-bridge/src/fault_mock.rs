// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Deterministic fault-injecting driver test doubles for host CI.
//!
//! Lets `cuda-bridge` unit tests inject `DeviceError::Driver` failures at exact
//! step indices across [`VmmDriver`], [`SlabBackend`], and [`HostRegionPinCache`]
//! without GPU hardware or driver allocations, asserting strict leak-zero
//! rollback invariants on every failure path.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::allocator::{BackingSlab, FenceEvent, SlabBackend, SubAllocator};
use crate::pinned_pool::{set_vm_host_pin_quota, HostPinQuota, HostPinTestOps, HostRegionPinCache};
use crate::vmm::{DEFAULT_VMM_CHUNK_BYTES, VmmDriver, VmmPool};
use crate::{DeviceError, Result};

#[derive(Clone, Default)]
struct MockFence;

impl FenceEvent for MockFence {
    fn is_complete(&self) -> bool {
        true
    }

    fn sync(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum VmmOp {
    ReserveVa,
    FreeVa,
    CreatePhysical,
    ReleasePhysical,
    Map,
    SetAccess,
    Unmap,
}

#[derive(Default)]
pub(crate) struct VmmLedger {
    next_va: u64,
    next_handle: u64,
    reserved_vas: BTreeMap<u64, usize>,
    live_handles: HashSet<u64>,
    mapped: BTreeMap<u64, (usize, u64, bool)>,
    fail_after: HashMap<VmmOp, usize>,
    calls: HashMap<VmmOp, usize>,
}

impl VmmLedger {
    fn maybe_fail(&mut self, op: VmmOp) -> Result<()> {
        *self.calls.entry(op).or_insert(0) += 1;
        if let Some(rem) = self.fail_after.get_mut(&op) {
            if *rem == 0 {
                return Err(DeviceError::Driver(format!("injected fault in {op:?}")));
            }
            *rem -= 1;
        }
        Ok(())
    }
}

pub(crate) struct FaultMockVmmDriver {
    chunk_bytes: usize,
    ledger: Arc<Mutex<VmmLedger>>,
}

impl FaultMockVmmDriver {
    pub(crate) fn new(chunk_bytes: usize) -> Self {
        Self {
            chunk_bytes,
            ledger: Arc::new(Mutex::new(VmmLedger {
                next_va: 0x1_0000_0000,
                next_handle: 1,
                ..VmmLedger::default()
            })),
        }
    }

    pub(crate) fn fail_after(&self, op: VmmOp, remaining_successes: usize) {
        self.ledger
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .fail_after
            .insert(op, remaining_successes);
    }

    pub(crate) fn clear_fault(&self, op: VmmOp) {
        self.ledger
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .fail_after
            .remove(&op);
    }

    pub(crate) fn ledger_clone(&self) -> Arc<Mutex<VmmLedger>> {
        Arc::clone(&self.ledger)
    }

    pub(crate) fn assert_clean(ledger: &Mutex<VmmLedger>) {
        let st = ledger.lock().unwrap_or_else(|p| p.into_inner());
        assert!(
            st.reserved_vas.is_empty(),
            "leaked reserved VAs: {:?}",
            st.reserved_vas
        );
        assert!(
            st.live_handles.is_empty(),
            "leaked physical handles: {:?}",
            st.live_handles
        );
        assert!(st.mapped.is_empty(), "leaked mappings: {:?}", st.mapped);
    }
}

impl VmmDriver for FaultMockVmmDriver {
    type Handle = u64;

    fn granularity(&self) -> usize {
        self.chunk_bytes
    }

    fn reserve_va(&self, size: usize, _alignment: usize) -> Result<u64> {
        let mut st = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        st.maybe_fail(VmmOp::ReserveVa)?;
        let va = st.next_va;
        st.next_va = st.next_va.saturating_add(size as u64 + self.chunk_bytes as u64);
        st.reserved_vas.insert(va, size);
        Ok(va)
    }

    fn free_va(&self, va: u64, size: usize) -> Result<()> {
        let mut st = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        st.maybe_fail(VmmOp::FreeVa)?;
        for (&m_va, &(m_sz, _, _)) in &st.mapped {
            assert!(
                m_va + m_sz as u64 <= va || m_va >= va + size as u64,
                "free_va({va:#x}, {size}) while subrange ({m_va:#x}, {m_sz}) still mapped"
            );
        }
        let recorded = st
            .reserved_vas
            .remove(&va)
            .expect("free_va called for unreserved VA");
        assert_eq!(recorded, size, "free_va size mismatch");
        Ok(())
    }

    fn create_physical(&self, size: usize) -> Result<Self::Handle> {
        assert_eq!(size % self.chunk_bytes, 0);
        let mut st = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        st.maybe_fail(VmmOp::CreatePhysical)?;
        let h = st.next_handle;
        st.next_handle += 1;
        st.live_handles.insert(h);
        Ok(h)
    }

    fn release_physical(&self, handle: Self::Handle) -> Result<()> {
        let mut st = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        st.maybe_fail(VmmOp::ReleasePhysical)?;
        assert!(
            st.live_handles.remove(&handle),
            "double release or unknown physical handle {handle}"
        );
        Ok(())
    }

    fn map(&self, va: u64, size: usize, handle: Self::Handle) -> Result<()> {
        let mut st = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        st.maybe_fail(VmmOp::Map)?;
        assert!(
            st.live_handles.contains(&handle),
            "map with dead physical handle {handle}"
        );
        assert!(
            st.reserved_vas
                .iter()
                .any(|(&r_va, &r_sz)| va >= r_va && va + size as u64 <= r_va + r_sz as u64),
            "map({va:#x}, {size}) outside reserved VA range"
        );
        assert!(!st.mapped.contains_key(&va), "double map at {va:#x}");
        st.mapped.insert(va, (size, handle, false));
        Ok(())
    }

    fn set_access(&self, va: u64, size: usize) -> Result<()> {
        let mut st = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        st.maybe_fail(VmmOp::SetAccess)?;
        let entry = st
            .mapped
            .get_mut(&va)
            .expect("set_access on unmapped VA range");
        assert_eq!(entry.0, size);
        entry.2 = true;
        Ok(())
    }

    fn unmap(&self, va: u64, size: usize) -> Result<()> {
        let mut st = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        st.maybe_fail(VmmOp::Unmap)?;
        let (sz, _, _) = st
            .mapped
            .remove(&va)
            .expect("unmap on unmapped VA range");
        assert_eq!(sz, size, "unmap size mismatch");
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SlabOp {
    AllocSlab,
    AllocManaged,
    StreamPoolAlloc,
    StreamPoolFree,
}

#[derive(Default)]
pub(crate) struct SlabLedger {
    next_ptr: u64,
    next_id: u64,
    live_slabs: HashMap<u64, usize>,
    managed_slabs: HashMap<u64, usize>,
    live_stream_pool_ptrs: HashMap<u64, usize>,
    fail_after: HashMap<SlabOp, usize>,
}

impl SlabLedger {
    fn maybe_fail(&mut self, op: SlabOp) -> Result<()> {
        if let Some(rem) = self.fail_after.get_mut(&op) {
            if *rem == 0 {
                return Err(DeviceError::Driver(format!("injected fault in {op:?}")));
            }
            *rem -= 1;
        }
        Ok(())
    }
}

pub(crate) struct FaultMockSlabBackend {
    ledger: Arc<Mutex<SlabLedger>>,
}

impl FaultMockSlabBackend {
    pub(crate) fn new() -> Self {
        Self {
            ledger: Arc::new(Mutex::new(SlabLedger {
                next_ptr: 0x2_0000_0000,
                next_id: 1,
                ..SlabLedger::default()
            })),
        }
    }

    pub(crate) fn fail_after(&self, op: SlabOp, remaining_successes: usize) {
        self.ledger
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .fail_after
            .insert(op, remaining_successes);
    }

    pub(crate) fn ledger_clone(&self) -> Arc<Mutex<SlabLedger>> {
        Arc::clone(&self.ledger)
    }

    pub(crate) fn assert_clean(ledger: &Mutex<SlabLedger>) {
        let st = ledger.lock().unwrap_or_else(|p| p.into_inner());
        assert!(st.live_slabs.is_empty(), "leaked slabs: {:?}", st.live_slabs);
        assert!(
            st.managed_slabs.is_empty(),
            "leaked managed slabs: {:?}",
            st.managed_slabs
        );
        assert!(
            st.live_stream_pool_ptrs.is_empty(),
            "leaked stream-pool allocations: {:?}",
            st.live_stream_pool_ptrs
        );
    }
}

impl SlabBackend for FaultMockSlabBackend {
    type Slab = u64;

    fn alloc_slab(&self, min_bytes: usize) -> Result<BackingSlab<Self::Slab>> {
        let mut st = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        st.maybe_fail(SlabOp::AllocSlab)?;
        let ptr = st.next_ptr;
        st.next_ptr += min_bytes as u64 + 4096;
        let id = st.next_id;
        st.next_id += 1;
        st.live_slabs.insert(id, min_bytes);
        Ok(BackingSlab {
            base_ptr: ptr,
            size_bytes: min_bytes,
            handle: id,
        })
    }

    fn free_slab(&self, slab: BackingSlab<Self::Slab>) {
        let mut st = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        let removed = st
            .live_slabs
            .remove(&slab.handle)
            .or_else(|| st.managed_slabs.remove(&slab.handle));
        assert_eq!(removed, Some(slab.size_bytes), "double or mismatched free_slab");
    }

    fn supports_native_stream_pool(&self) -> bool {
        true
    }

    fn alloc_from_stream_pool(&self, bytes: usize, _stream_id: Option<u64>) -> Result<Option<u64>> {
        let mut st = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        st.maybe_fail(SlabOp::StreamPoolAlloc)?;
        let ptr = st.next_ptr;
        st.next_ptr += bytes as u64 + 4096;
        st.live_stream_pool_ptrs.insert(ptr, bytes);
        Ok(Some(ptr))
    }

    fn free_to_stream_pool(&self, ptr: u64, _stream_id: Option<u64>) -> Result<()> {
        let mut st = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        st.maybe_fail(SlabOp::StreamPoolFree)?;
        assert!(
            st.live_stream_pool_ptrs.remove(&ptr).is_some(),
            "double or unknown free_to_stream_pool({ptr:#x})"
        );
        Ok(())
    }

    fn alloc_managed_slab(
        &self,
        min_bytes: usize,
        _read_mostly: bool,
    ) -> Result<BackingSlab<Self::Slab>> {
        let mut st = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        st.maybe_fail(SlabOp::AllocManaged)?;
        let ptr = st.next_ptr;
        st.next_ptr += min_bytes as u64 + 4096;
        let id = st.next_id;
        st.next_id += 1;
        st.managed_slabs.insert(id, min_bytes);
        Ok(BackingSlab {
            base_ptr: ptr,
            size_bytes: min_bytes,
            handle: id,
        })
    }
}

struct FaultMockHostPinOps {
    remaining_successes: Mutex<Option<usize>>,
    live_pins: Mutex<BTreeMap<usize, usize>>,
    unregister_calls: AtomicU64,
}

impl FaultMockHostPinOps {
    fn new(fail_after_n: Option<usize>) -> Self {
        Self {
            remaining_successes: Mutex::new(fail_after_n),
            live_pins: Mutex::new(BTreeMap::new()),
            unregister_calls: AtomicU64::new(0),
        }
    }
}

impl HostPinTestOps for FaultMockHostPinOps {
    fn register(&self, base_addr: usize, len: usize) -> bool {
        let mut rem = self.remaining_successes.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(left) = rem.as_mut() {
            if *left == 0 {
                return false;
            }
            *left -= 1;
        }
        let mut pins = self.live_pins.lock().unwrap_or_else(|p| p.into_inner());
        assert!(
            pins.insert(base_addr, len).is_none(),
            "double cuMemHostRegister for {base_addr:#x}"
        );
        true
    }

    fn unregister(&self, base_addr: usize) {
        self.unregister_calls.fetch_add(1, Ordering::Relaxed);
        let mut pins = self.live_pins.lock().unwrap_or_else(|p| p.into_inner());
        assert!(
            pins.remove(&base_addr).is_some(),
            "double or unknown cuMemHostUnregister for {base_addr:#x}"
        );
    }
}

#[cfg(test)]
mod tests;
