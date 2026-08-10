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
    use super::{calculate_effective_cpus, ContainerInfo};
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

        let memory_limit =
            read_cgroup_file("/sys/fs/cgroup/memory.max").and_then(|s| parse_u64(&s));

        let memory_usage =
            read_cgroup_file("/sys/fs/cgroup/memory.current").and_then(|s| parse_u64(&s));

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
            cpu_shares: None,          // v2 uses cpu.weight, not shares
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

        let memory_limit = read_cgroup_file("/sys/fs/cgroup/memory/memory.limit_in_bytes")
            .and_then(|s| parse_u64(&s))
            // v1 uses a very large sentinel (PAGE_COUNTER_MAX << PAGE_SHIFT)
            // for "no limit". Anything above 2^62 bytes is effectively unlimited.
            .filter(|&v| v < (1u64 << 62));

        let memory_usage = read_cgroup_file("/sys/fs/cgroup/memory/memory.usage_in_bytes")
            .and_then(|s| parse_u64(&s));

        let cpu_quota =
            read_cgroup_file("/sys/fs/cgroup/cpu/cpu.cfs_quota_us").and_then(|s| parse_i64(&s));

        let cpu_period =
            read_cgroup_file("/sys/fs/cgroup/cpu/cpu.cfs_period_us").and_then(|s| parse_u64(&s));

        let cpu_shares =
            read_cgroup_file("/sys/fs/cgroup/cpu/cpu.shares").and_then(|s| parse_u64(&s));

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
    use super::{calculate_effective_cpus, ContainerInfo};

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

/// Floor for the ergonomic default heap, in bytes (the historical 256 MB
/// baseline).
pub const ERGONOMIC_HEAP_FLOOR: u64 = 256 * 1024 * 1024;

/// Default cap for the ergonomic default heap, in bytes (4 GiB).
///
/// The cap exists because CratonVM's generational heap **eagerly commits** its
/// arenas (`Arena::new` → `vec![0u8; cap]`): an uncapped 1/4-of-RAM heap (e.g.
/// 16 GiB on a 64 GiB host) would charge ~16 GiB of commit per process. The cap
/// keeps the default's commit bounded while still giving GC-heavy workloads
/// enough room. (If the heap is ever made lazily-committed, the cap can grow or
/// be removed to fully match HotSpot.)
pub const MAX_ERGONOMIC_HEAP: u64 = 4 * 1024 * 1024 * 1024;

/// The ergonomic default maximum heap, clamped — the ONE implementation.
///
/// Take 1/4 of `basis` (HotSpot's default `MaxRAMPercentage`), cap it at `cap`
/// (itself floored so a tiny `CRATONVM_DEFAULT_HEAP_MAX_MB` cannot drop below
/// the 256 MB floor), floor it at 256 MB, and finally bound it by `basis` itself
/// so a tiny container is never handed more than its whole limit.
///
/// # Why this is not two functions
///
/// It was. The launcher sized off `min(physical RAM, cgroup limit)` with a 4 GiB
/// cap and a 256 MB floor; `SharedVm::new` sized off the **cgroup limit alone**
/// with an 8 GiB cap and a 16 MiB floor and no host-RAM basis at all. Two
/// answers to one question, differing by 2x at the top end, and only one of them
/// carried the eager-commit reasoning that motivates having a cap. Both callers
/// now clamp here.
pub fn clamp_ergonomic_heap(basis: u64, cap: u64) -> usize {
    let quarter = basis / 4;
    let capped = quarter.min(cap.max(ERGONOMIC_HEAP_FLOOR));
    let floored = capped.max(ERGONOMIC_HEAP_FLOOR);
    let bounded = floored.min(basis);
    usize::try_from(bounded).unwrap_or(usize::MAX)
}

/// The cap in force, honouring `CRATONVM_DEFAULT_HEAP_MAX_MB`.
pub fn ergonomic_heap_cap() -> u64 {
    cratonvm_types::flags::runtime_var("CRATONVM_DEFAULT_HEAP_MAX_MB")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|mb| mb.saturating_mul(1024 * 1024))
        .unwrap_or(MAX_ERGONOMIC_HEAP)
}

/// Whether ergonomic default-heap sizing is enabled at all
/// (`CRATONVM_DEFAULT_HEAP_ERGONOMICS=0` opts out, giving the fixed 256 MB
/// library default).
pub fn ergonomic_heap_sizing_enabled() -> bool {
    cratonvm_types::flags::runtime_var("CRATONVM_DEFAULT_HEAP_ERGONOMICS").as_deref() != Ok("0")
}

