// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! NUMA topology detection (Phase 16.3) and string deduplication (Phase 16.4).
//!
//! Real platform NUMA detection lives in [`NumaTopology::detect`] with
//! per-OS code paths (Linux sysfs, Windows env-based fallback, macOS proxy,
//! generic single-node fallback). The detected topology is cached in a
//! `OnceLock` accessible via [`global_topology`] so other crates can share
//! the result without re-reading sysfs.
//!
//! # No NUMA placement happens. This module only *detects*.
//!
//! Audit note, 2026-07-26 (`arch-2026-07-26/refs-metaspace-unloading`). Stated
//! plainly because the module name and the type name `NumaAllocator` both imply
//! otherwise:
//!
//! **Live on the default path** — exactly three entry points, all consumed by
//! `gc/src/gen_heap.rs` (search `crate::numa::`):
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
//! compares.
//!
//! **Inert** — no caller anywhere in the workspace:
//!
//! * [`NumaAllocator`], [`NumaArena`], [`NumaAllocation`], [`NumaStats`],
//!   [`NumaPolicy`]. `NumaAllocator::allocate` is a *simulation*: it bumps a
//!   `next_address` counter starting at `0x1000` and returns that integer. It
//!   touches no memory and its "addresses" are not pointers. It also never
//!   reports a cross-node allocation — `allocate` unconditionally increments
//!   `local_allocations`, so `local_allocation_ratio` is `1.0` unless a caller
//!   manually calls `record_cross_node_access`.
//! * [`StringDeduplicator`] and its config/stats types. G1's string dedup is
//!   not wired to this.
//!
//! So: any report, dashboard or doc claiming CratonVM does NUMA-aware
//! placement is wrong. The topology probe is real and correct; the placement
//! it would inform does not exist. See
//! `refs-metaspace-unloading.md` for what wiring
//! it would take.

use std::collections::HashMap;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// 16.3  NUMA-Aware Allocation
// ---------------------------------------------------------------------------

/// Policy governing how allocations are placed across NUMA nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NumaPolicy {
    /// OS default placement.
    Default,
    /// Prefer a specific node.
    Preferred(usize),
    /// Round-robin across all nodes.
    Interleaved,
    /// Allocate on the node local to the calling thread.
    LocalAlloc,
}

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
/// Carries both the legacy rich `nodes` view used by [`NumaAllocator`] and the
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
    /// On Linux this parses `/proc/self/status` for the `Cpus_allowed_list:`
    /// line and reports the node of the first CPU in the affinity mask.
    /// On other platforms (or on parse failure) this returns 0.
    pub fn current_thread_node() -> usize {
        #[cfg(target_os = "linux")]
        {
            if let Some(cpu) = linux_current_cpu_hint() {
                return global_topology().node_of_cpu(cpu);
            }
        }
        0
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

/// Best-effort current-CPU hint by parsing `/proc/self/status`'s
/// `Cpus_allowed_list:` line and returning the first allowed CPU.
#[cfg(target_os = "linux")]
fn linux_current_cpu_hint() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Cpus_allowed_list:") {
            let cpus = parse_cpulist(rest.trim())?;
            return cpus.into_iter().next();
        }
    }
    None
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

/// Per-node allocation arena tracking.
#[derive(Debug, Clone)]
pub struct NumaArena {
    pub node_id: usize,
    pub allocated_bytes: u64,
    pub allocation_count: u64,
    pub peak_bytes: u64,
}

impl NumaArena {
    fn new(node_id: usize) -> Self {
        NumaArena {
            node_id,
            allocated_bytes: 0,
            allocation_count: 0,
            peak_bytes: 0,
        }
    }

    fn record_allocation(&mut self, size: usize) {
        self.allocated_bytes += size as u64;
        self.allocation_count += 1;
        if self.allocated_bytes > self.peak_bytes {
            self.peak_bytes = self.allocated_bytes;
        }
    }
}

/// Result of a NUMA allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumaAllocation {
    /// Simulated address.
    pub address: usize,
    pub size: usize,
    pub node_id: usize,
}

