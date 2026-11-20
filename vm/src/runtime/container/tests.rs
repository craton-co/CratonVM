use super::*;

// ── calculate_effective_cpus ──────────────────────────────────────

#[test]
fn effective_cpus_exact_division() {
    // 200_000 / 100_000 = 2 CPUs
    assert_eq!(calculate_effective_cpus(200_000, 100_000), 2);
}

#[test]
fn effective_cpus_rounds_up() {
    // 150_000 / 100_000 = 1.5 → rounds up to 2
    assert_eq!(calculate_effective_cpus(150_000, 100_000), 2);
}

#[test]
fn effective_cpus_minimum_one() {
    // 1 / 100_000 = 0.00001 → clamped to 1
    assert_eq!(calculate_effective_cpus(1, 100_000), 1);
}

#[test]
fn effective_cpus_single_cpu() {
    // 100_000 / 100_000 = exactly 1
    assert_eq!(calculate_effective_cpus(100_000, 100_000), 1);
}

#[test]
fn effective_cpus_large_quota() {
    // 800_000 / 100_000 = 8 CPUs
    assert_eq!(calculate_effective_cpus(800_000, 100_000), 8);
}

#[test]
fn effective_cpus_unlimited_quota() {
    // -1 means unlimited — should return hardware thread count
    let hw = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    assert_eq!(calculate_effective_cpus(-1, 100_000), hw);
}

#[test]
fn effective_cpus_zero_period() {
    // period=0 is invalid — should return hardware count
    let hw = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    assert_eq!(calculate_effective_cpus(200_000, 0), hw);
}

#[test]
fn effective_cpus_fractional_small() {
    // 50_000 / 100_000 = 0.5 → rounds up to 1
    assert_eq!(calculate_effective_cpus(50_000, 100_000), 1);
}

// ── effective_memory_limit ────────────────────────────────────────

#[test]
fn memory_limit_no_cgroup() {
    let info = ContainerInfo::non_containerized();
    assert_eq!(
        effective_memory_limit(&info, 512 * 1024 * 1024),
        512 * 1024 * 1024
    );
}

#[test]
fn memory_limit_cgroup_lower() {
    let info = ContainerInfo {
        memory_limit: Some(256 * 1024 * 1024),
        ..ContainerInfo::non_containerized()
    };
    // cgroup says 256 MB, config says 512 MB → use 256 MB
    assert_eq!(
        effective_memory_limit(&info, 512 * 1024 * 1024),
        256 * 1024 * 1024
    );
}

#[test]
fn memory_limit_config_lower() {
    let info = ContainerInfo {
        memory_limit: Some(1024 * 1024 * 1024),
        ..ContainerInfo::non_containerized()
    };
    // cgroup says 1 GB, config says 256 MB → use 256 MB
    assert_eq!(
        effective_memory_limit(&info, 256 * 1024 * 1024),
        256 * 1024 * 1024
    );
}

#[test]
fn memory_limit_equal() {
    let info = ContainerInfo {
        memory_limit: Some(512 * 1024 * 1024),
        ..ContainerInfo::non_containerized()
    };
    assert_eq!(
        effective_memory_limit(&info, 512 * 1024 * 1024),
        512 * 1024 * 1024
    );
}

// ── suggested_default_max_heap ────────────────────────────────────

#[test]
fn suggested_heap_no_cgroup_returns_fixed_default() {
    let info = ContainerInfo::non_containerized();
    let fixed = 256 * 1024 * 1024;
    assert_eq!(suggested_default_max_heap(&info, fixed), fixed);
}

#[test]
fn suggested_heap_quarter_of_limit() {
    // 4 GiB container → 1/4 = 1 GiB default heap.
    let info = ContainerInfo {
        memory_limit: Some(4 * 1024 * 1024 * 1024),
        ..ContainerInfo::non_containerized()
    };
    assert_eq!(
        suggested_default_max_heap(&info, 256 * 1024 * 1024),
        1024 * 1024 * 1024
    );
}

#[test]
fn suggested_heap_capped_for_large_container() {
    // 256 GiB container → 1/4 = 64 GiB, capped to the ergonomic 32 GiB
    // (HotSpot's `MaxRAM` x 25%). This cap used to be 8 GiB here and
    // 4 GiB in the launcher — one question, two answers.
    let info = ContainerInfo {
        memory_limit: Some(256 * 1024 * 1024 * 1024),
        ..ContainerInfo::non_containerized()
    };
    assert_eq!(
        suggested_default_max_heap(&info, 256 * 1024 * 1024),
        MAX_ERGONOMIC_HEAP as usize
    );
}

