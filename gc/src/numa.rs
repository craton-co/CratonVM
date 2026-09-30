// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! NUMA topology detection (Phase 16.3).
//!
//! Real platform NUMA detection lives in [`NumaTopology::detect`] with
//! per-OS code paths (Linux sysfs, Windows env-based fallback, macOS proxy,
//! generic single-node fallback). The detected topology is cached in a
//! `OnceLock` accessible via [`global_topology`] so other crates can share
//! the result without re-reading sysfs.
//!
//! # No NUMA placement happens. This module only *detects*.
//!
//! Audit note, 2026-07-26 (`arch-2026-07-26/refs-metaspace-unloading`), stated
//! plainly because the module name implies otherwise.
//!
//! **Live on the default path** — exactly three entry points, consumed by
//! `gc/src/gen_heap.rs` and `gc/src/heap.rs` (search `numa::`):
//!
//! * [`global_topology`] and [`NumaTopology::current_thread_node`], read once
//!   at `GenerationalHeap` construction and again on the allocation slow path;
//! * [`NumaTopology::node_of_cpu`], used by the above.
//!
//! What `gen_heap` does with the answer is store it in a `numa_node_hint`
//! field and, on a multi-node host, emit a `tracing::trace!` when the calling
//! thread's node differs from the heap's primary node. It calls this a "NUMA
//! stub" in its own comments. **No allocation is ever steered to a node**: the
//! heap has one young arena and one old generation, memory comes from the
//! process allocator, and nothing calls `mbind`, `set_mempolicy`,
//! `numa_alloc_onnode`, or `VirtualAllocExNuma`. On a single-node host (every
//! current CI and dev target) even the trace is dead — it is two integer
//! compares. Windows NUMA is not detected at all (one node, sized from
//! `NUMBER_OF_PROCESSORS`).
//!
//! **Deleted 2026-09-23** (gc-common round, wave 2, lane E — proposal
//! `common-f-proposal-numa-module-cleanup-FIXED-20260923.md`): `NumaAllocator`, `NumaArena`,
//! `NumaAllocation`, `NumaStats`, `NumaPolicy` (a simulation that bumped an
//! integer from `0x1000` and touched no memory), `fnv1a_hash` and the
//! `StringDeduplicator` family (never wired to `-XX:+UseStringDeduplication`),
//! about 800 lines with their tests. None had a caller anywhere in the
//! workspace; they made this module read as if CratonVM did NUMA placement and
//! string deduplication.
//!
//! So: any report, dashboard or doc claiming CratonVM does NUMA-aware
//! placement is wrong. The topology probe is real and correct; the placement
//! it would inform does not exist. See
//! `refs-metaspace-unloading.md` for what wiring
//! it would take.

use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// 16.3  NUMA topology
// ---------------------------------------------------------------------------

/// A single NUMA node with associated CPUs and memory.
#[derive(Debug, Clone)]
pub struct NumaNode {
    pub id: usize,
    pub cpu_count: usize,
    pub memory_size_bytes: u64,
    pub free_memory_bytes: u64,
    pub cpu_ids: Vec<usize>,
}

/// Discovered NUMA topology of the host.
///
/// Carries both the legacy rich `nodes` view (its one consumer, the
/// simulated `NumaAllocator`, was deleted on 2026-09-23) and the
/// flat per-node slices required by the cross-crate topology contract
/// (`num_nodes`, `node_cpus`, `node_memory_bytes`). The two views are kept in
/// sync by [`NumaTopology::detect`] and the constructor helpers below.
#[derive(Debug, Clone)]
pub struct NumaTopology {
    /// Rich per-node info (CPU lists, memory, etc.).
    pub nodes: Vec<NumaNode>,
    /// Number of NUMA nodes (mirrors `nodes.len()`; always >= 1).
    pub total_nodes: usize,
    /// Whether real NUMA hardware was detected (i.e. >1 node).
    pub is_numa_available: bool,

    // ---- Cross-crate contract surface (used by other agents/crates) -------
    /// Number of NUMA nodes (alias for `total_nodes`; always >= 1).
    pub num_nodes: usize,
    /// Per-node CPU lists (CPU indices, as `u32`).
    pub node_cpus: Vec<Vec<u32>>,
    /// Per-node memory size in bytes (best-effort; 0 if unknown).
    pub node_memory_bytes: Vec<u64>,
}