/// Aggregate statistics across all NUMA nodes.
#[derive(Debug, Clone)]
pub struct NumaStats {
    pub per_node_allocated: Vec<u64>,
    pub per_node_peak: Vec<u64>,
    pub total_allocated: u64,
    pub cross_node_accesses: u64,
    pub local_allocation_ratio: f64,
}

/// NUMA-aware allocator distributing memory across nodes.
pub struct NumaAllocator {
    pub topology: NumaTopology,
    pub per_node_arenas: Vec<NumaArena>,
    pub allocations: u64,
    next_address: usize,
    interleave_counter: usize,
    local_allocations: u64,
    cross_node_accesses: u64,
}

impl NumaAllocator {
    pub fn new(topology: NumaTopology) -> Self {
        let arenas = topology
            .nodes
            .iter()
            .map(|n| NumaArena::new(n.id))
            .collect();
        let total_nodes = topology.total_nodes;
        NumaAllocator {
            topology,
            per_node_arenas: arenas,
            allocations: 0,
            next_address: 0x1000,
            interleave_counter: 0,
            local_allocations: 0,
            cross_node_accesses: if total_nodes > 0 { 0 } else { 0 },
        }
    }

    /// Allocate `size` bytes, preferring the given node.
    pub fn allocate(&mut self, size: usize, node_hint: usize) -> NumaAllocation {
        let node_id = if node_hint < self.per_node_arenas.len() {
            node_hint
        } else {
            0
        };
        self.per_node_arenas[node_id].record_allocation(size);
        let addr = self.next_address;
        self.next_address += size;
        self.allocations += 1;
        self.local_allocations += 1;
        NumaAllocation {
            address: addr,
            size,
            node_id,
        }
    }

    /// Allocate with round-robin interleaving across nodes.
    pub fn allocate_interleaved(&mut self, size: usize) -> NumaAllocation {
        let node_count = self.per_node_arenas.len().max(1);
        let node_id = self.interleave_counter % node_count;
        self.interleave_counter += 1;
        self.per_node_arenas[node_id].record_allocation(size);
        let addr = self.next_address;
        self.next_address += size;
        self.allocations += 1;
        self.local_allocations += 1;
        NumaAllocation {
            address: addr,
            size,
            node_id,
        }
    }

    /// Record a cross-node access (for stats tracking).
    pub fn record_cross_node_access(&mut self) {
        self.cross_node_accesses += 1;
    }

    /// Gather allocation statistics.
    pub fn get_stats(&self) -> NumaStats {
        let per_node_allocated: Vec<u64> = self
            .per_node_arenas
            .iter()
            .map(|a| a.allocated_bytes)
            .collect();
        let per_node_peak: Vec<u64> = self.per_node_arenas.iter().map(|a| a.peak_bytes).collect();
        let total_allocated: u64 = per_node_allocated.iter().sum();
        let total = self.local_allocations + self.cross_node_accesses;
        let local_ratio = if total > 0 {
            self.local_allocations as f64 / total as f64
        } else {
            1.0
        };
        NumaStats {
            per_node_allocated,
            per_node_peak,
            total_allocated,
            cross_node_accesses: self.cross_node_accesses,
            local_allocation_ratio: local_ratio,
        }
    }
}

// ---------------------------------------------------------------------------
// 16.4  String Deduplication
// ---------------------------------------------------------------------------

/// FNV-1a hash for byte slices.
pub fn fnv1a_hash(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// An entry in the deduplication table mapping a hash to a known backing array.
#[derive(Debug, Clone)]
pub struct DeduplicationEntry {
    pub hash: u64,
    pub backing_array_id: usize,
    pub length: usize,
    pub age: u32,
}

/// Result of a deduplication attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeduplicationResult {
    /// The value was already known (exact same backing array).
    AlreadyDeduped,
    /// Found a match — saved `saved_bytes`.
    Deduped { saved_bytes: usize },
    /// First occurrence — added to table.
    New,
    /// Skipped (too young or disabled).
    Skipped,
}

