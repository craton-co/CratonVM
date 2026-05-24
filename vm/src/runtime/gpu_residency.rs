// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Side-table tracking `GpuArray<T>` host bytes and any pinned
//! device residency. Phase 3 surfaces this via
//! `craton.gpu.internal.Native.arrayWrap*`.

#[cfg(feature = "gpu-offload")]
use std::sync::Arc;
#[cfg(feature = "gpu-offload")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "gpu-offload")]
use parking_lot::RwLock;
#[cfg(feature = "gpu-offload")]
use rustc_hash::FxHashMap;

/// Primitive element type carried by a `GpuArray<T>`. The variants
/// match the JVM primitive-array shapes the Phase 3 native shims
/// admit.
#[cfg(feature = "gpu-offload")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimitiveType { I32, I64, F32, F64 }

#[cfg(feature = "gpu-offload")]
impl PrimitiveType {
    pub fn element_bytes(self) -> usize {
        match self {
            Self::I32 | Self::F32 => 4,
            Self::I64 | Self::F64 => 8,
        }
    }
}

/// One tracked array. `host_bytes` is the canonical copy; if the
/// data has been uploaded to a device buffer it lives in
/// `device_bytes` (None until first use). `last_stream` is the
/// stream the data was most recently touched on (for residency
/// ordering decisions).
#[cfg(feature = "gpu-offload")]
pub struct ResidentArray {
    pub element_type: PrimitiveType,
    pub host_bytes: Vec<u8>,
    pub device_bytes: Option<cuda_bridge::DeviceBuffer<u8>>,
    pub last_stream: Option<Arc<cuda_bridge::Stream>>,
}

#[cfg(feature = "gpu-offload")]
pub struct ResidencyTracker {
    arrays: RwLock<FxHashMap<u64, ResidentArray>>,
    next_handle: AtomicU64,
}

#[cfg(feature = "gpu-offload")]
impl ResidencyTracker {
    pub fn new() -> Self {
        Self {
            arrays: RwLock::new(FxHashMap::default()),
            next_handle: AtomicU64::new(1),
        }
    }

    /// Wrap a host byte buffer in a new resident-array entry; return
    /// the opaque handle the Java side stores in `GpuArray.handle`.
    pub fn wrap(&self, element_type: PrimitiveType, host_bytes: Vec<u8>) -> u64 {
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.arrays.write().insert(handle, ResidentArray {
            element_type,
            host_bytes,
            device_bytes: None,
            last_stream: None,
        });
        handle
    }

    /// Sync host-side copy of an array. None if the handle is unknown.
    pub fn to_host(&self, handle: u64) -> Option<Vec<u8>> {
        self.arrays.read().get(&handle).map(|a| a.host_bytes.clone())
    }

    pub fn element_type(&self, handle: u64) -> Option<PrimitiveType> {
        self.arrays.read().get(&handle).map(|a| a.element_type)
    }

    /// Whether the array currently has a live device buffer.
    pub fn is_resident(&self, handle: u64) -> bool {
        self.arrays.read().get(&handle).map_or(false, |a| a.device_bytes.is_some())
    }

    /// Release the entry. The device buffer drops; the host bytes
    /// drop. Idempotent on unknown handles.
    pub fn release(&self, handle: u64) {
        self.arrays.write().remove(&handle);
    }

    /// Iteration helpers for diagnostics / tests.
    pub fn len(&self) -> usize { self.arrays.read().len() }
}

#[cfg(feature = "gpu-offload")]
impl Default for ResidencyTracker {
    fn default() -> Self { Self::new() }
}

#[cfg(all(test, feature = "gpu-offload"))]
mod tests {
    use super::*;

    #[test]
    fn wrap_returns_unique_handles() {
        let tracker = ResidencyTracker::new();
        let h1 = tracker.wrap(PrimitiveType::I32, vec![0u8; 16]);
        let h2 = tracker.wrap(PrimitiveType::I32, vec![0u8; 16]);
        let h3 = tracker.wrap(PrimitiveType::F64, vec![0u8; 32]);
        assert_ne!(h1, h2);
        assert_ne!(h2, h3);
        assert_ne!(h1, h3);
        assert_eq!(tracker.len(), 3);
    }

    #[test]
    fn to_host_round_trips_bytes() {
        let tracker = ResidencyTracker::new();
        let payload: Vec<u8> = (0u8..32).collect();
        let handle = tracker.wrap(PrimitiveType::I64, payload.clone());
        let read_back = tracker.to_host(handle).expect("handle present");
        assert_eq!(read_back, payload);
        // Unknown handle returns None.
        assert!(tracker.to_host(0xDEAD_BEEF).is_none());
    }

    #[test]
    fn release_removes_entry() {
        let tracker = ResidencyTracker::new();
        let handle = tracker.wrap(PrimitiveType::F32, vec![1, 2, 3, 4]);
        assert_eq!(tracker.len(), 1);
        tracker.release(handle);
        assert_eq!(tracker.len(), 0);
        assert!(tracker.to_host(handle).is_none());
        // Idempotent on unknown / already-released handles.
        tracker.release(handle);
        tracker.release(0xDEAD_BEEF);
        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn element_type_remembered() {
        let tracker = ResidencyTracker::new();
        let h_i32 = tracker.wrap(PrimitiveType::I32, vec![0u8; 4]);
        let h_i64 = tracker.wrap(PrimitiveType::I64, vec![0u8; 8]);
        let h_f32 = tracker.wrap(PrimitiveType::F32, vec![0u8; 4]);
        let h_f64 = tracker.wrap(PrimitiveType::F64, vec![0u8; 8]);
        assert_eq!(tracker.element_type(h_i32), Some(PrimitiveType::I32));
        assert_eq!(tracker.element_type(h_i64), Some(PrimitiveType::I64));
        assert_eq!(tracker.element_type(h_f32), Some(PrimitiveType::F32));
        assert_eq!(tracker.element_type(h_f64), Some(PrimitiveType::F64));
        assert_eq!(PrimitiveType::I32.element_bytes(), 4);
        assert_eq!(PrimitiveType::F32.element_bytes(), 4);
        assert_eq!(PrimitiveType::I64.element_bytes(), 8);
        assert_eq!(PrimitiveType::F64.element_bytes(), 8);
        assert!(tracker.element_type(0xDEAD_BEEF).is_none());
    }

    #[test]
    fn is_resident_false_until_uploaded() {
        let tracker = ResidencyTracker::new();
        let handle = tracker.wrap(PrimitiveType::I32, vec![0u8; 16]);
        // Fresh entries have no device buffer.
        assert!(!tracker.is_resident(handle));
        // Unknown handles report not-resident as well.
        assert!(!tracker.is_resident(0xDEAD_BEEF));
    }
}