#[test]
fn suggested_heap_floored_but_within_limit() {
    // 512 MiB container → 1/4 = 128 MiB, raised to the 256 MiB floor
    // (which is the launcher's floor; this path used to floor at 16 MiB).
    let info = ContainerInfo {
        memory_limit: Some(512 * 1024 * 1024),
        ..ContainerInfo::non_containerized()
    };
    assert_eq!(
        suggested_default_max_heap(&info, 256 * 1024 * 1024),
        ERGONOMIC_HEAP_FLOOR as usize
    );
}

#[test]
fn suggested_heap_tiny_container_never_exceeds_limit() {
    // 8 MiB container: 1/4 = 2 MiB → the floor would be 256 MiB, but the
    // limit is only 8 MiB, so the suggestion must be clamped to 8 MiB.
    let info = ContainerInfo {
        memory_limit: Some(8 * 1024 * 1024),
        ..ContainerInfo::non_containerized()
    };
    assert_eq!(
        suggested_default_max_heap(&info, 256 * 1024 * 1024),
        8 * 1024 * 1024
    );
}

/// The property the split implementations did not have: for one basis, the
/// launcher's sizer and the embedder's sizer produce the same number.
#[test]
fn the_launcher_and_the_embedder_size_a_container_identically() {
    for limit_gib in [1u64, 2, 4, 8, 16, 64] {
        let limit = limit_gib * 1024 * 1024 * 1024;
        let info = ContainerInfo {
            memory_limit: Some(limit),
            ..ContainerInfo::non_containerized()
        };
        assert_eq!(
            suggested_default_max_heap(&info, 256 * 1024 * 1024),
            clamp_ergonomic_heap(limit, MAX_ERGONOMIC_HEAP),
            "the two sizers disagreed at a {limit_gib} GiB limit"
        );
    }
}

#[test]
fn suggested_heap_is_pure_independent_of_fixed_default_when_containerized() {
    // The fixed default is ignored when a cgroup limit is present.
    let info = ContainerInfo {
        memory_limit: Some(2 * 1024 * 1024 * 1024),
        ..ContainerInfo::non_containerized()
    };
    let a = suggested_default_max_heap(&info, 1);
    let b = suggested_default_max_heap(&info, usize::MAX);
    assert_eq!(a, b);
    assert_eq!(a, 512 * 1024 * 1024); // 1/4 of 2 GiB
}

// ── effective_available_processors ────────────────────────────────

#[test]
fn available_processors_with_cgroup_count() {
    let info = ContainerInfo {
        effective_cpu_count: Some(4),
        ..ContainerInfo::non_containerized()
    };
    assert_eq!(effective_available_processors(&info), 4);    }

#[test]
fn available_processors_no_cgroup() {
    let info = ContainerInfo::non_containerized();
    let hw = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    assert_eq!(effective_available_processors(&info), hw);
}

// ── detect_container on non-Linux ────────────────────────────────

#[test]
fn detect_container_returns_sensible_defaults() {
    let info = detect_container();
    // On non-Linux (CI/dev machines), or Linux without containers,
    // we should still get a valid struct.
    // At minimum, the struct should be well-formed.
    let _ = format!("{:?}", info);

    // If not containerized, limits should be None.
    if !info.is_containerized {
        // On non-Linux, all fields are None.
        #[cfg(not(target_os = "linux"))]
        {
            assert!(!info.is_containerized);
            assert!(info.memory_limit.is_none());
            assert!(info.memory_usage.is_none());
            assert!(info.cpu_quota.is_none());
            assert!(info.cpu_period.is_none());
            assert!(info.cpu_shares.is_none());
            assert!(info.effective_cpu_count.is_none());
        }
    }
}

#[test]
fn is_running_in_container_returns_bool() {
    // Smoke test — just verify it doesn't panic.
    let _ = is_running_in_container();
}

// ── ContainerInfo construction ───────────────────────────────────

#[test]
fn container_info_non_containerized_defaults() {
    let info = ContainerInfo::non_containerized();
    assert!(!info.is_containerized);
    assert!(info.memory_limit.is_none());
    assert!(info.memory_usage.is_none());
    assert!(info.cpu_quota.is_none());
    assert!(info.cpu_period.is_none());
    assert!(info.cpu_shares.is_none());
    assert!(info.effective_cpu_count.is_none());
}

#[test]
fn container_info_clone_is_independent() {
    let info1 = ContainerInfo {
        is_containerized: true,
        memory_limit: Some(1024),
        ..ContainerInfo::non_containerized()
    };
    let mut info2 = info1.clone();
    info2.memory_limit = Some(2048);
    assert_eq!(info1.memory_limit, Some(1024));
    assert_eq!(info2.memory_limit, Some(2048));
}

#[test]
fn container_info_equality() {
    let a = ContainerInfo::non_containerized();
    let b = ContainerInfo::non_containerized();
    assert_eq!(a, b);
}

// ── Cgroup file content parsing (unit tests) ─────────────────────