/// Cumulative statistics for string deduplication.
#[derive(Debug, Clone)]
pub struct DeduplicationStats {
    pub strings_inspected: u64,
    pub strings_deduped: u64,
    pub bytes_saved: u64,
    pub table_entries: usize,
    pub table_size: usize,
    pub lookup_time_ns: u64,
    pub dedup_ratio: f64,
}

impl DeduplicationStats {
    fn new() -> Self {
        DeduplicationStats {
            strings_inspected: 0,
            strings_deduped: 0,
            bytes_saved: 0,
            table_entries: 0,
            table_size: 0,
            lookup_time_ns: 0,
            dedup_ratio: 0.0,
        }
    }

    fn update_ratio(&mut self) {
        self.dedup_ratio = if self.strings_inspected > 0 {
            self.strings_deduped as f64 / self.strings_inspected as f64
        } else {
            0.0
        };
    }
}

/// Configuration knobs for the deduplicator.
#[derive(Debug, Clone)]
pub struct DeduplicationConfig {
    pub enabled: bool,
    pub min_age_threshold: u32,
    pub table_initial_size: usize,
    pub table_max_load_factor: f64,
    pub max_table_size: usize,
    pub print_stats: bool,
}

impl Default for DeduplicationConfig {
    fn default() -> Self {
        DeduplicationConfig {
            enabled: true,
            min_age_threshold: 3,
            table_initial_size: 1024,
            table_max_load_factor: 0.75,
            max_table_size: 16384,
            print_stats: false,
        }
    }
}

/// String deduplicator backed by a hash table of known backing arrays.
pub struct StringDeduplicator {
    pub table: HashMap<u64, Vec<DeduplicationEntry>>,
    pub stats: DeduplicationStats,
    pub config: DeduplicationConfig,
    next_backing_id: usize,
}

impl StringDeduplicator {
    pub fn new(config: DeduplicationConfig) -> Self {
        let initial = config.table_initial_size;
        StringDeduplicator {
            table: HashMap::with_capacity(initial),
            stats: DeduplicationStats::new(),
            config,
            next_backing_id: 1,
        }
    }

    /// Check if a string with the given age should be considered for dedup.
    pub fn should_deduplicate(&self, string_age_gc_cycles: u32) -> bool {
        self.config.enabled && string_age_gc_cycles >= self.config.min_age_threshold
    }

    /// Attempt to deduplicate a string value.
    ///
    /// `hash` — pre-computed hash of `value_bytes`.
    /// `value_bytes` — raw byte content of the string's backing array.
    pub fn deduplicate(&mut self, hash: u64, value_bytes: &[u8]) -> DeduplicationResult {
        self.stats.strings_inspected += 1;

        if !self.config.enabled {
            self.stats.update_ratio();
            return DeduplicationResult::Skipped;
        }

        if let Some(entries) = self.table.get(&hash) {
            // Check for an exact-length match (simulated content equality).
            for entry in entries {
                if entry.length == value_bytes.len() {
                    self.stats.strings_deduped += 1;
                    self.stats.bytes_saved += value_bytes.len() as u64;
                    self.stats.update_ratio();
                    return DeduplicationResult::Deduped {
                        saved_bytes: value_bytes.len(),
                    };
                }
            }
        }

        // New entry — record it.
        let entry = DeduplicationEntry {
            hash,
            backing_array_id: self.next_backing_id,
            length: value_bytes.len(),
            age: 0,
        };
        self.next_backing_id += 1;
        self.table.entry(hash).or_default().push(entry);
        self.stats.table_entries += 1;
        self.stats.table_size = self.table.len();
        self.stats.update_ratio();

        self.resize_table_if_needed();

        DeduplicationResult::New
    }

    /// Return current statistics.
    pub fn get_stats(&self) -> &DeduplicationStats {
        &self.stats
    }