impl NumaTopology {
    /// Detect the NUMA topology from the host OS.
    ///
    /// Per-platform behavior:
    /// * **Linux**: parses `/sys/devices/system/node/node*/cpulist` and
    ///   `/sys/devices/system/node/node*/meminfo`.
    /// * **Windows**: no `windows` crate available in this crate, so falls
    ///   back to a single-node topology sized from `NUMBER_OF_PROCESSORS`.
    /// * **macOS / other**: single-node fallback sized from
    ///   `std::thread::available_parallelism()`.
    pub fn detect() -> Self {
        #[cfg(target_os = "linux")]
        {
            if let Some(t) = detect_linux() {
                return t;
            }
        }
        #[cfg(target_os = "windows")]
        {
            if let Some(t) = detect_windows() {
                return t;
            }
        }
        #[cfg(target_os = "macos")]
        {
            if let Some(t) = detect_macos() {
                return t;
            }
        }
        fallback_single_node()
    }

    /// Construct a topology from raw per-node CPU lists & memory sizes.
    /// Used by the platform detection paths to keep both views in sync.
    fn from_parts(node_cpus: Vec<Vec<u32>>, node_memory_bytes: Vec<u64>) -> Self {
        let num_nodes = node_cpus.len().max(1);
        // If detection somehow produced zero nodes, fall back to single-node.
        let (node_cpus, node_memory_bytes) = if node_cpus.is_empty() {
            (vec![Vec::<u32>::new()], vec![0u64])
        } else {
            (node_cpus, node_memory_bytes)
        };
        let nodes: Vec<NumaNode> = node_cpus
            .iter()
            .enumerate()
            .map(|(idx, cpus)| {
                let mem = node_memory_bytes.get(idx).copied().unwrap_or(0);
                NumaNode {
                    id: idx,
                    cpu_count: cpus.len(),
                    memory_size_bytes: mem,
                    free_memory_bytes: mem, // best-effort; we don't track free separately
                    cpu_ids: cpus.iter().map(|&c| c as usize).collect(),
                }
            })
            .collect();
        let total_nodes = nodes.len();
        NumaTopology {
            nodes,
            total_nodes,
            is_numa_available: total_nodes > 1,
            num_nodes,
            node_cpus,
            node_memory_bytes,
        }
    }

    /// Map a CPU id to the NUMA node that owns it (legacy API).
    pub fn node_for_cpu(&self, cpu_id: usize) -> usize {
        for node in &self.nodes {
            if node.cpu_ids.contains(&cpu_id) {
                return node.id;
            }
        }
        0 // fallback
    }

    /// Map a CPU index to its NUMA node (cross-crate contract API).
    ///
    /// Returns 0 if the CPU is unknown.
    pub fn node_of_cpu(&self, cpu: u32) -> usize {
        for (idx, cpus) in self.node_cpus.iter().enumerate() {
            if cpus.contains(&cpu) {
                return idx;
            }
        }
        0
    }

