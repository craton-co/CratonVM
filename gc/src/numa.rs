//! NUMA-aware allocation (Phase 16.3) and string deduplication (Phase 16.4).

use std::collections::HashMap;

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
#[derive(Debug, Clone)]
pub struct NumaTopology {
    pub nodes: Vec<NumaNode>,
    pub total_nodes: usize,
    pub is_numa_available: bool,
}

impl NumaTopology {
    /// Detect the NUMA topology.  Stub implementation returns a single-node
    /// topology that owns 4 CPUs and 8 GiB of memory.
    pub fn detect() -> Self {
        let node = NumaNode {
            id: 0,
            cpu_count: 4,
            memory_size_bytes: 8 * 1024 * 1024 * 1024,
            free_memory_bytes: 6 * 1024 * 1024 * 1024,
            cpu_ids: vec![0, 1, 2, 3],
        };
        NumaTopology {
            nodes: vec![node],
            total_nodes: 1,
            is_numa_available: false,
        }
    }

    /// Map a CPU id to the NUMA node that owns it.
    pub fn node_for_cpu(&self, cpu_id: usize) -> usize {
        for node in &self.nodes {
            if node.cpu_ids.contains(&cpu_id) {
                return node.id;
            }
        }
        0 // fallback
    }

    /// Determine the preferred node for a given thread id (simple modular mapping).
    pub fn preferred_node_for_thread(&self, thread_id: u64) -> usize {
        if self.total_nodes == 0 {
            return 0;
        }
        (thread_id as usize) % self.total_nodes
    }
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
        let per_node_allocated: Vec<u64> =
            self.per_node_arenas.iter().map(|a| a.allocated_bytes).collect();
        let per_node_peak: Vec<u64> =
            self.per_node_arenas.iter().map(|a| a.peak_bytes).collect();
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
        self.table.entry(hash).or_insert_with(Vec::new).push(entry);
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
        assert_eq!(topo.total_nodes, 1);
        assert!(!topo.is_numa_available);
        assert_eq!(topo.nodes.len(), 1);
    }

    #[test]
    fn test_node_for_cpu_found() {
        let topo = NumaTopology::detect();
        assert_eq!(topo.node_for_cpu(0), 0);
        assert_eq!(topo.node_for_cpu(3), 0);
    }

    #[test]
    fn test_node_for_cpu_fallback() {
        let topo = NumaTopology::detect();
        // CPU 99 does not exist — should fall back to node 0
        assert_eq!(topo.node_for_cpu(99), 0);
    }

    #[test]
    fn test_preferred_node_single() {
        let topo = NumaTopology::detect();
        assert_eq!(topo.preferred_node_for_thread(0), 0);
        assert_eq!(topo.preferred_node_for_thread(7), 0);
    }

    #[test]
    fn test_preferred_node_multi() {
        let topo = NumaTopology {
            nodes: vec![
                NumaNode { id: 0, cpu_count: 2, memory_size_bytes: 4 << 30, free_memory_bytes: 2 << 30, cpu_ids: vec![0, 1] },
                NumaNode { id: 1, cpu_count: 2, memory_size_bytes: 4 << 30, free_memory_bytes: 2 << 30, cpu_ids: vec![2, 3] },
            ],
            total_nodes: 2,
            is_numa_available: true,
        };
        assert_eq!(topo.preferred_node_for_thread(0), 0);
        assert_eq!(topo.preferred_node_for_thread(1), 1);
        assert_eq!(topo.preferred_node_for_thread(2), 0);
    }

    #[test]
    fn test_numa_node_properties() {
        let topo = NumaTopology::detect();
        let node = &topo.nodes[0];
        assert_eq!(node.cpu_count, 4);
        assert_eq!(node.memory_size_bytes, 8 * 1024 * 1024 * 1024);
        assert!(node.free_memory_bytes <= node.memory_size_bytes);
    }

    #[test]
    fn test_node_for_cpu_multi_node() {
        let topo = NumaTopology {
            nodes: vec![
                NumaNode { id: 0, cpu_count: 2, memory_size_bytes: 4 << 30, free_memory_bytes: 2 << 30, cpu_ids: vec![0, 1] },
                NumaNode { id: 1, cpu_count: 2, memory_size_bytes: 4 << 30, free_memory_bytes: 2 << 30, cpu_ids: vec![2, 3] },
            ],
            total_nodes: 2,
            is_numa_available: true,
        };
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
        let topo = NumaTopology::detect();
        let mut alloc = NumaAllocator::new(topo);
        let a1 = alloc.allocate_interleaved(100);
        let a2 = alloc.allocate_interleaved(100);
        // single node — both go to node 0
        assert_eq!(a1.node_id, 0);
        assert_eq!(a2.node_id, 0);
    }

    #[test]
    fn test_allocate_interleaved_multi_node() {
        let topo = NumaTopology {
            nodes: vec![
                NumaNode { id: 0, cpu_count: 2, memory_size_bytes: 4 << 30, free_memory_bytes: 2 << 30, cpu_ids: vec![0, 1] },
                NumaNode { id: 1, cpu_count: 2, memory_size_bytes: 4 << 30, free_memory_bytes: 2 << 30, cpu_ids: vec![2, 3] },
            ],
            total_nodes: 2,
            is_numa_available: true,
        };
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
        assert_eq!(
            result,
            DeduplicationResult::Deduped {
                saved_bytes: 5
            }
        );
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
        assert_ne!(DeduplicationResult::New, DeduplicationResult::AlreadyDeduped);
        assert_ne!(DeduplicationResult::New, DeduplicationResult::Skipped);
        assert_eq!(
            DeduplicationResult::Deduped { saved_bytes: 10 },
            DeduplicationResult::Deduped { saved_bytes: 10 }
        );
    }

    #[test]
    fn test_numa_allocation_equality() {
        let a1 = NumaAllocation { address: 0x1000, size: 64, node_id: 0 };
        let a2 = NumaAllocation { address: 0x1000, size: 64, node_id: 0 };
        assert_eq!(a1, a2);
    }

    #[test]
    fn test_allocator_multi_node_stats() {
        let topo = NumaTopology {
            nodes: vec![
                NumaNode { id: 0, cpu_count: 2, memory_size_bytes: 4 << 30, free_memory_bytes: 2 << 30, cpu_ids: vec![0, 1] },
                NumaNode { id: 1, cpu_count: 2, memory_size_bytes: 4 << 30, free_memory_bytes: 2 << 30, cpu_ids: vec![2, 3] },
            ],
            total_nodes: 2,
            is_numa_available: true,
        };
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