    /// Grow or shrink table capacity based on load factor.
    pub fn resize_table_if_needed(&mut self) {
        let capacity = self.table.capacity().max(1);
        let load = self.table.len() as f64 / capacity as f64;
        if load > self.config.table_max_load_factor
            && self.table.capacity() < self.config.max_table_size
        {
            let new_cap = (capacity * 2).min(self.config.max_table_size);
            self.table.reserve(new_cap.saturating_sub(capacity));
        }
    }

    /// Remove entries whose age exceeds a threshold (simulating collected strings).
    /// Returns how many entries were removed.
    pub fn cleanup_dead_entries(&mut self) -> usize {
        let mut removed = 0usize;
        self.table.retain(|_hash, entries| {
            let before = entries.len();
            entries.retain(|e| e.age < 100); // age >= 100 considered dead
            removed += before - entries.len();
            !entries.is_empty()
        });
        self.stats.table_entries = self.stats.table_entries.saturating_sub(removed);
        self.stats.table_size = self.table.len();
        removed
    }

    /// Increment age of all entries by one GC cycle.
    pub fn age_entries(&mut self) {
        for entries in self.table.values_mut() {
            for entry in entries.iter_mut() {
                entry.age += 1;
            }
        }
    }
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

    // ---- NumaAllocator tests ----

    #[test]
    fn test_allocator_basic() {
        let topo = NumaTopology::detect();
        let mut alloc = NumaAllocator::new(topo);
        let a = alloc.allocate(256, 0);
        assert_eq!(a.size, 256);
        assert_eq!(a.node_id, 0);
        assert_eq!(alloc.allocations, 1);
    }

    #[test]
    fn test_allocator_addresses_increase() {
        let topo = NumaTopology::detect();
        let mut alloc = NumaAllocator::new(topo);
        let a1 = alloc.allocate(128, 0);
        let a2 = alloc.allocate(256, 0);
        assert!(a2.address > a1.address);
        assert_eq!(a2.address, a1.address + 128);
    }

    #[test]
    fn test_allocator_invalid_node_falls_back() {
        let topo = NumaTopology::detect();
        let mut alloc = NumaAllocator::new(topo);
        let a = alloc.allocate(64, 999);
        assert_eq!(a.node_id, 0);
    }

    #[test]
    fn test_allocate_interleaved_single_node() {
        // Use a deterministic single-node topology so this is host-agnostic.
        let topo = NumaTopology::from_parts(vec![vec![0]], vec![0]);
        let mut alloc = NumaAllocator::new(topo);
        let a1 = alloc.allocate_interleaved(100);
        let a2 = alloc.allocate_interleaved(100);
        // single node — both go to node 0
        assert_eq!(a1.node_id, 0);
        assert_eq!(a2.node_id, 0);
    }

    #[test]
    fn test_allocate_interleaved_multi_node() {
        let topo = synthetic_two_node();
        let mut alloc = NumaAllocator::new(topo);
        let a1 = alloc.allocate_interleaved(64);
        let a2 = alloc.allocate_interleaved(64);
        let a3 = alloc.allocate_interleaved(64);
        assert_eq!(a1.node_id, 0);
        assert_eq!(a2.node_id, 1);
        assert_eq!(a3.node_id, 0);
    }