    /// Return the NUMA node of the calling OS thread (best-effort).
    ///
    /// On Linux this reports the node of the CPU the calling thread last ran
    /// on (`linux_current_cpu_hint`, `cfg`-gated and so not linked). On other
    /// platforms (or on read/parse
    /// failure) it returns 0 — see `detect_windows` and `detect_macos`, which
    /// report a single node, so 0 is also the only valid answer there.
    ///
    /// # Cached per thread, with a bounded reuse window
    ///
    /// This is NOT a cold path. `GenerationalHeap::numa_slow_path_hint` calls
    /// it from the young allocation slow path and from every TLAB refill —
    /// guarded by `numa_num_nodes > 1`, so on a single-node host (every
    /// current CI and dev target) it is never reached, and on a MULTI-SOCKET
    /// host — the only shape where the hint means anything — every one of
    /// those calls used to be a `/proc` read, a line scan and a `Vec<u32>`
    /// allocation. That is orders of magnitude more expensive than the arena
    /// lock it precedes.
    ///
    /// gengc-round1-alloc memoised it PERMANENTLY, and said in as many words
    /// that the premise was "the probe is constant per thread, therefore
    /// caching is free" — true of an affinity mask, and the premise stops
    /// holding the moment the probe reports the RUNNING CPU, which round 2
    /// made it do. So the memo is now a reuse window rather than a memo: the
    /// probe re-runs every `NODE_HINT_REUSE_CALLS` calls (the item is
    /// `cfg`-gated to Linux, hence not linked), which is roughly
    /// once per that many TLAB refills and keeps the `/proc` read off all but
    /// one in a thousand slow paths. A thread that migrates between sockets is
    /// now capable of saying so, which is the entire point — the one consumer
    /// is a `tracing::trace!` in `gen_heap` that could not previously fire at
    /// all, and no correctness is attached to the value (see this module's
    /// header).
    ///
    /// A `thread_local!` rather than a process global: this is per-thread
    /// derived state, and there is no VM-scoped place to hang it (the topology
    /// itself is already a process-wide `OnceLock`).
    pub fn current_thread_node() -> usize {
        #[cfg(target_os = "linux")]
        {
            use std::cell::Cell;
            thread_local! {
                /// `(node, calls still to be served from the cache)`. A zero
                /// second element means "probe now", which is also the
                /// never-asked state, so a fresh thread cannot inherit or
                /// default its way to an answer it never took.
                static NODE: Cell<(usize, u32)> = const { Cell::new((0, 0)) };
            }
            NODE.with(|slot| {
                let (node, left) = slot.get();
                if left > 0 {
                    slot.set((node, left - 1));
                    return node;
                }
                let n = match linux_current_cpu_hint() {
                    Some(cpu) => global_topology().node_of_cpu(cpu),
                    None => 0,
                };
                slot.set((n, NODE_HINT_REUSE_CALLS));
                n
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            0
        }
    }

    /// Determine the preferred node for a given thread id (simple modular mapping).
    pub fn preferred_node_for_thread(&self, thread_id: u64) -> usize {
        if self.total_nodes == 0 {
            return 0;
        }
        (thread_id as usize) % self.total_nodes
    }
}

// ---------------------------------------------------------------------------
// Global cached topology
// ---------------------------------------------------------------------------

/// How many [`NumaTopology::current_thread_node`] calls one probe answers for.
///
/// The probe is a `/proc` read, and the caller is an allocation slow path, so
/// the two constraints are "never once per call" and "not never". 1024 refills
/// at the 256 KiB TLAB baseline is ~256 MB of allocation per probe on a
/// thread that is allocating hard, which is a rate a thread migration is
/// visible at and a cost that is invisible beside it.
///
/// Deliberately a CALL count and not a wall-clock interval: there is no clock
/// on this path, and `Instant::now()` twice per call to decide whether to skip
/// a read would be most of the read.
#[cfg(any(target_os = "linux", test))]
const NODE_HINT_REUSE_CALLS: u32 = 1024;

static GLOBAL_TOPOLOGY: OnceLock<NumaTopology> = OnceLock::new();

/// Return a process-wide cached `NumaTopology`. Detection runs exactly once.
pub fn global_topology() -> &'static NumaTopology {
    GLOBAL_TOPOLOGY.get_or_init(NumaTopology::detect)
}

// ---------------------------------------------------------------------------
// Platform-specific detection paths
// ---------------------------------------------------------------------------

/// Best-effort total CPU count, falling back to 1.
fn available_cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Final fallback: one node owning every visible CPU and `total_ram_bytes()`.
fn fallback_single_node() -> NumaTopology {
    let n = available_cpu_count();
    let cpus: Vec<u32> = (0..n as u32).collect();
    let mem = total_ram_bytes_best_effort();
    NumaTopology::from_parts(vec![cpus], vec![mem])
}

/// Best-effort total RAM in bytes (0 if unknown / can't read).
fn total_ram_bytes_best_effort() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("MemTotal:") {
                    return parse_kb_value(rest).unwrap_or(0);
                }
            }
        }
    }
    0
}

/// Parse a "<number> kB" tail (whitespace-tolerant) into a byte count.
#[cfg(any(target_os = "linux", test))]
fn parse_kb_value(s: &str) -> Option<u64> {
    let trimmed = s.trim();
    let trimmed = trimmed
        .strip_suffix("kB")
        .or_else(|| trimmed.strip_suffix("KB"))
        .unwrap_or(trimmed)
        .trim();
    trimmed.parse::<u64>().ok().map(|kb| kb * 1024)
}

// ---- Linux sysfs parsing --------------------------------------------------

