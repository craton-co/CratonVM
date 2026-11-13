// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! OpenCL target properties and device capability representations.

/// The OpenCL target a kernel is rendered for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenClTarget {
    /// OpenCL specification version, e.g. (1, 2), (2, 0), (3, 0).
    pub opencl_version: (u32, u32),
    /// Double-precision floating point support (`cl_khr_fp64`).
    pub fp64: bool,
    /// 64-bit integer atomics in global memory (`cl_khr_int64_base_atomics`).
    pub global_int64_atomics: bool,
    /// Preferred subgroup / warp size (typically 32 on NVIDIA/Intel/RDNA, 64 on GCN).
    pub warp_size: u32,
    /// Device identifier or name for diagnostics.
    pub device_name: String,
}

impl Default for OpenClTarget {
    fn default() -> Self {
        Self {
            opencl_version: (1, 2),
            fp64: true,
            global_int64_atomics: true,
            warp_size: 32,
            device_name: "Generic OpenCL Device".to_string(),
        }
    }
}

impl OpenClTarget {
    pub fn new(
        opencl_version: (u32, u32),
        fp64: bool,
        global_int64_atomics: bool,
        warp_size: u32,
        device_name: impl Into<String>,
    ) -> Self {
        Self {
            opencl_version,
            fp64,
            global_int64_atomics,
            warp_size: warp_size.max(1),
            device_name: device_name.into(),
        }
    }
}
