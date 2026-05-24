// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Container/cgroup detection for container-aware JVM behavior.
//!
//! Detects cgroup v1 and v2 memory/CPU limits so the JVM can automatically
//! size its heap and thread pools to respect container boundaries (equivalent
//! to HotSpot's `-XX:+UseContainerSupport`).
//!
//! On non-Linux platforms this module compiles but always reports
//! `is_containerized: false` with no limits.

/// Information about the container environment the JVM is running in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInfo {
    /// Whether we detected a container runtime (Docker, Podman, etc.).
    pub is_containerized: bool,

    /// Cgroup memory limit in bytes, if set.
    pub memory_limit: Option<u64>,

    /// Current cgroup memory usage in bytes, if available.
    pub memory_usage: Option<u64>,

    /// CPU quota in microseconds per period. `-1` means unlimited.
    pub cpu_quota: Option<i64>,

    /// CPU period in microseconds (typically 100 000).
    pub cpu_period: Option<u64>,

    /// CPU shares (relative weight, default 1024 in cgroup v1).
    pub cpu_shares: Option<u64>,

    /// Effective CPU count derived from quota/period, clamped to hardware count.
    pub effective_cpu_count: Option<u32>,
}

impl ContainerInfo {
    /// Returns a default non-containerized info with all fields `None`.
    fn non_containerized() -> Self {
        Self {
            is_containerized: false,
            memory_limit: None,
            memory_usage: None,
            cpu_quota: None,
            cpu_period: None,
            cpu_shares: None,
            effective_cpu_count: None,
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Linux implementation — real cgroup detection
// ───────────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod platform {
    use super::ContainerInfo;
    use std::fs;
    use std::path::Path;

    /// Main entry point: detect container environment and cgroup limits.
    pub fn detect_container() -> ContainerInfo {
        let containerized = is_running_in_container();

        // Try cgroup v2 first (unified hierarchy), fall back to v1.
        let mut info = detect_cgroup_v2()
            .or_else(detect_cgroup_v1)
            .unwrap_or_else(ContainerInfo::non_containerized);

        info.is_containerized = containerized;

        // Compute effective CPU count from quota/period if available.
        if let (Some(quota), Some(period)) = (info.cpu_quota, info.cpu_period) {
            info.effective_cpu_count = Some(calculate_effective_cpus(quota, period));
        }

        info
    }

    /// Check common container indicators.
    pub fn is_running_in_container() -> bool {
        // Docker creates /.dockerenv
        if Path::new("/.dockerenv").exists() {
            return true;
        }
        // Podman / Buildah create /run/.containerenv
        if Path::new("/run/.containerenv").exists() {
            return true;
        }
        // Check /proc/1/cgroup for container-specific paths
        if let Ok(content) = fs::read_to_string("/proc/1/cgroup") {
            for line in content.lines() {
                let lower = line.to_ascii_lowercase();
                if lower.contains("docker")
                    || lower.contains("kubepods")
                    || lower.contains("containerd")
                    || lower.contains("lxc")
                {
                    return true;
                }
            }
        }
        false
    }

    /// Read a cgroup file safely. Refuses to follow symlinks that escape
    /// `/sys/fs/cgroup` to avoid reading arbitrary system files.
    fn read_cgroup_file(path: &str) -> Option<String> {
        let p = Path::new(path);

        // Security: only read files under /sys/fs/cgroup.
        // Resolve the canonical path and verify it stays within the cgroup fs.
        match p.canonicalize() {
            Ok(canonical) => {
                if !canonical.starts_with("/sys/fs/cgroup") {
                    return None;
                }
            }
            Err(_) => {
                // File doesn't exist or can't be resolved — that's fine.
                return None;
            }
        }

        fs::read_to_string(p).ok().map(|s| s.trim().to_string())
    }

    /// Parse a string as u64, ignoring "max" / negative sentinels.
    fn parse_u64(s: &str) -> Option<u64> {
        let s = s.trim();
        if s == "max" || s == "-1" || s.is_empty() {
            return None;
        }
        s.parse::<u64>().ok()
    }

    /// Parse a string as i64.
    fn parse_i64(s: &str) -> Option<i64> {
        s.trim().parse::<i64>().ok()
    }

    /// Detect cgroup v2 (unified hierarchy) limits.
    pub fn detect_cgroup_v2() -> Option<ContainerInfo> {
        // Cgroup v2 uses a single hierarchy under /sys/fs/cgroup.
        // Check for the v2 sentinel file.
        if !Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
            return None;
        }

        let memory_limit = read_cgroup_file("/sys/fs/cgroup/memory.max")
            .and_then(|s| parse_u64(&s));

        let memory_usage = read_cgroup_file("/sys/fs/cgroup/memory.current")
            .and_then(|s| parse_u64(&s));

        // cpu.max format: "$MAX $PERIOD" or "max $PERIOD"
        let (cpu_quota, cpu_period) =
            if let Some(content) = read_cgroup_file("/sys/fs/cgroup/cpu.max") {
                let parts: Vec<&str> = content.split_whitespace().collect();
                if parts.len() == 2 {
                    let quota = if parts[0] == "max" {
                        Some(-1i64)
                    } else {
                        parse_i64(parts[0])
                    };
                    let period = parse_u64(parts[1]);
                    (quota, period)
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            };

        Some(ContainerInfo {
            is_containerized: false, // caller sets this
            memory_limit,
            memory_usage,
            cpu_quota,
            cpu_period,
            cpu_shares: None, // v2 uses cpu.weight, not shares
            effective_cpu_count: None, // caller computes this
        })
    }

    /// Detect cgroup v1 limits.
    pub fn detect_cgroup_v1() -> Option<ContainerInfo> {
        // v1 uses separate hierarchies per controller.
        let mem_dir = Path::new("/sys/fs/cgroup/memory");
        let cpu_dir = Path::new("/sys/fs/cgroup/cpu");

        if !mem_dir.exists() && !cpu_dir.exists() {
            return None;
        }

        let memory_limit =
            read_cgroup_file("/sys/fs/cgroup/memory/memory.limit_in_bytes")
                .and_then(|s| parse_u64(&s))
                // v1 uses a very large sentinel (PAGE_COUNTER_MAX << PAGE_SHIFT)
                // for "no limit". Anything above 2^62 bytes is effectively unlimited.
                .filter(|&v| v < (1u64 << 62));

        let memory_usage =
            read_cgroup_file("/sys/fs/cgroup/memory/memory.usage_in_bytes")
                .and_then(|s| parse_u64(&s));

        let cpu_quota =
            read_cgroup_file("/sys/fs/cgroup/cpu/cpu.cfs_quota_us")
                .and_then(|s| parse_i64(&s));

        let cpu_period =
            read_cgroup_file("/sys/fs/cgroup/cpu/cpu.cfs_period_us")
                .and_then(|s| parse_u64(&s));

        let cpu_shares =
            read_cgroup_file("/sys/fs/cgroup/cpu/cpu.shares")
                .and_then(|s| parse_u64(&s));

        Some(ContainerInfo {
            is_containerized: false,
            memory_limit,
            memory_usage,
            cpu_quota,
            cpu_period,
            cpu_shares,
            effective_cpu_count: None,
        })
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Non-Linux implementation — stubs that report "not containerized"
// ───────────────────────────────────────────────────────────────────────────

#[cfg(not(target_os = "linux"))]
mod platform {
    use super::ContainerInfo;

    pub fn detect_container() -> ContainerInfo {
        ContainerInfo::non_containerized()
    }

    pub fn is_running_in_container() -> bool {
        false
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Public API (platform-independent)
// ───────────────────────────────────────────────────────────────────────────

/// Detect the container environment and cgroup resource limits.
///
/// On Linux this reads cgroup v2 (then v1) files under `/sys/fs/cgroup/`.
/// On other platforms it returns a non-containerized default.
pub fn detect_container() -> ContainerInfo {
    platform::detect_container()
}

/// Quick check: are we inside a container?
pub fn is_running_in_container() -> bool {
    platform::is_running_in_container()
}

/// Calculate the effective number of CPUs from quota and period.
///
/// Returns `ceil(quota / period)`, clamped to at least 1.
/// If the quota is negative (unlimited), returns the hardware thread count.
pub fn calculate_effective_cpus(quota: i64, period: u64) -> u32 {
    if quota <= 0 || period == 0 {
        // Unlimited or invalid — fall back to hardware count.
        let hw = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1);
        return hw;
    }
    // ceil(quota / period), minimum 1
    let cpus = ((quota as u64) + period - 1) / period;
    std::cmp::max(cpus as u32, 1)
}

/// Return the effective memory limit, taking the minimum of the cgroup
/// limit (if any) and the user-configured maximum.
///
/// If no cgroup limit is set, returns `config_max` unchanged.
pub fn effective_memory_limit(info: &ContainerInfo, config_max: usize) -> usize {
    match info.memory_limit {
        Some(limit) => {
            let limit_usize = limit as usize;
            std::cmp::min(limit_usize, config_max)
        }
        None => config_max,
    }
}

/// Return the effective number of available processors.
///
/// Uses the cgroup-derived CPU count if available, otherwise falls back
/// to the hardware thread count.
pub fn effective_available_processors(info: &ContainerInfo) -> u32 {
    info.effective_cpu_count.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1)
    })
}

// ───────────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
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
        assert_eq!(effective_memory_limit(&info, 512 * 1024 * 1024), 512 * 1024 * 1024);
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

    // ── effective_available_processors ────────────────────────────────

    #[test]
    fn available_processors_with_cgroup_count() {
        let info = ContainerInfo {
            effective_cpu_count: Some(4),
            ..ContainerInfo::non_containerized()
        };
        assert_eq!(effective_available_processors(&info), 4);
    }

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
}