#[cfg(target_os = "linux")]
fn detect_linux() -> Option<NumaTopology> {
    let entries = std::fs::read_dir("/sys/devices/system/node").ok()?;
    let mut nodes: Vec<(usize, Vec<u32>, u64)> = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy().into_owned();
        let Some(id_str) = name.strip_prefix("node") else {
            continue;
        };
        // Require the suffix to be purely numeric to skip files like
        // "node_online" or "node_possible".
        let Ok(node_id) = id_str.parse::<usize>() else {
            continue;
        };
        let cpu_path = entry.path().join("cpulist");
        let mem_path = entry.path().join("meminfo");
        let cpus = std::fs::read_to_string(&cpu_path)
            .ok()
            .and_then(|s| parse_cpulist(s.trim()))
            .unwrap_or_default();
        let mem = std::fs::read_to_string(&mem_path)
            .ok()
            .and_then(|s| parse_node_meminfo(&s, node_id))
            .unwrap_or(0);
        nodes.push((node_id, cpus, mem));
    }
    if nodes.is_empty() {
        return None;
    }
    nodes.sort_by_key(|(id, _, _)| *id);
    // Compact node ids: if Linux exposes only node0,node2 we still produce
    // a contiguous Vec keyed by detection order — node_of_cpu uses indices.
    let node_cpus: Vec<Vec<u32>> = nodes.iter().map(|(_, c, _)| c.clone()).collect();
    let node_mem: Vec<u64> = nodes.iter().map(|(_, _, m)| *m).collect();
    Some(NumaTopology::from_parts(node_cpus, node_mem))
}

/// Parse `/sys/devices/system/node/nodeN/cpulist` content.
///
/// Format examples: `0-3`, `0-7,16-23`, `2`, empty.
fn parse_cpulist(s: &str) -> Option<Vec<u32>> {
    let mut out = Vec::new();
    if s.is_empty() {
        return Some(out);
    }
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((lo, hi)) = part.split_once('-') {
            let lo: u32 = lo.trim().parse().ok()?;
            let hi: u32 = hi.trim().parse().ok()?;
            if hi < lo {
                return None;
            }
            // Guard against a malformed cpulist (e.g. `0-4294967295`)
            // forcing a multi-GB allocation. Real systems never expose
            // more than a few thousand CPUs; reject an absurd range the
            // same way other malformed input is rejected (return None).
            const MAX_CPUS: u32 = 8192;
            if hi - lo >= MAX_CPUS {
                return None;
            }
            for c in lo..=hi {
                out.push(c);
            }
        } else {
            let c: u32 = part.parse().ok()?;
            out.push(c);
        }
    }
    Some(out)
}

/// Parse `Node N MemTotal:       NNN kB` out of a node's meminfo file.
#[cfg(any(target_os = "linux", test))]
fn parse_node_meminfo(content: &str, node_id: usize) -> Option<u64> {
    let needle_prefix = format!("Node {} MemTotal:", node_id);
    for line in content.lines() {
        if let Some(rest) = line.trim_start().strip_prefix(&needle_prefix) {
            return parse_kb_value(rest);
        }
    }
    None
}

/// Field 39 (`processor`) of a `/proc/<tid>/stat` line: the CPU the task last
/// executed on.
///
/// # Why the last `)` and not `split_whitespace().nth(38)`
///
/// Field 2 is `comm`, the executable basename in parentheses, and it may
/// contain spaces AND parentheses -- a Java process routinely has threads named
/// `C2 CompilerThre` and `GC Thread#0`, and `proc(5)` allows worse. Splitting
/// the whole line on whitespace therefore mis-numbers every later field by
/// however many spaces the name holds. The documented parse is to find the LAST
/// `)`: everything after it is fields 3 onwards, unambiguously.
///
/// `processor` is field 39, so it is the 37th of those, index 36.
#[cfg(any(target_os = "linux", test))]
fn parse_stat_processor_field(stat: &str) -> Option<u32> {
    let after_comm = stat.get(stat.rfind(')')? + 1..)?;
    after_comm
        .split_ascii_whitespace()
        .nth(36)?
        .parse::<u32>()
        .ok()
}