#[test]
fn parse_cpu_max_with_quota_and_period() {
    // Simulate parsing "200000 100000" from cpu.max
    let content = "200000 100000";
    let parts: Vec<&str> = content.split_whitespace().collect();
    assert_eq!(parts.len(), 2);
    let quota: i64 = parts[0].parse().unwrap();
    let period: u64 = parts[1].parse().unwrap();
    assert_eq!(quota, 200_000);
    assert_eq!(period, 100_000);
    assert_eq!(calculate_effective_cpus(quota, period), 2);
}

#[test]
fn parse_cpu_max_unlimited() {
    // Simulate parsing "max 100000" from cpu.max
    let content = "max 100000";
    let parts: Vec<&str> = content.split_whitespace().collect();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0], "max");
    let period: u64 = parts[1].parse().unwrap();
    assert_eq!(period, 100_000);
    // "max" means unlimited → quota = -1
    let quota: i64 = -1;
    let hw = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    assert_eq!(calculate_effective_cpus(quota, period), hw);
}

#[test]
fn parse_memory_max_numeric() {
    let content = "268435456"; // 256 MB
    let val: u64 = content.trim().parse().unwrap();
    assert_eq!(val, 256 * 1024 * 1024);
}

#[test]
fn parse_memory_max_unlimited() {
    let content = "max";
    let result = content.trim().parse::<u64>();
    assert!(result.is_err()); // "max" is not a number
}

#[test]
fn parse_cgroup_v1_quota_unlimited() {
    // v1 uses -1 for unlimited quota
    let content = "-1";
    let val: i64 = content.trim().parse().unwrap();
    assert_eq!(val, -1);
}

#[test]
fn parse_cgroup_v1_memory_sentinel() {
    // v1 uses a very large value for "no limit" (typically 2^63 - 4096 or similar)
    let sentinel: u64 = 9223372036854771712;
    // Our detection filters values >= 2^62 as "unlimited"
    assert!(sentinel >= (1u64 << 62));
}

// ── Cross-platform / Windows safety ──────────────────────────────
//
// T6.8: on non-Linux platforms detect_container() must NEVER panic,
// must NEVER touch /proc or /sys (they don't exist), and must cleanly
// report "not containerized" with all limits None.

/// On Windows/macOS the detector must report is_containerized=false
/// and leave every limit field unset.
#[cfg(not(target_os = "linux"))]
#[test]
fn windows_detector_reports_non_containerized() {
    let info = detect_container();
    assert!(!info.is_containerized);
    assert!(info.memory_limit.is_none());
    assert!(info.memory_usage.is_none());
    assert!(info.cpu_quota.is_none());
    assert!(info.cpu_period.is_none());
    assert!(info.cpu_shares.is_none());
    assert!(info.effective_cpu_count.is_none());
}

/// On every platform, detect_container() must return a value without
/// panicking, and the effective processor count must be at least 1.
#[test]
fn cross_platform_detect_never_panics() {
    let info = detect_container();
    let procs = effective_available_processors(&info);
    assert!(procs >= 1, "available processors must be >= 1");
}

/// T6.8.3: when UseContainerSupport is enabled (the default), the
/// effective memory limit must be the minimum of cgroup limit and
/// config maximum.  Verified with a simulated containerized info.
#[test]
fn use_container_support_enforces_cgroup_minimum() {
    // Simulate running inside a 128 MiB container.
    let info = ContainerInfo {
        is_containerized: true,
        memory_limit: Some(128 * 1024 * 1024),
        cpu_quota: Some(200_000),
        cpu_period: Some(100_000),
        effective_cpu_count: Some(2),
        ..ContainerInfo::non_containerized()
    };

    // User asked for 4 GiB heap — cgroup should win.
    assert_eq!(
        effective_memory_limit(&info, 4 * 1024 * 1024 * 1024),
        128 * 1024 * 1024
    );
    // availableProcessors must honor cgroup, not hardware.
    assert_eq!(effective_available_processors(&info), 2);
}

/// The detector code path must consider BOTH cgroup v2 AND v1.
/// Without access to a real cgroupfs we cannot execute the probes,
/// but we can at least confirm both helpers exist and are reachable
/// on Linux builds (compile-gated).
#[cfg(target_os = "linux")]
#[test]
fn linux_detector_probes_both_cgroup_versions() {
    use super::platform;
    // Both probes must return Option<ContainerInfo> without panic.
    let _v2 = platform::detect_cgroup_v2();
    let _v1 = platform::detect_cgroup_v1();
    let _is_c = platform::is_running_in_container();
}

/// ContainerInfo sizes: the struct must remain ABI-stable-ish
/// (all fields Clone, PartialEq, Eq, Debug).  Compile-time check.
#[test]
fn container_info_has_required_traits() {
    fn assert_traits<T: Clone + std::fmt::Debug + PartialEq + Eq>() {}
    assert_traits::<ContainerInfo>();
}
