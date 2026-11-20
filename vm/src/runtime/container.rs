// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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

/// Default cap for the ergonomic default heap, in bytes (32 GiB): HotSpot's
/// own ceiling, `MaxRAM` (128 GiB on 64-bit) times `MaxRAMPercentage` (25%).
///
/// It was 4 GiB while the generational heap **eagerly committed** its arenas
/// (`Arena::new` → `vec![0u8; cap]`), when a 1/4-of-RAM heap charged that much
/// commit per process. Every heap store is now a reservation committed as it is
/// used (`gc::reservation::HeapStore`, default on; `CRATONVM_GC_RESERVE=0`
/// restores the wholly committed block), so the cap follows HotSpot's default
/// instead: a program without `-Xmx` gets HotSpot's `MaxHeapSize` (interpreter
/// round i1 wave 48, lane L7: on a 31 GiB host, HotSpot allocates a 4 GiB
/// `int[1 << 30]` that the 4 GiB cap refused, probe `L7W23ErrorShapes`).
/// `CRATONVM_DEFAULT_HEAP_MAX_MB=4096` restores the old cap.
pub const MAX_ERGONOMIC_HEAP: u64 = 32 * 1024 * 1024 * 1024;

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
    Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref() != Ok("0")
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
mod tests;