/// The CPU the CALLING THREAD last ran on, from `/proc/thread-self/stat`.
///
/// # What this replaces, and why the old answer could not vary
///
/// This used to parse `/proc/self/status`'s `Cpus_allowed_list:` and take the
/// first entry. That is the AFFINITY MASK, which on an unpinned process is
/// every CPU on the machine -- so the answer was CPU 0, for every thread, on
/// every call, forever, and `node_of_cpu(0)` is node 0. A thread actually
/// running on socket 1 was told it was on node 0, and `gen_heap`'s
/// cross-node `tracing::trace!` -- the one instrument that would have said
/// "this heap is being allocated into from a foreign node" -- was unfireable by
/// construction. See
/// `docs/internal/gc/gengc-alloc-numa-hint-cannot-vary-RETIRED-20260923.md`.
///
/// `/proc/thread-self` (Linux 3.17+) is the calling THREAD's directory.
/// `/proc/self` is the thread-group leader's, whose `processor` field says
/// nothing about any other thread -- reading it would swap one
/// same-answer-for-everyone bug for another, so a kernel too old to have
/// `thread-self` gets `None` (node 0, the documented fallback) rather than a
/// confidently wrong number.
///
/// # `sched_getcpu` first (gc-common w1-f)
///
/// This doc used to call `sched_getcpu()` "the right call" and rule it out
/// because it seemed to need a `libc` dependency, which `gc/` avoids by
/// design. It does not need one. glibc (2.6 and later) and musl both export
/// the symbol, and a bare `extern "C"` declaration reaches it, the same way
/// `crate::reservation` declares `mmap` / `madvise` / `mprotect`. It is a
/// vDSO read of the running CPU, with no file open and no allocation. The
/// `/proc/thread-self/stat` parse remains only as the fallback for a libc that
/// answers -1 (`ENOSYS`).
/// [`NumaTopology::current_thread_node`]'s reuse window stays, so the probe
/// runs once per window whichever arm answers.
#[cfg(target_os = "linux")]
fn linux_current_cpu_hint() -> Option<u32> {
    extern "C" {
        fn sched_getcpu() -> i32;
    }
    // SAFETY: no arguments and no caller memory; returns -1 when unsupported.
    let cpu = unsafe { sched_getcpu() };
    if let Ok(cpu) = u32::try_from(cpu) {
        return Some(cpu);
    }
    let stat = std::fs::read_to_string("/proc/thread-self/stat").ok()?;
    parse_stat_processor_field(&stat)
}

// ---- Windows fallback -----------------------------------------------------

#[cfg(target_os = "windows")]
fn detect_windows() -> Option<NumaTopology> {
    // TODO: when the `windows` crate becomes a dependency of `gc/`, switch
    // to `GetLogicalProcessorInformationEx(RelationNumaNode, ...)` here.
    // For now: single node sized from NUMBER_OF_PROCESSORS, with all CPUs.
    let n = cratonvm_types::flags::runtime_var("NUMBER_OF_PROCESSORS")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or_else(available_cpu_count)
        .max(1);
    let cpus: Vec<u32> = (0..n as u32).collect();
    Some(NumaTopology::from_parts(vec![cpus], vec![0]))
}

// ---- macOS fallback -------------------------------------------------------

