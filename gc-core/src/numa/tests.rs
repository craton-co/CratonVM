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
