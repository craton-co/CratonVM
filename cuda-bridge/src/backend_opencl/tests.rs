use super::*;

#[test]
fn amd_opencl_wavefront_width_handles_rdna_ryzen_ai_and_gcn() {
    assert_eq!(
        amd_opencl_warp_size(Some(32), Some(11), "AMD", "gfx1150"),
        32
    );
    assert_eq!(
        amd_opencl_warp_size(
            None,
            Some(11),
            "Advanced Micro Devices, Inc.",
            "AMD Radeon Graphics"
        ),
        32
    );
    assert_eq!(
        amd_opencl_warp_size(None, None, "AMD", "AMD Radeon 780M"),
        32
    );
    assert_eq!(amd_opencl_warp_size(None, None, "AMD", "gfx1152"), 32);
    assert_eq!(amd_opencl_warp_size(Some(64), Some(9), "AMD", "gfx90a"), 64);
    assert_eq!(
        amd_opencl_warp_size(None, None, "Intel(R) Corporation", "Intel UHD"),
        32
    );
}

#[test]
fn opencl_shared_memory_caps_detect_unified_and_svm() {
    let apu = shared_memory_caps_from_opencl(true, 1);
    assert!(apu.supports_zero_copy_shared_memory());
    assert!(!apu.pageable_memory_access);
    let fine_svm = shared_memory_caps_from_opencl(true, 1 << 2);
    assert!(fine_svm.supports_zero_copy_shared_memory());
    assert!(fine_svm.pageable_memory_access);
    let dgpu = shared_memory_caps_from_opencl(false, 0);
    assert!(!dgpu.supports_zero_copy_shared_memory());
}