#[cfg(target_os = "macos")]
fn detect_macos() -> Option<NumaTopology> {
    // macOS doesn't expose NUMA APIs directly. Without the `libc` crate as
    // a dependency we can't call `sysctlbyname("hw.packages", ...)` from
    // here either; rather than guess we fall through to the single-node
    // fallback shape, which is accurate for virtually all current Macs.
    let n = available_cpu_count();
    let cpus: Vec<u32> = (0..n as u32).collect();
    Some(NumaTopology::from_parts(vec![cpus], vec![0]))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- NUMA topology tests ----

    #[test]
    fn test_detect_topology() {
        let topo = NumaTopology::detect();
        // Real detection: at least one node, with internal views consistent.
        assert!(topo.total_nodes >= 1);
        assert_eq!(topo.nodes.len(), topo.total_nodes);
        assert_eq!(topo.num_nodes, topo.total_nodes);
        assert_eq!(topo.node_cpus.len(), topo.total_nodes);
        assert_eq!(topo.node_memory_bytes.len(), topo.total_nodes);
        assert_eq!(topo.is_numa_available, topo.total_nodes > 1);
    }

    #[test]
    fn detect_returns_at_least_one_node() {
        let topo = NumaTopology::detect();
        assert!(topo.num_nodes >= 1, "expected >= 1 NUMA node");
        assert!(!topo.node_cpus.is_empty());
        // Across all nodes, at least one CPU must be visible somewhere.
        let total_cpus: usize = topo.node_cpus.iter().map(|c| c.len()).sum();
        assert!(
            total_cpus >= 1,
            "expected at least one CPU across all nodes"
        );
    }

    #[test]
    fn current_thread_node_is_valid_index() {
        let topo = global_topology();
        let node = NumaTopology::current_thread_node();
        assert!(
            node < topo.num_nodes,
            "current_thread_node returned {} but only {} nodes exist",
            node,
            topo.num_nodes
        );
    }

    /// gengc-round1-alloc / round2-alloc2, 2026-09-20 — the per-thread cache.
    ///
    /// `numa_slow_path_hint` calls this from the young allocation slow path on
    /// a multi-node host, so it must be a cheap answer rather than a `/proc`
    /// read per allocation. Round 1 cached it forever on the premise that the
    /// probe (an affinity mask) was constant per thread; round 2 changed the
    /// probe to the RUNNING CPU, so the guarantee is now weaker and stated as
    /// such: the answer is stable for at least `NODE_HINT_REUSE_CALLS` calls
    /// and is always a VALID node index, which is all any caller may rely on.
    ///
    /// The second thread is the part that is unchanged and still load-bearing:
    /// the cache is per-thread, so a fresh thread must take its own probe
    /// rather than inherit a stale answer or read an empty cell as node 0 when
    /// the topology says otherwise.
    #[test]
    fn current_thread_node_is_cached_per_thread_and_always_valid() {
        let nodes = global_topology().num_nodes;
        let first = NumaTopology::current_thread_node();
        assert!(first < nodes);
        for _ in 0..64 {
            assert_eq!(
                NumaTopology::current_thread_node(),
                first,
                "well inside one reuse window, the hint must not re-probe",
            );
        }
        // Past the window: the value may legitimately differ on a host where
        // the scheduler moved this thread, so the assertion is on VALIDITY,
        // not on equality. Asserting equality here would be asserting that the
        // probe cannot vary, which is the defect round 2 removed.
        for _ in 0..(super::NODE_HINT_REUSE_CALLS as usize * 2) {
            assert!(NumaTopology::current_thread_node() < nodes);
        }
        let other = std::thread::spawn(NumaTopology::current_thread_node)
            .join()
            .expect("hint thread must not panic");
        assert!(other < nodes, "a fresh thread's hint must be a valid index");
    }

    /// gengc-round2-alloc2, 2026-09-20 — `/proc/<tid>/stat` field 39.
    ///
    /// The parser is tested and the `/proc` read is not, deliberately: the
    /// read only works on Linux and this suite's usual host is Windows, so a
    /// `#[cfg(target_os = "linux")]` test would be a check that never runs on
    /// the machine where the code is written. The parsing is where the bug
    /// would be, and it is a pure function of a string.
    ///
    /// The two cases below are the ones that break the obvious
    /// implementation: a `comm` with a SPACE in it (`GC Thread#0`,
    /// `C2 CompilerThre` — routine for a JVM) shifts every field, and one with
    /// a `)` in it defeats `find(')')`. `proc(5)` allows both, which is why
    /// the documented parse is "everything after the LAST `)`".
    #[test]
    fn the_stat_processor_field_survives_a_comm_with_spaces_and_parens() {
        // Fields 3..38 as 36 placeholders, then `processor` = 7, then the
        // fields that follow it in modern kernels.
        let tail = format!("{} 7 0 0 0 0 0", "0 ".repeat(36).trim());
        assert_eq!(
            super::parse_stat_processor_field(&format!("4242 (java) {tail}")),
            Some(7)
        );
        assert_eq!(
            super::parse_stat_processor_field(&format!("4242 (GC Thread#0) {tail}")),
            Some(7),
            "a comm with a space must not shift the field index"
        );
        assert_eq!(
            super::parse_stat_processor_field(&format!("4242 (we(i)rd name) {tail}")),
            Some(7),
            "the split point is the LAST ')', not the first"
        );
        // Truncated or malformed input is `None`, never a wrong CPU: the
        // caller maps `None` to node 0, which is at least an honest default.
        assert_eq!(super::parse_stat_processor_field("4242 (java) 0 0 0"), None);
        assert_eq!(super::parse_stat_processor_field("no parens here"), None);
        assert_eq!(super::parse_stat_processor_field(""), None);
    }

    #[test]
    fn node_of_cpu_dispatches_correctly() {
        let topo = NumaTopology::detect();
        // Every CPU listed for a node must round-trip to that node's index.
        for (node_idx, cpus) in topo.node_cpus.iter().enumerate() {
            for &cpu in cpus {
                assert_eq!(
                    topo.node_of_cpu(cpu),
                    node_idx,
                    "cpu {} should map to node {}",
                    cpu,
                    node_idx
                );
            }
        }
        // Unknown CPUs fall back to node 0.
        assert_eq!(topo.node_of_cpu(u32::MAX), 0);
    }

    #[test]
    fn test_global_topology_is_cached() {
        let t1 = global_topology();
        let t2 = global_topology();
        // OnceLock guarantees pointer stability.
        assert!(std::ptr::eq(t1, t2));
    }

    #[test]
    fn test_node_for_cpu_found() {
        let topo = NumaTopology::detect();
        // CPU 0 always belongs to *some* node; with our layout it's index 0.
        assert_eq!(topo.node_for_cpu(0), 0);
    }

    #[test]
    fn test_node_for_cpu_fallback() {
        let topo = NumaTopology::detect();
        // CPU 9999 should not exist — should fall back to node 0
        assert_eq!(topo.node_for_cpu(9999), 0);
    }

    #[test]
    fn test_preferred_node_single() {
        // Synthetic single-node topology to keep this test deterministic
        // regardless of the host hardware.
        let topo = NumaTopology::from_parts(vec![vec![0, 1]], vec![0]);
        assert_eq!(topo.preferred_node_for_thread(0), 0);
        assert_eq!(topo.preferred_node_for_thread(7), 0);
    }

    #[test]
    fn test_parse_cpulist_simple() {
        assert_eq!(parse_cpulist("0-3").unwrap(), vec![0, 1, 2, 3]);
        assert_eq!(parse_cpulist("5").unwrap(), vec![5]);
        assert_eq!(parse_cpulist("0-1,4-5").unwrap(), vec![0, 1, 4, 5]);
        assert_eq!(parse_cpulist("").unwrap(), Vec::<u32>::new());
    }

    #[test]
    fn test_parse_cpulist_bad() {
        assert!(parse_cpulist("garbage").is_none());
        assert!(parse_cpulist("5-3").is_none());
    }

    #[test]
    fn test_parse_kb_value() {
        assert_eq!(parse_kb_value("       1024 kB"), Some(1024 * 1024));
        assert_eq!(parse_kb_value("0 kB"), Some(0));
        assert_eq!(parse_kb_value("nonsense"), None);
    }

    #[test]
    fn test_parse_node_meminfo() {
        let sample = "\
Node 0 MemTotal:       16777216 kB
Node 0 MemFree:        12345678 kB
Node 0 MemUsed:         4431538 kB
";
        assert_eq!(parse_node_meminfo(sample, 0), Some(16777216u64 * 1024));
        assert_eq!(parse_node_meminfo(sample, 1), None);
    }

    /// Synthetic 2-node topology used by several tests below.
    fn synthetic_two_node() -> NumaTopology {
        NumaTopology::from_parts(vec![vec![0, 1], vec![2, 3]], vec![4 << 30, 4 << 30])
    }

    #[test]
    fn test_preferred_node_multi() {
        let topo = synthetic_two_node();
        assert_eq!(topo.preferred_node_for_thread(0), 0);
        assert_eq!(topo.preferred_node_for_thread(1), 1);
        assert_eq!(topo.preferred_node_for_thread(2), 0);
    }

    #[test]
    fn test_numa_node_properties() {
        let topo = NumaTopology::detect();
        let node = &topo.nodes[0];
        // Real detection: shape is host-dependent; just sanity-check
        // internal consistency.
        assert_eq!(node.cpu_count, node.cpu_ids.len());
        assert!(node.free_memory_bytes <= node.memory_size_bytes);
    }

    #[test]
    fn test_node_for_cpu_multi_node() {
        let topo = synthetic_two_node();
        assert_eq!(topo.node_for_cpu(2), 1);
        assert_eq!(topo.node_for_cpu(3), 1);
    }
}