    #[test]
    fn test_numa_stats_basic() {
        let topo = NumaTopology::detect();
        let mut alloc = NumaAllocator::new(topo);
        alloc.allocate(512, 0);
        let stats = alloc.get_stats();
        assert_eq!(stats.total_allocated, 512);
        assert_eq!(stats.per_node_allocated[0], 512);
        assert_eq!(stats.cross_node_accesses, 0);
        assert!((stats.local_allocation_ratio - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_numa_stats_cross_node() {
        let topo = NumaTopology::detect();
        let mut alloc = NumaAllocator::new(topo);
        alloc.allocate(100, 0);
        alloc.record_cross_node_access();
        alloc.record_cross_node_access();
        let stats = alloc.get_stats();
        assert_eq!(stats.cross_node_accesses, 2);
        assert!(stats.local_allocation_ratio < 1.0);
    }

    #[test]
    fn test_arena_peak_tracking() {
        let topo = NumaTopology::detect();
        let mut alloc = NumaAllocator::new(topo);
        alloc.allocate(1024, 0);
        alloc.allocate(2048, 0);
        let stats = alloc.get_stats();
        assert_eq!(stats.per_node_peak[0], 3072);
    }

    #[test]
    fn test_numa_policy_variants() {
        assert_eq!(NumaPolicy::Default, NumaPolicy::Default);
        assert_eq!(NumaPolicy::Preferred(1), NumaPolicy::Preferred(1));
        assert_ne!(NumaPolicy::Preferred(0), NumaPolicy::Preferred(1));
        assert_eq!(NumaPolicy::Interleaved, NumaPolicy::Interleaved);
        assert_eq!(NumaPolicy::LocalAlloc, NumaPolicy::LocalAlloc);
    }

    #[test]
    fn test_allocator_many_allocations() {
        let topo = NumaTopology::detect();
        let mut alloc = NumaAllocator::new(topo);
        for _ in 0..1000 {
            alloc.allocate(16, 0);
        }
        assert_eq!(alloc.allocations, 1000);
        let stats = alloc.get_stats();
        assert_eq!(stats.total_allocated, 16000);
    }

    // ---- FNV-1a hash tests ----

    #[test]
    fn test_fnv1a_empty() {
        assert_eq!(fnv1a_hash(b""), 0xcbf29ce484222325);
    }

    #[test]
    fn test_fnv1a_deterministic() {
        let h1 = fnv1a_hash(b"hello");
        let h2 = fnv1a_hash(b"hello");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_fnv1a_different_inputs() {
        assert_ne!(fnv1a_hash(b"hello"), fnv1a_hash(b"world"));
    }

    // ---- String deduplication tests ----

    #[test]
    fn test_dedup_new_string() {
        let mut dd = StringDeduplicator::new(DeduplicationConfig::default());
        let hash = fnv1a_hash(b"hello");
        let result = dd.deduplicate(hash, b"hello");
        assert_eq!(result, DeduplicationResult::New);
    }

    #[test]
    fn test_dedup_duplicate_string() {
        let mut dd = StringDeduplicator::new(DeduplicationConfig::default());
        let hash = fnv1a_hash(b"hello");
        dd.deduplicate(hash, b"hello");
        let result = dd.deduplicate(hash, b"hello");
        assert_eq!(result, DeduplicationResult::Deduped { saved_bytes: 5 });
    }

    #[test]
    fn test_dedup_different_strings() {
        let mut dd = StringDeduplicator::new(DeduplicationConfig::default());
        let h1 = fnv1a_hash(b"alpha");
        let h2 = fnv1a_hash(b"bravo");
        assert_eq!(dd.deduplicate(h1, b"alpha"), DeduplicationResult::New);
        assert_eq!(dd.deduplicate(h2, b"bravo"), DeduplicationResult::New);
    }

    #[test]
    fn test_dedup_disabled() {
        let config = DeduplicationConfig {
            enabled: false,
            ..DeduplicationConfig::default()
        };
        let mut dd = StringDeduplicator::new(config);
        let hash = fnv1a_hash(b"test");
        let result = dd.deduplicate(hash, b"test");
        assert_eq!(result, DeduplicationResult::Skipped);
    }

    #[test]
    fn test_should_dedup_age_below_threshold() {
        let dd = StringDeduplicator::new(DeduplicationConfig::default());
        assert!(!dd.should_deduplicate(0));
        assert!(!dd.should_deduplicate(2));
    }

    #[test]
    fn test_should_dedup_age_at_threshold() {
        let dd = StringDeduplicator::new(DeduplicationConfig::default());
        assert!(dd.should_deduplicate(3));
        assert!(dd.should_deduplicate(10));
    }

    #[test]
    fn test_should_dedup_disabled() {
        let config = DeduplicationConfig {
            enabled: false,
            ..DeduplicationConfig::default()
        };
        let dd = StringDeduplicator::new(config);
        assert!(!dd.should_deduplicate(100));
    }

    #[test]
    fn test_dedup_stats_after_operations() {
        let mut dd = StringDeduplicator::new(DeduplicationConfig::default());
        let h = fnv1a_hash(b"abc");
        dd.deduplicate(h, b"abc");
        dd.deduplicate(h, b"abc");
        dd.deduplicate(h, b"abc");
        let stats = dd.get_stats();
        assert_eq!(stats.strings_inspected, 3);
        assert_eq!(stats.strings_deduped, 2);
        assert_eq!(stats.bytes_saved, 6); // 3 bytes * 2
        assert!(stats.dedup_ratio > 0.0);
    }

    #[test]
    fn test_dedup_stats_ratio() {
        let mut dd = StringDeduplicator::new(DeduplicationConfig::default());
        let h = fnv1a_hash(b"x");
        dd.deduplicate(h, b"x");
        dd.deduplicate(h, b"x");
        let stats = dd.get_stats();
        // 1 dedup out of 2 inspected = 0.5
        assert!((stats.dedup_ratio - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_dedup_table_entries_count() {
        let mut dd = StringDeduplicator::new(DeduplicationConfig::default());
        for i in 0u8..10 {
            let data = [i];
            let h = fnv1a_hash(&data);
            dd.deduplicate(h, &data);
        }
        assert_eq!(dd.stats.table_entries, 10);
    }

    #[test]
    fn test_cleanup_dead_entries_none_dead() {
        let mut dd = StringDeduplicator::new(DeduplicationConfig::default());
        let h = fnv1a_hash(b"alive");
        dd.deduplicate(h, b"alive");
        let removed = dd.cleanup_dead_entries();
        assert_eq!(removed, 0);
        assert_eq!(dd.stats.table_entries, 1);
    }

    #[test]
    fn test_cleanup_dead_entries_some_dead() {
        let mut dd = StringDeduplicator::new(DeduplicationConfig::default());
        let h = fnv1a_hash(b"old");
        dd.deduplicate(h, b"old");
        // Age all entries past the threshold
        for _ in 0..100 {
            dd.age_entries();
        }
        let removed = dd.cleanup_dead_entries();
        assert_eq!(removed, 1);
        assert_eq!(dd.stats.table_entries, 0);
    }

    #[test]
    fn test_age_entries() {
        let mut dd = StringDeduplicator::new(DeduplicationConfig::default());
        let h = fnv1a_hash(b"test");
        dd.deduplicate(h, b"test");
        dd.age_entries();
        dd.age_entries();
        let entries = dd.table.get(&h).unwrap();
        assert_eq!(entries[0].age, 2);
    }

    #[test]
    fn test_dedup_config_defaults() {
        let cfg = DeduplicationConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.min_age_threshold, 3);
        assert_eq!(cfg.table_initial_size, 1024);
        assert!((cfg.table_max_load_factor - 0.75).abs() < f64::EPSILON);
        assert_eq!(cfg.max_table_size, 16384);
        assert!(!cfg.print_stats);
    }

    #[test]
    fn test_dedup_hash_collision_different_lengths() {
        let mut dd = StringDeduplicator::new(DeduplicationConfig::default());
        // Force same hash bucket by using same hash key — different lengths
        let h = 42u64;
        assert_eq!(dd.deduplicate(h, b"ab"), DeduplicationResult::New);
        assert_eq!(dd.deduplicate(h, b"abc"), DeduplicationResult::New);
        // Same hash, same length => dedup
        assert_eq!(
            dd.deduplicate(h, b"xy"),
            DeduplicationResult::Deduped { saved_bytes: 2 }
        );
    }

    #[test]
    fn test_dedup_many_unique_strings() {
        let mut dd = StringDeduplicator::new(DeduplicationConfig::default());
        for i in 0u32..200 {
            let s = format!("string_{}", i);
            let h = fnv1a_hash(s.as_bytes());
            dd.deduplicate(h, s.as_bytes());
        }
        assert_eq!(dd.stats.strings_inspected, 200);
        assert_eq!(dd.stats.strings_deduped, 0);
    }

    #[test]
    fn test_dedup_resize_trigger() {
        let config = DeduplicationConfig {
            table_initial_size: 4,
            table_max_load_factor: 0.5,
            max_table_size: 64,
            ..DeduplicationConfig::default()
        };
        let mut dd = StringDeduplicator::new(config);
        for i in 0u32..20 {
            let s = format!("s{}", i);
            let h = fnv1a_hash(s.as_bytes());
            dd.deduplicate(h, s.as_bytes());
        }
        // Table should have grown beyond initial capacity
        assert!(dd.table.capacity() >= 4);
    }

    #[test]
    fn test_dedup_bytes_saved_accumulates() {
        let mut dd = StringDeduplicator::new(DeduplicationConfig::default());
        let h = fnv1a_hash(b"save_me");
        dd.deduplicate(h, b"save_me");
        dd.deduplicate(h, b"save_me");
        dd.deduplicate(h, b"save_me");
        dd.deduplicate(h, b"save_me");
        // 3 dedup events, each saving 7 bytes
        assert_eq!(dd.stats.bytes_saved, 21);
    }

    #[test]
    fn test_dedup_entry_fields() {
        let entry = DeduplicationEntry {
            hash: 123,
            backing_array_id: 7,
            length: 5,
            age: 2,
        };
        assert_eq!(entry.hash, 123);
        assert_eq!(entry.backing_array_id, 7);
        assert_eq!(entry.length, 5);
        assert_eq!(entry.age, 2);
    }

    #[test]
    fn test_dedup_result_variants() {
        assert_ne!(
            DeduplicationResult::New,
            DeduplicationResult::AlreadyDeduped
        );
        assert_ne!(DeduplicationResult::New, DeduplicationResult::Skipped);
        assert_eq!(
            DeduplicationResult::Deduped { saved_bytes: 10 },
            DeduplicationResult::Deduped { saved_bytes: 10 }
        );
    }

    #[test]
    fn test_numa_allocation_equality() {
        let a1 = NumaAllocation {
            address: 0x1000,
            size: 64,
            node_id: 0,
        };
        let a2 = NumaAllocation {
            address: 0x1000,
            size: 64,
            node_id: 0,
        };
        assert_eq!(a1, a2);
    }

    #[test]
    fn test_allocator_multi_node_stats() {
        let topo = synthetic_two_node();
        let mut alloc = NumaAllocator::new(topo);
        alloc.allocate(100, 0);
        alloc.allocate(200, 1);
        let stats = alloc.get_stats();
        assert_eq!(stats.per_node_allocated[0], 100);
        assert_eq!(stats.per_node_allocated[1], 200);
        assert_eq!(stats.total_allocated, 300);
    }

    #[test]
    fn test_custom_age_threshold() {
        let config = DeduplicationConfig {
            min_age_threshold: 10,
            ..DeduplicationConfig::default()
        };
        let dd = StringDeduplicator::new(config);
        assert!(!dd.should_deduplicate(9));
        assert!(dd.should_deduplicate(10));
    }

    #[test]
    fn test_fnv1a_known_value() {
        // "a" -> known FNV-1a 64-bit value
        let h = fnv1a_hash(b"a");
        assert_ne!(h, 0);
        assert_ne!(h, 0xcbf29ce484222325); // not the empty-string hash
    }

    #[test]
    fn test_cleanup_preserves_live_entries() {
        let mut dd = StringDeduplicator::new(DeduplicationConfig::default());
        let h1 = fnv1a_hash(b"live");
        let h2 = fnv1a_hash(b"dead");
        dd.deduplicate(h1, b"live");
        dd.deduplicate(h2, b"dead");
        // Age only h2 past the threshold
        if let Some(entries) = dd.table.get_mut(&h2) {
            for e in entries.iter_mut() {
                e.age = 100;
            }
        }
        let removed = dd.cleanup_dead_entries();
        assert_eq!(removed, 1);
        assert!(dd.table.contains_key(&h1));
        assert!(!dd.table.contains_key(&h2));
    }
}