/// The ergonomic default maximum heap in bytes, or `None` when ergonomics are
/// off or physical RAM cannot be determined.
///
/// `container_mem_limit` is the detected cgroup limit (or `None` when uncontained
/// or container support is off); the basis is then `min(physical RAM, cgroup
/// limit)`, because HotSpot's `MaxRAMPercentage` applies to the container limit
/// rather than the host total — on a 64 GB host with `--memory=512m` the default
/// heap is sized off 512 MB.
///
/// An explicit `-Xmx` always wins over this; the caller is responsible for not
/// calling it when one was given.
pub fn ergonomic_default_max_heap(container_mem_limit: Option<u64>) -> Option<usize> {
    if !ergonomic_heap_sizing_enabled() {
        return None;
    }
    let phys = physical_ram_bytes()?;
    let basis = match container_mem_limit {
        Some(limit) => phys.min(limit),
        None => phys,
    };
    Some(clamp_ergonomic_heap(basis, ergonomic_heap_cap()))
}

/// Suggest a container-aware default maximum heap size, in bytes.
///
/// When a cgroup memory limit is present, sizes it with
/// [`ergonomic_default_max_heap`] — the same arithmetic, the same cap and the
/// same env knobs the launcher uses. When no cgroup memory limit is known
/// (non-containerized, non-Linux, or "unlimited"), returns `fixed_default`
/// unchanged.
///
/// # Why an uncontained embedder still gets the fixed default
///
/// This is the one place the launcher and the library deliberately differ, and
/// it is about *whose process it is*. The launcher owns the process it sizes, so
/// taking a quarter of host RAM is its call to make. An embedded VM shares the
/// address space of an application that allocated its own memory before
/// `JNI_CreateJavaVM` was ever reached, and the heap commits eagerly — silently
/// claiming a quarter of the machine inside someone else's process is not a
/// library's decision. A cgroup limit IS an explicit statement about the
/// process's memory budget, which is why that case is sized and the bare-metal
/// case is not. An embedder that wants launcher ergonomics calls
/// [`ergonomic_default_max_heap`] and passes the result to
/// `VmConfig::with_max_heap_size`.
///
/// `SharedVm::new` uses this to seed `max_heap_size` only while the config is
/// still at the built-in default. Callers that set a non-default heap size keep
/// their configured value.
/// The basis is the cgroup limit itself rather than the launcher's
/// `min(physical RAM, limit)`: a cgroup limit is by construction a bound the
/// process cannot exceed, so the two differ only when the limit is configured
/// *above* physical RAM — a case where the container runtime has already made a
/// promise the machine cannot keep. Taking the limit here keeps the answer
/// independent of the host the code runs on, which the launcher's path (which
/// genuinely has no other basis) cannot be.
pub fn suggested_default_max_heap(info: &ContainerInfo, fixed_default: usize) -> usize {
    match info.memory_limit {
        Some(limit) if ergonomic_heap_sizing_enabled() => {
            clamp_ergonomic_heap(limit, ergonomic_heap_cap())
        }
        _ => fixed_default,
    }
}

/// Total physical RAM in bytes, or `None` if it can't be determined.
///
/// Used for HotSpot-style ergonomic default-heap sizing when the user did not
/// pass an explicit `-Xmx`. Mirrors the platform probes in
/// `vm::runtime::crash_handler` but returns the raw byte count.
pub fn physical_ram_bytes() -> Option<u64> {
    #[cfg(target_os = "windows")]
    {
        #[repr(C)]
        struct MemoryStatusEx {
            dw_length: u32,
            dw_memory_load: u32,
            ull_total_phys: u64,
            ull_avail_phys: u64,
            ull_total_page_file: u64,
            ull_avail_page_file: u64,
            ull_total_virtual: u64,
            ull_avail_virtual: u64,
            ull_avail_extended_virtual: u64,
        }
        extern "system" {
            fn GlobalMemoryStatusEx(lp_buffer: *mut MemoryStatusEx) -> i32;
        }
        let mut status = MemoryStatusEx {
            dw_length: std::mem::size_of::<MemoryStatusEx>() as u32,
            dw_memory_load: 0,
            ull_total_phys: 0,
            ull_avail_phys: 0,
            ull_total_page_file: 0,
            ull_avail_page_file: 0,
            ull_total_virtual: 0,
            ull_avail_virtual: 0,
            ull_avail_extended_virtual: 0,
        };
        let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
        if ok != 0 && status.ull_total_phys > 0 {
            return Some(status.ull_total_phys);
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        let content = std::fs::read_to_string("/proc/meminfo").ok()?;
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                let kb = rest.split_whitespace().next()?.parse::<u64>().ok()?;
                return Some(kb * 1024);
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        String::from_utf8(out.stdout)
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        None
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
        // 64 GiB container → 1/4 = 16 GiB, capped to the ergonomic 4 GiB.
        // This is the cap that used to be 8 GiB here and 4 GiB in the
        // launcher — one question, two answers.
        let info = ContainerInfo {
            memory_limit: Some(64 * 1024 * 1024 * 1024),
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
