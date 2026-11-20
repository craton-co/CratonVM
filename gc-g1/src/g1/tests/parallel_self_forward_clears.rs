#[allow(unused_imports)]
use super::*;

    /// Regression (parallel-evac self-forward UAF). The parallel evacuator
    /// installs each forward into the from-space object's *persistent*
    /// `ObjectHeader::forwarding_ptr` field. A normally-evacuated object's
    /// region is reset in Phase 5 (zeroing the field), but a SELF-FORWARDED
    /// (evacuation-failure) object's region is KEPT — so its `forwarding_ptr`
    /// must be explicitly cleared at cycle end. Without the clear, the NEXT
    /// collection's `evacuate` fast path reads the stale self-pointer, SKIPS
    /// re-evacuating the still-live object, records no forward, and
    /// `free_or_keep_cset` (seeing no `key == value` entry) frees the region
    /// out from under it — a use-after-free that surfaced as V7b dangling
    /// references / SIGSEGV on the `PromoteMixed` humongous-array workload.
    #[test]
    pub fn parallel_self_forward_clears_forwarding_ptr_across_cycles() {
        let gc = G1Collector::new(parallel_config(4, 6));
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(12345));
        let mut roots = vec![obj];

        // Force an evacuation failure: drain the to-space pool by retyping every
        // Free region to Old (non-CSet), leaving the GC nowhere to copy the
        // Eden survivor — so it must self-forward (stay in place).
        {
            let mut regions = gc.regions.write();
            for r in regions.iter_mut() {
                if r.region_type() == RegionType::Free {
                    r.set_region_type(RegionType::Old);
                }
            }
        }

        // Cycle 1: obj cannot be copied -> self-forwards (identity entry).
        let r1 = gc.young_collection_parallel(&mut roots, &NoopMonitors);
        let addr = roots[0].as_ptr() as usize;
        assert_eq!(
            r1.pointer_map.get(&addr),
            Some(&addr),
            "expected a self-forward (evacuation failure)"
        );
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(12345));
        // THE FIX: the kept object's persistent forwarding_ptr is cleared, so it
        // does not look "already forwarded" to the next cycle.
        assert!(
            !gc.get_header(roots[0]).is_forwarded(),
            "self-forwarded object's forwarding_ptr must be cleared after the cycle"
        );

        // Cycle 2: with a stale self-pointer the object would be skipped and its
        // region freed; the field read would then hit reclaimed memory.
        gc.young_collection_parallel(&mut roots, &NoopMonitors);
        assert_eq!(
            gc.get_field(roots[0], 0).as_int(),
            Some(12345),
            "live object lost across a second collection (stale self-forward UAF)"
        );
    }

    /// Regression for the residual parallel self-forward race: workers must not
    /// scan evacuation-failed CSet objects in place while other workers may still
    /// be copying from them. The parallel closure defers those in-place scans to
    /// a serial drain, and that drain must still discover transitive children.
    #[test]
    pub fn parallel_self_forwarded_holders_are_drained_serially() {
        let gc = G1Collector::new(parallel_config(4, 6));
        let child = gc.alloc_object(ClassId::new(2), 1);
        gc.set_field(child, 0, Value::Int(77));
        let parent = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(parent, 0, Value::Object(Some(child)));

        let parent_addr = parent.as_ptr() as usize;
        let child_addr = child.as_ptr() as usize;
        let mut roots = vec![parent];

        // Force evacuation failure: leave the young CSet with no free to-space.
        {
            let mut regions = gc.regions.write();
            for r in regions.iter_mut() {
                if r.region_type() == RegionType::Free {
                    r.set_region_type(RegionType::Old);
                }
            }
        }

        let r = gc.young_collection_parallel(&mut roots, &NoopMonitors);

        assert_eq!(roots[0].as_ptr() as usize, parent_addr);
        assert_eq!(r.pointer_map.get(&parent_addr), Some(&parent_addr));
        assert_eq!(
            r.pointer_map.get(&child_addr),
            Some(&child_addr),
            "serial drain did not discover the self-forwarded child"
        );

        let drained_child = match gc.get_field(roots[0], 0) {
            Value::Object(Some(o)) => o,
            other => panic!("parent child ref lost after serial drain: {other:?}"),
        };
        assert_eq!(drained_child.as_ptr() as usize, child_addr);
        assert_eq!(gc.get_field(drained_child, 0).as_int(), Some(77));
        assert!(!gc.get_header(roots[0]).is_forwarded());
        assert!(!gc.get_header(drained_child).is_forwarded());
    }

    #[test]
    pub fn parallel_identity_forwards_seed_serial_drain() {
        let mut deferred = vec![0x1000usize];
        let forwards = vec![
            (0x2000usize, 0x3000usize),
            (0x4000usize, 0x4000usize),
            (0x4000usize, 0x4000usize),
            (0x1000usize, 0x1000usize),
        ];

        SharedEvac::append_self_forwarded_from_forwards(&forwards, &mut deferred);

        assert_eq!(deferred, vec![0x1000usize, 0x4000usize]);
    }

    /// Regression (defect 2: persistent forwarding_ptr root-remap). When the
    /// parallel evacuator reaches a CSet object whose `forwarding_ptr` is already
    /// set (a fast-path hit), it must RECORD `old -> existing` in `pointer_map`
    /// (so the VM's `update_all_roots` can remap a root that still points at
    /// `old`) and CLEAR the header at cycle end (so the forward does not persist
    /// into the next cycle). Before the fix the fast path returned `existing`
    /// without recording it, so the root stayed stuck on the from-space object
    /// and dangled when its region was reused.
    #[test]
    pub fn parallel_fast_path_hit_is_recorded_and_root_remapped() {
        let mut cfg = parallel_config(4, 8);
        cfg.promotion_age = 1;
        let gc = G1Collector::new(cfg);

        // Promote a target T to Old so it is a stable (non-CSet) forward target.
        let t = gc.alloc_object(ClassId::new(2), 1);
        gc.set_field(t, 0, Value::Int(99));
        let mut troots = vec![t];
        gc.young_collection_parallel(&mut troots, &NoopMonitors); // Survivor, age 1
        gc.young_collection_parallel(&mut troots, &NoopMonitors); // promote -> Old
        let t_old = troots[0].as_ptr() as usize;

        // O lives in young Eden; pre-install O's forward to T as if a worker
        // had already forwarded O to T this cycle (or a stale prior-cycle
        // redirect). Root O.
        let o = gc.alloc_object(ClassId::new(1), 0);
        let o_addr = o.as_ptr() as usize;
        unsafe {
            (*(o.as_ptr() as *mut ObjectHeader)).set_forwarding_address(t_old as *mut u8);
        }
        let mut roots = vec![o];
        let r = gc.young_collection_parallel(&mut roots, &NoopMonitors);

        // (a) the fast-path forward O -> T is recorded in pointer_map.
        assert_eq!(
            r.pointer_map.get(&o_addr),
            Some(&t_old),
            "fast-path forward O->T was not recorded in pointer_map (root cannot be remapped)"
        );
        // (b) the root is remapped to T, and T is intact (not re-copied/corrupted).
        assert_eq!(
            roots[0].as_ptr() as usize,
            t_old,
            "root not remapped to the forward target"
        );
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(99));
    }

    /// Drain-phase evacuation-failure semantics: a LIVE holder that
    /// self-forwards during the serial drain still gets its children
    /// discovered (fixpoint over the identity-forward set), while a DEAD
    /// object in the same kept region is NOT scanned — its targets are not
    /// resurrected (an earlier revision walked every kept-region object,
    /// which under sustained evacuation failure resurrected kept garbage
    /// wholesale each pause and OOMed the SteadyChurn recreation). The dead
    /// holder's slot goes stale, which is safe: a stale reference can only
    /// point into a region that is Free or a this-pause destination — never a
    /// CSet-resident live object — so nothing consults it as a live edge.
    #[test]
    pub fn parallel_drain_phase_self_forward_kept_region_fully_scanned() {
        // workers=1 → the parallel machinery runs single-threaded, making the
        // pool-exhaustion sequencing deterministic.
        let gc = G1Collector::new(parallel_config(1, 8));

        let region_of = |o: ObjectRef| {
            gc.lookup_region_for_addr(o.as_ptr() as usize)
                .expect("object not in any region")
        };

        // Region A: root X.
        let x = gc.alloc_object(ClassId::new(1), 1);
        let ra = region_of(x);

        // Spill into region B.
        let mut filler = gc.alloc_object(ClassId::new(9), 200);
        while region_of(filler) == ra {
            filler = gc.alloc_object(ClassId::new(9), 200);
        }

        // Region B: DEAD holder D (never rooted) and live child Y; X -> Y.
        let d = gc.alloc_object(ClassId::new(2), 1);
        let y = gc.alloc_object(ClassId::new(3), 1);
        let rb = region_of(d);
        assert_eq!(region_of(y), rb, "D and Y must share region B");
        assert_ne!(ra, rb);
        gc.set_field(y, 0, Value::Int(41));
        gc.set_field(x, 0, Value::Object(Some(y)));

        // Spill into region C.
        let mut filler2 = gc.alloc_object(ClassId::new(9), 200);
        while region_of(filler2) == rb {
            filler2 = gc.alloc_object(ClassId::new(9), 200);
        }

        // Region C: Z, referenced ONLY by the dead D.
        let z = gc.alloc_object(ClassId::new(4), 1);
        let rc = region_of(z);
        assert!(rc != ra && rc != rb);
        gc.set_field(z, 0, Value::Int(42));
        gc.set_field(d, 0, Value::Object(Some(z)));
        let z_addr = z.as_ptr() as usize;

        // Empty the to-space pool so EVERY evacuation self-forwards: X in
        // Phase 1 (worker-phase keep of region A), Y only during the serial
        // drain (drain-phase keep of region B — after the old single-round
        // backstop had already run).
        {
            let mut regions = gc.regions.write();
            for r in regions.iter_mut() {
                if r.region_type() == RegionType::Free {
                    r.set_region_type(RegionType::Old);
                }
            }
        }

        let mut roots = vec![x];
        let r = gc.young_collection_parallel(&mut roots, &NoopMonitors);

        // Y self-forwarded during the drain — region B is a drain-phase keep.
        let y_addr = gc.get_field(roots[0], 0);
        let y_now = match y_addr {
            Value::Object(Some(o)) => o,
            other => panic!("X.field lost: {other:?}"),
        };
        assert_eq!(
            r.pointer_map.get(&(y_now.as_ptr() as usize)),
            Some(&(y_now.as_ptr() as usize)),
            "Y should have self-forwarded (drain-phase evacuation failure)"
        );

        // Dead holder D (region B) must NOT have been scanned: its target Z is
        // garbage and must not be resurrected into the pointer map (that
        // wholesale resurrection is what OOMed the churn workload).
        assert_eq!(
            r.pointer_map.get(&z_addr),
            None,
            "dead kept-region holder was scanned — its garbage target Z was \
             resurrected (kept-garbage amplification)"
        );
        assert_eq!(gc.get_field(d, 0), Value::Object(None),
            "an unreachable holder in a failed region must lose its stale edge before a later RSet walk");
        // And the live path through the drain is fully intact.
        assert_eq!(gc.get_field(y_now, 0).as_int(), Some(41), "Y corrupted");
    }

    #[test]
    pub fn parallel_mixed_basic() {
        let mut cfg = parallel_config(4, 16);
        cfg.promotion_age = 1;
        let gc = G1Collector::new(cfg);
        let a = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(a, 0, Value::Int(11));
        let b = gc.alloc_object(ClassId::new(2), 1);
        gc.set_field(b, 0, Value::Int(22));
        let mut roots = vec![a, b];
        gc.young_collection_parallel(&mut roots, &NoopMonitors); // -> Survivor
        gc.young_collection_parallel(&mut roots, &NoopMonitors); // -> Old
        assert!(gc.count_regions(RegionType::Old) >= 1);
        // Parallel mixed collection (young empty, old region(s) in CSet).
        let _r = gc.mixed_collection_parallel(&mut roots, &NoopMonitors);
        assert_eq!(gc.get_field(roots[0], 0).as_int(), Some(11));
        assert_eq!(gc.get_field(roots[1], 0).as_int(), Some(22));
    }

    // --- Serial-vs-parallel equivalence (no loss / dup / corruption) ---

    pub fn build_equiv_graph(gc: &G1Collector, n: usize, s: usize) -> ObjectRef {
        let shared: Vec<ObjectRef> = (0..s)
            .map(|j| {
                let c = gc.alloc_object(ClassId::new(9), 1);
                gc.set_field(c, 0, Value::Int(1000 + j as i32));
                c
            })
            .collect();
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, n);
        for i in 0..n {
            let parent = gc.alloc_object(ClassId::new(8), 2);
            gc.set_field(parent, 0, Value::Int(i as i32));
            gc.set_field(parent, 1, Value::Object(Some(shared[i % s])));
            gc.set_array_element(arr, i, Value::Object(Some(parent)))
                .unwrap();
        }
        arr
    }

    /// Audit §9 item 2: the post-evacuation CSet verification must run in a
    /// build that ships, and must SAY how much of the heap it looked at.
    ///
    /// The unit-test build has `debug_assertions`, so the pass here is the
    /// unbounded one — which is the point of this test: it pins that the
    /// metering happens on the path tests actually take, so a future change
    /// that stops publishing coverage fails here rather than silently making
    /// the release sampler's numbers the only ones anybody sees.
    #[test]
    pub fn cset_verification_publishes_its_coverage() {
        let before = cratonvm_gc_core::gc_metrics::gc_metrics_raw();
        let gc = make_collector();

        // A small live graph in a surviving region, so the verifier has
        // something to walk after the pause.
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, 32);
        for i in 0..32 {
            let o = gc.alloc_object(ClassId::new(1), 1);
            gc.set_field(o, 0, Value::Int(i as i32));
            gc.set_array_element(arr, i, Value::Object(Some(o)))
                .unwrap();
        }
        let mut roots = vec![arr];
        gc.young_collection(&mut roots, &NoopMonitors);

        let after = cratonvm_gc_core::gc_metrics::gc_metrics_raw();
        assert!(
            after.cset_verify_pauses > before.cset_verify_pauses,
            "the pause must record a verification pass"
        );
        assert!(
            after.cset_verify_objects > before.cset_verify_objects,
            "the pass must publish the number of objects it walked — a zero \
             `dangling` is not a claim about anything without it"
        );
        assert_eq!(
            after.cset_verify_dangling, before.cset_verify_dangling,
            "a correct collection leaves no dangling reference into a freed CSet region"
        );
    }

    /// The budgeted (release-shaped) pass must stop AT its budget and leave a
    /// cursor, so coverage accumulates across pauses instead of re-walking the
    /// same prefix of the heap forever.
    ///
    /// Calls the inner walk directly with a small budget: the wrapper's budget
    /// is `usize::MAX` under `debug_assertions`, so going through it would
    /// assert on the sampler's behaviour while running the unbounded sweep.
    /// An empty CSet is deliberate -- nothing was freed, so there is nothing to
    /// report, and what is under test is the budget and the cursor.
    #[test]
    pub fn budgeted_cset_verification_stops_at_its_budget_and_leaves_a_cursor() {
        let gc = make_collector();
        for i in 0..64 {
            let o = gc.alloc_object(ClassId::new(1), 2);
            gc.set_field(o, 0, Value::Int(i));
        }

        let regions = gc.regions.read();
        let empty_cset: RegionSet = RegionSet::new();
        let empty_map = cratonvm_types::PointerMap::default();

        let before = cratonvm_gc_core::gc_metrics::gc_metrics_raw();
        gc.cset_verify_cursor.store(0, Ordering::Relaxed);
        gc.verify_no_dangling_into_cset_within(
            &regions,
            &empty_cset,
            &empty_map,
            &[],
            4,
            usize::MAX,
        );
        let after = cratonvm_gc_core::gc_metrics::gc_metrics_raw();

        let walked = after.cset_verify_objects - before.cset_verify_objects;
        assert!(
            walked <= 4,
            "the budget is a ceiling on objects walked, got {walked}"
        );
        assert_eq!(
            after.cset_verify_truncated - before.cset_verify_truncated,
            1,
            "a pass that stopped on its budget must be counted as truncated, or a partial sweep reads like a whole-heap verification"
        );

        // A budget large enough to reach the end resets the rotation, so the
        // next pass does not begin mid-heap for no reason.
        gc.verify_no_dangling_into_cset_within(
            &regions,
            &empty_cset,
            &empty_map,
            &[],
            usize::MAX,
            usize::MAX,
        );
        assert_eq!(gc.cset_verify_cursor.load(Ordering::Relaxed), 0);
    }

    /// G1-9 regression: the parallel evacuator must scan a COMPACT object's
    /// reference fields.
    ///
    /// This is the coverage gap that let G1-9 hide for months. Every other gc
    /// unit test allocates through `alloc_object(ClassId, n)` with no
    /// `CompactLayout` registered for that class, so its objects take the
    /// legacy uniform 16-byte-cell body — and the parallel evacuator's old
    /// `HEADER_SIZE + slot_idx * SLOT_SIZE` stride is accidentally CORRECT for
    /// exactly those objects. A compact body needs a registered layout, which
    /// only a real loaded class produced, so the defect was reachable only from
    /// a full VM run: `-XX:+UseG1GC` + parallel evacuation reported
    /// `[g1][SECURITY V7b] post-evacuation dangling reference` on the first
    /// pause, deterministically, including at `CRATONVM_G1_WORKERS=1`.
    ///
    /// Registering the layout here closes that gap. Under the pre-fix scan the
    /// holders' reference fields are never visited, so their referents are
    /// never evacuated and the assertions below fail.
    ///
    /// # This test writes PROCESS-GLOBAL state, deliberately and narrowly
    ///
    /// `register_class_layout` publishes into the same kind of process-wide
    /// table that `narrow_oop::enable` does, and `cargo test` runs this
    /// binary's tests on parallel threads — the hazard that produced this
    /// session's original flake (see `narrow_oop::disable_for_test`). The blast
    /// radius is bounded here in a way it was not there: a registration only
    /// affects objects allocated with THIS `(class_id, field_count)` pair, and
    /// `COMPACT_HOLDER_CLASS` is an id no other test in this crate uses. It is
    /// also never unregistered, so the state it publishes is monotone — a
    /// second run of this test finds its own layout already present rather than
    /// racing a teardown.
    #[test]
    pub fn parallel_evacuation_scans_compact_object_reference_fields() {
        use cratonvm_types::field_layout::{CompactLayout, FieldStorageKind};

        // An id no other gc test allocates with; see the note above.
        const COMPACT_HOLDER_CLASS: u32 = 4242;

        // Four fields: int, ref, int, ref — references at PACKED offsets that
        // the legacy 16-byte stride cannot land on. Body is 32 bytes where the
        // legacy layout would be 4 * 16 = 64, so the old loop also addressed
        // twice the object's length.
        let layout = CompactLayout {
            field_disps: vec![8, 16, 24, 32],
            is_ref: vec![false, true, false, true],
            field_kinds: vec![
                FieldStorageKind::Int,
                FieldStorageKind::Reference,
                FieldStorageKind::Int,
                FieldStorageKind::Reference,
            ],
            ref_disps: vec![16, 32],
            total_size: 40,
        };

        let gc = G1Collector::new(parallel_config(8, 32));
        cratonvm_types::register_class_layout(
            gc.layout_domain(),
            COMPACT_HOLDER_CLASS,
            std::sync::Arc::new(layout),
        );

        let holders = 200usize;
        let shared = 40usize;

        let children: Vec<ObjectRef> = (0..shared)
            .map(|j| {
                let c = gc.alloc_object(ClassId::new(9), 1);
                gc.set_field(c, 0, Value::Int(7_000 + j as i32));
                c
            })
            .collect();

        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, holders);
        for i in 0..holders {
            let h = gc.alloc_object(ClassId::new(COMPACT_HOLDER_CLASS), 4);
            assert!(
                cratonvm_types::is_compact_object(&gc.get_header(h)),
                "holder {i} must be COMPACT — otherwise this test proves nothing \
                 (is the layout registered under the collector's domain?)"
            );
            gc.set_field(h, 0, Value::Int(i as i32));
            gc.set_field(h, 1, Value::Object(Some(children[i % shared])));
            gc.set_field(h, 2, Value::Int(-(i as i32)));
            gc.set_field(h, 3, Value::Object(Some(children[(i * 3 + 1) % shared])));
            gc.set_array_element(arr, i, Value::Object(Some(h)))
                .unwrap();
        }

        let mut roots = vec![arr];
        let res = gc.young_collection_parallel(&mut roots, &NoopMonitors);

        // 1 array + holders + shared children, each copied exactly once. Under
        // the pre-fix scan the children are never reached, so this count is
        // short by `shared` before it is short by anything else.
        assert_eq!(
            res.stats.objects_copied,
            1 + holders + shared,
            "every live object — including the compact holders' referents — is copied once"
        );

        let mut child_addrs = std::collections::HashSet::new();
        for i in 0..holders {
            let h = match gc.get_array_element(roots[0], i).unwrap() {
                Value::Object(Some(o)) => o,
                other => panic!("holder {i} lost -> {other:?}"),
            };
            assert_eq!(
                gc.get_field(h, 0).as_int(),
                Some(i as i32),
                "holder {i} int 0"
            );
            assert_eq!(
                gc.get_field(h, 2).as_int(),
                Some(-(i as i32)),
                "holder {i} int 2"
            );
            for (slot, j) in [(1usize, i % shared), (3usize, (i * 3 + 1) % shared)] {
                let child = match gc.get_field(h, slot) {
                    Value::Object(Some(o)) => o,
                    other => panic!("holder {i} slot {slot} lost -> {other:?}"),
                };
                assert_eq!(
                    gc.get_field(child, 0).as_int(),
                    Some(7_000 + j as i32),
                    "holder {i} slot {slot} points at the wrong child"
                );
                child_addrs.insert(child.as_ptr() as usize);
            }
        }
        assert_eq!(
            child_addrs.len(),
            shared,
            "the shared children must be evacuated to one copy each"
        );
    }

    /// Shape for the repeated-collection oracle below: an array of `p` parents,
    /// each holding an identity int and TWO references into a pool of `s`
    /// shared children, each of which holds an identity int and a reference
    /// into a pool of `l` shared leaves.
    ///
    /// Two levels of sharing, not one, and two reference slots per parent
    /// rather than one: the CAS-forwarding loser arm in `SharedEvac::evacuate`
    /// is only reachable when two workers race on the SAME object, so a tree
    /// never exercises it — that is why the original decode bug there needed a
    /// diamond to surface at all. A second level means a worker can lose a race
    /// on a child while another worker is still copying that child's referent.
    pub fn build_shared_graph(gc: &G1Collector, p: usize, s: usize, l: usize) -> ObjectRef {
        let leaves: Vec<ObjectRef> = (0..l)
            .map(|k| {
                let o = gc.alloc_object(ClassId::new(11), 1);
                gc.set_field(o, 0, Value::Int(20_000 + k as i32));
                o
            })
            .collect();
        let children: Vec<ObjectRef> = (0..s)
            .map(|j| {
                let o = gc.alloc_object(ClassId::new(10), 2);
                gc.set_field(o, 0, Value::Int(10_000 + j as i32));
                gc.set_field(o, 1, Value::Object(Some(leaves[j % l])));
                o
            })
            .collect();
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, p);
        for i in 0..p {
            let parent = gc.alloc_object(ClassId::new(9), 3);
            gc.set_field(parent, 0, Value::Int(i as i32));
            gc.set_field(parent, 1, Value::Object(Some(children[i % s])));
            gc.set_field(parent, 2, Value::Object(Some(children[(i * 7 + 3) % s])));
            gc.set_array_element(arr, i, Value::Object(Some(parent)))
                .unwrap();
        }
        arr
    }

    /// Walk the whole graph and assert every property a correct evacuation
    /// preserves: nothing lost, nothing duplicated, no two objects sharing an
    /// address, every identity int intact.
    ///
    /// The duplication and overlap checks are the ones a value-multiset
    /// comparison misses. A shared child copied TWICE still yields the right
    /// value through both parents — it is visible only as two distinct
    /// addresses carrying one identity. Two objects copied to the SAME address
    /// is likewise value-plausible until one address is seen carrying two
    /// identities.
    pub fn verify_shared_graph(
        gc: &G1Collector,
        arr: ObjectRef,
        round: usize,
        p: usize,
        s: usize,
        l: usize,
    ) {
        use std::collections::{HashMap, HashSet};
        let mut addr_of_identity: HashMap<i32, usize> = HashMap::new();
        let mut identity_of_addr: HashMap<usize, i32> = HashMap::new();
        let mut leaf_addrs: HashSet<usize> = HashSet::new();

        for i in 0..p {
            let parent = match gc.get_array_element(arr, i).unwrap() {
                Value::Object(Some(o)) => o,
                other => panic!("round {round}: parent {i} lost -> {other:?}"),
            };
            assert_eq!(
                gc.get_field(parent, 0).as_int(),
                Some(i as i32),
                "round {round}: parent {i} identity"
            );
            let mut note = |identity: i32, addr: usize| {
                let canonical = *addr_of_identity.entry(identity).or_insert(addr);
                assert_eq!(
                    canonical, addr,
                    "round {round}: object {identity} exists at TWO addresses ({canonical:#x} and {addr:#x}) - evacuated twice"
                );
                let owner = *identity_of_addr.entry(addr).or_insert(identity);
                assert_eq!(
                    owner, identity,
                    "round {round}: address {addr:#x} carries TWO identities ({owner} and {identity}) - overlapping copies"
                );
            };
            note(i as i32, parent.as_ptr() as usize);

            for (slot, j) in [(1usize, i % s), (2usize, (i * 7 + 3) % s)] {
                let child = match gc.get_field(parent, slot) {
                    Value::Object(Some(o)) => o,
                    other => panic!("round {round}: parent {i} slot {slot} lost -> {other:?}"),
                };
                let child_id = 10_000 + j as i32;
                assert_eq!(
                    gc.get_field(child, 0).as_int(),
                    Some(child_id),
                    "round {round}: parent {i} slot {slot} points at the wrong child"
                );
                note(child_id, child.as_ptr() as usize);

                let leaf = match gc.get_field(child, 1) {
                    Value::Object(Some(o)) => o,
                    other => panic!("round {round}: child {j} leaf lost -> {other:?}"),
                };
                let leaf_id = 20_000 + (j % l) as i32;
                assert_eq!(
                    gc.get_field(leaf, 0).as_int(),
                    Some(leaf_id),
                    "round {round}: child {j} points at the wrong leaf"
                );
                note(leaf_id, leaf.as_ptr() as usize);
                leaf_addrs.insert(leaf.as_ptr() as usize);
            }
        }
        assert_eq!(
            leaf_addrs.len(),
            l,
            "round {round}: the shared leaves collapsed or multiplied"
        );
        assert_eq!(
            addr_of_identity.len(),
            p + s + l,
            "round {round}: reachable object count changed"
        );
    }

    /// G1-9 oracle at unit level.
    ///
    /// Every other parallel-evacuation test runs ONE collection over a graph
    /// built moments earlier, so it only ever exercises Eden -> Survivor with
    /// every object at age 0. The audit's corruption is reported from sustained
    /// churn, where the same objects are re-evacuated pause after pause, age
    /// past `promotion_age` into the Old TLAB, and are reached through a heap
    /// whose region types keep changing under them.
    ///
    /// So: hold one graph across repeated 8-worker parallel pauses, allocate
    /// real garbage between them, and re-verify the WHOLE graph — structure and
    /// identity, not just a value multiset — after every pause.
    #[test]
    pub fn repeated_parallel_young_collections_never_lose_duplicate_or_corrupt() {
        let gc = G1Collector::new(parallel_config(8, 32));
        let (p, s, l) = (300usize, 60usize, 20usize);
        let arr = build_shared_graph(&gc, p, s, l);
        let mut roots = vec![arr];

        for round in 0..8usize {
            // Garbage the pause has to walk past and reclaim.
            for g in 0..200i32 {
                let junk = gc.alloc_object(ClassId::new(99), 2);
                gc.set_field(junk, 0, Value::Int(g));
            }
            let res = gc.young_collection_parallel(&mut roots, &NoopMonitors);
            if round == 0 {
                // Everything live is young and in the CSet on the first pause.
                assert_eq!(
                    res.stats.objects_copied,
                    1 + p + s + l,
                    "round 0: exactly the live set is copied, once each"
                );
            }
            verify_shared_graph(&gc, roots[0], round, p, s, l);
        }
    }

    /// gci i14/yt — `CRATONVM_G1_SURVIVOR_PLAB=1`: the same graph oracle over
    /// repeated 8-worker parallel pauses, with Survivor copies packed into
    /// PLAB chunks of a shared region. The live set (~400 small objects) fits
    /// one region, so at most two Survivor regions exist after any pause (two
    /// only if a publish race lost), and every Survivor region parses from 0
    /// to its cursor exactly: the retired chunk tails are fillers, not holes.
    /// Filter: `cargo test -j 5 -p cratonvm-gc --lib gci_i14_yt_`.
/// gci i15/ra finding 1 — `CRATONVM_G1_SURVIVOR_PLAB=1` with a survivor
    /// volume of several regions, so the shared region SWITCHES several times
    /// while eight workers carve from it. One worker installs each region, so
    /// the pause leaves about `copied / region` Survivor regions (plus chunk
    /// tails), not one single-chunk region per worker that lost the publish
    /// race; every copy keeps its identity, every Survivor region parses to
    /// its cursor, and the shared word never stays at the switching sentinel
    /// (a second pause carves again). Filter:
    /// `cargo test -j 5 -p cratonvm-gc --lib gci_i15_ra_`.
/// gci i16/pl findings 3 and 4 — `CRATONVM_G1_SURVIVOR_PLAB=1` with the
    /// chunks no longer zeroed and large objects copied straight into the
    /// shared region. Every Free region is dirtied with a junk pattern first,
    /// so a Survivor region that relied on zeroed bytes would not parse; a
    /// third of the survivors are 4 KiB arrays (above 10 % of a 32 KiB PLAB),
    /// so chunks run short of them and the direct arm runs. Over two pauses
    /// every copy keeps its identity and its whole body, every Survivor
    /// region parses from 0 to its cursor (chunk tails are fillers, direct
    /// copies whole objects), and the census counts direct copies. Filter:
    /// `cargo test -j 5 -p cratonvm-gc --lib gci_i16_pl_`.
/// gci i16/pl finding 3 — `G1Region::claim_unzeroed` claims like
    /// `bump_alloc` (8-aligned, bounded by the region, cursor advanced) and
    /// leaves the bytes as they were.
    #[test]
    pub fn gci_i16_pl_claim_unzeroed_claims_without_zeroing() {
        let gc = make_collector();
        let idx = {
            let regions = gc.regions.read();
            regions
                .iter()
                .position(|r| r.region_type() == RegionType::Free)
                .expect("a Free region")
        };
        gc.with_regions_mut(|regions| regions[idx].data.fill(0x5A));
        let regions = gc.regions.read();
        let r = &regions[idx];
        let len = r.data.len();
        r.set_cursor(0);
        assert_eq!(r.claim_unzeroed(64, "test"), Some(0));
        assert_eq!(r.cursor(), 64);
        assert!(r.data[..64].iter().all(|&b| b == 0x5A), "not zeroed");
        // Past the end: refused, cursor untouched.
        assert_eq!(r.claim_unzeroed(len, "test"), None);
        assert_eq!(r.cursor(), 64);
        // Exactly the rest.
        assert_eq!(r.claim_unzeroed(len - 64, "test"), Some(64));
        assert_eq!(r.cursor(), len);
        assert_eq!(r.claim_unzeroed(8, "test"), None);
        r.set_cursor(0);
    }

    /// gci i16/pl finding 4 — HotSpot's direct-allocation rule at the
    /// default `ParallelGCBufferWastePct` (10 % of a 32 KiB PLAB = 3 276.8).
    #[test]
    pub fn gci_i16_pl_direct_allocation_threshold() {
        assert!(!survivor_plab_wants_direct(16));
        assert!(!survivor_plab_wants_direct(3_276));
        assert!(survivor_plab_wants_direct(3_277));
        assert!(survivor_plab_wants_direct(SURVIVOR_PLAB_BYTES));
        assert!(survivor_plab_wants_direct(usize::MAX), "saturates");
    }

    /// gci i16/pl finding 8 — a pin on an already-pinned region and an unpin
    /// that is not the last are shared-guard counts; the first pin and the
    /// last unpin are the write-guard ones, and `pinned` is always
    /// `pin_total() > 0`.
    #[test]
    pub fn gci_i16_pl_repeat_pins_are_shared_guard_counts() {
        let gc = make_collector();
        let state = |gc: &G1Collector| {
            let regions = gc.regions.read();
            let r = &regions[3];
            (
                r.pin_count,
                r.critical_extra_pins.load(Ordering::Relaxed),
                r.pin_total(),
                r.pinned,
            )
        };
        gc.pin_region(3);
        assert_eq!(state(&gc), (1, 0, 1, true), "the first pin takes the write guard");
        gc.pin_region(3);
        gc.pin_region(3);
        assert_eq!(state(&gc), (1, 2, 3, true), "repeat pins are shared-guard counts");
        gc.unpin_region(3);
        gc.unpin_region(3);
        assert_eq!(state(&gc), (1, 0, 1, true), "non-last unpins are shared-guard");
        gc.unpin_region(3);
        assert_eq!(state(&gc), (0, 0, 0, false), "the last unpin clears the flag");
        gc.unpin_region(3);
        assert_eq!(state(&gc), (0, 0, 0, false), "an extra unpin is a no-op");
        // A write-guard unpin takes a shared-guard pin first, so a region is
        // never left pinned by a leftover extra count.
        gc.pin_region(3);
        gc.pin_region(3);
        gc.with_regions_mut(|regions| regions[3].unpin_exclusive());
        assert_eq!(state(&gc), (1, 0, 1, true));
        gc.with_regions_mut(|regions| regions[3].unpin_exclusive());
        assert_eq!(state(&gc), (0, 0, 0, false));
        // Out of range: nothing, no panic.
        gc.pin_region(usize::MAX);
        gc.unpin_region(usize::MAX);
    }

    /// gci i16/pl finding 8 — the repeat pin and the non-last unpin really do
    /// run under a shared guard: they complete while another holder keeps a
    /// READ guard, which a write-guard pin could not (it would wait for it).
    /// Also through `pin_array_payload_for_critical`, the JNI door.
    #[test]
    pub fn gci_i16_pl_repeat_pin_runs_beside_a_reader() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 16);
        let addr = arr.as_ptr() as usize;
        let idx = gc
            .pin_array_payload_for_critical(addr, ARRAY_DATA_OFFSET, 16 * 4)
            .expect("an Eden array's payload is inside its region");
        let done = std::thread::scope(|s| {
            let held = gc.regions.read();
            let (tx, rx) = std::sync::mpsc::channel();
            let g = &gc;
            s.spawn(move || {
                let again = g.pin_array_payload_for_critical(addr, ARRAY_DATA_OFFSET, 16 * 4);
                g.pin_region(idx);
                g.unpin_region(idx);
                g.unpin_region(idx);
                let _ = tx.send(again);
            });
            let got = rx.recv_timeout(std::time::Duration::from_secs(30));
            drop(held);
            got
        });
        assert_eq!(done.ok(), Some(Some(idx)), "the shared-guard arms ran beside a reader");
        assert!(gc.is_pinned(idx), "the first section still holds it");
        assert_eq!(gc.regions.read()[idx].pin_total(), 1);
        gc.unpin_region(idx);
        assert!(!gc.is_pinned(idx));
    }

    /// The same oracle with a mark cycle OPEN across the pauses.
    ///
    /// A young pause under an active cycle takes paths the quiescent one never
    /// does: `marking_keepalive_roots` evacuates CSet-resident gray/SATB
    /// objects as extra roots, the survivors are re-grayed after Phase 5, and
    /// the mark worklist is remapped through the same `pointer_map` the roots
    /// were. That interaction is where a lost or double-copied object would
    /// show up not as a value divergence but as a stale gray entry.
    #[test]
    pub fn repeated_parallel_young_collections_under_an_open_mark_cycle() {
        let gc = G1Collector::new(parallel_config(8, 32));
        let (p, s, l) = (200usize, 40usize, 15usize);
        let arr = build_shared_graph(&gc, p, s, l);
        let mut roots = vec![arr];

        gc.start_concurrent_mark(&stw());
        gc.remark(&stw(), &roots);

        for round in 0..6usize {
            for g in 0..150i32 {
                let junk = gc.alloc_object(ClassId::new(99), 2);
                gc.set_field(junk, 0, Value::Int(g));
            }
            // Drive some marking between pauses so the gray set is genuinely
            // non-empty when the next evacuation starts.
            gc.concurrent_mark_step(64);
            gc.young_collection_parallel(&mut roots, &NoopMonitors);
            verify_shared_graph(&gc, roots[0], round, p, s, l);
        }
    }

    pub fn reachable_ints(gc: &G1Collector, root_arr: ObjectRef, n: usize) -> Vec<i32> {
        let mut vals = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for i in 0..n {
            let parent = match gc.get_array_element(root_arr, i).unwrap() {
                Value::Object(Some(o)) => o,
                _ => panic!("parent {i} lost"),
            };
            vals.push(gc.get_field(parent, 0).as_int().unwrap());
            let child = match gc.get_field(parent, 1) {
                Value::Object(Some(o)) => o,
                _ => panic!("child of {i} lost"),
            };
            if seen.insert(child.as_ptr()) {
                vals.push(gc.get_field(child, 0).as_int().unwrap());
            }
        }
        vals.sort();
        vals
    }

    /// The serial path, the 1-worker parallel path, and the 8-worker parallel
    /// path must all copy the same number of objects and preserve the same
    /// multiset of reachable values. Addresses differ; program-observable
    /// state does not.
    #[test]
    pub fn parallel_matches_serial_no_loss_or_dup() {
        let n = 500usize;
        let s = 40usize;

        // Serial baseline (env flag is unset in the test process, so the public
        // `young_collection` dispatches to the serial body).
        let gc_serial = G1Collector::new(parallel_config(1, 32));
        let arr_s = build_equiv_graph(&gc_serial, n, s);
        let mut roots_s = vec![arr_s];
        let res_s = gc_serial.young_collection(&mut roots_s, &NoopMonitors);
        let vals_s = reachable_ints(&gc_serial, roots_s[0], n);

        // 1-worker parallel (deterministic drain through the parallel code).
        let gc_p1 = G1Collector::new(parallel_config(1, 32));
        let arr_p1 = build_equiv_graph(&gc_p1, n, s);
        let mut roots_p1 = vec![arr_p1];
        let res_p1 = gc_p1.young_collection_parallel(&mut roots_p1, &NoopMonitors);
        let vals_p1 = reachable_ints(&gc_p1, roots_p1[0], n);

        // 8-worker parallel (real concurrency + CAS races).
        let gc_p8 = G1Collector::new(parallel_config(8, 32));
        let arr_p8 = build_equiv_graph(&gc_p8, n, s);
        let mut roots_p8 = vec![arr_p8];
        let res_p8 = gc_p8.young_collection_parallel(&mut roots_p8, &NoopMonitors);
        let vals_p8 = reachable_ints(&gc_p8, roots_p8[0], n);

        // 1 array + n parents + s shared children.
        let expected_copied = 1 + n + s;
        assert_eq!(res_s.stats.objects_copied, expected_copied, "serial count");
        assert_eq!(res_p1.stats.objects_copied, expected_copied, "p1 count");
        assert_eq!(res_p8.stats.objects_copied, expected_copied, "p8 count");

        assert_eq!(vals_s, vals_p1, "serial vs 1-worker values diverge");
        assert_eq!(vals_s, vals_p8, "serial vs 8-worker values diverge");
    }

    /// Regression for the serial-G1 evacuate-into-CSet-region bug: a young GC
    /// evacuates ALL survivor regions, so a partially-filled survivor region is
    /// itself in the CSet. `alloc_in_type_locked` used to reuse it as an
    /// evacuation *destination*, so survivors were copied into a region Phase 5
    /// then reset (freed) — silently dropping live objects and leaving every
    /// reference dangling (the held-tree `got=1` repro). A held object graph must
    /// survive REPEATED young GCs fully intact. This drives the SERIAL path
    /// (`young_collection`; the env flag is unset in tests).
    #[test]
    pub fn held_chain_survives_repeated_young_gc() {
        let gc = make_collector();
        let n = 50usize;
        let head = gc.alloc_object(ClassId::new(1), 1);
        let mut cur = head;
        for _ in 1..n {
            let node = gc.alloc_object(ClassId::new(1), 1);
            gc.set_field(cur, 0, Value::Object(Some(node)));
            cur = node;
        }
        gc.set_field(cur, 0, Value::Int(999)); // tail marker
        let mut roots = vec![head];
        for round in 0..6 {
            let _ = gc.alloc_object(ClassId::new(9), 8); // a little garbage
            gc.young_collection(&mut roots, &NoopMonitors);
            // Walk the whole chain: it must still be exactly `n` nodes ending in
            // the tail marker — no node lost to an evacuate-into-CSet free.
            let mut count = 0usize;
            let mut c = roots[0];
            loop {
                count += 1;
                assert!(count <= n, "round {round}: chain longer than {n}");
                match gc.get_field(c, 0) {
                    Value::Object(Some(next)) => c = next,
                    Value::Int(999) => break,
                    other => panic!("round {round}: chain broke at node {count} -> {other:?}"),
                }
            }
            assert_eq!(count, n, "round {round}: chain length changed (lost nodes)");
        }
    }

    /// Regression for G1 evacuation failure (to-space exhaustion). A heap of
    /// exactly two regions is filled by a held chain so that when a young GC
    /// runs there are ZERO free regions for to-space. Previously `evacuate_object`
    /// returned `None` and the caller DROPPED the object → silent live-object
    /// loss. Now it self-forwards in place and `free_or_keep_cset` keeps the
    /// region, so the entire held chain survives intact (nothing reclaimed — the
    /// real heap is full, which the allocation path turns into a clean OOM).
    #[test]
    pub fn evacuation_failure_self_forwards_does_not_drop() {
        let mut cfg = small_config();
        cfg.region_size = 1024 * 1024;
        cfg.heap_size = 2 * 1024 * 1024; // exactly 2 regions
        let heap_size = cfg.heap_size;
        let gc = G1Collector::new(cfg);
        // Fill ~1.1 MB across the 2 regions with a held chain → both regions
        // become Eden (CSet), leaving 0 Free regions for evacuation to-space.
        // Sized from the layout, not restated: the premise is "fill both
        // regions so evacuation has ZERO free to-space", and a literal object
        // count silently stops meaning that when the header shrinks. At
        // HEADER_SIZE = 32 the old literal 24000 filled ~1.15 MB of the 2 MB
        // heap; at 24 the same count fits in ONE region, evacuation succeeds,
        // and the test failed asserting a *successful* collection freed
        // nothing. One field, no registered compact layout -> legacy SLOT_SIZE.
        let obj_size = cratonvm_types::HEADER_SIZE + cratonvm_types::SLOT_SIZE;
        let n = (heap_size * 55 / 100) / obj_size;
        let head = gc.alloc_object(ClassId::new(1), 1);
        let mut cur = head;
        for _ in 1..n {
            let node = gc.alloc_object(ClassId::new(1), 1);
            gc.set_field(cur, 0, Value::Object(Some(node)));
            cur = node;
        }
        gc.set_field(cur, 0, Value::Int(7));
        let mut roots = vec![head];
        // No free region → every survivor hits evacuation failure → self-forward.
        let r = gc.young_collection(&mut roots, &NoopMonitors);
        assert_eq!(
            r.stats.bytes_freed, 0,
            "a failed collection must free nothing"
        );
        // The whole chain must still be reachable and intact — nothing dropped.
        let mut count = 0usize;
        let mut c = roots[0];
        loop {
            count += 1;
            assert!(count <= n, "chain longer than {n}");
            match gc.get_field(c, 0) {
                Value::Object(Some(next)) => c = next,
                Value::Int(7) => break,
                other => panic!("chain dropped at node {count} -> {other:?}"),
            }
        }
        assert_eq!(count, n, "evacuation failure dropped live nodes");
    }

    #[test]
    pub fn evacuation_failure_scrubs_moved_originals_and_full_mark_reclaims_kept_regions() {
        let mut cfg = small_config();
        cfg.region_size = 64 * 1024;
        cfg.heap_size = 3 * cfg.region_size;
        cfg.initial_heap_size = cfg.heap_size;
        let gc = G1Collector::new(cfg);
        // Four nodes fit per region. Interleave the roots so the one free
        // destination fills partway through BOTH source regions.
        let nodes: Vec<_> = (0..8)
            .map(|id| {
                let n = gc.alloc_object(ClassId::new(71), 1000);
                gc.set_field(n, 0, Value::Int(id));
                n
            })
            .collect();
        for i in 0..8 {
            gc.set_field(nodes[i], 1, Value::Object(Some(nodes[(i + 4) % 8])));
        }
        assert_eq!(gc.count_regions(RegionType::Free), 1);
        let mut roots: Vec<_> = [0, 4, 1, 5, 2, 6, 3, 7].map(|i| nodes[i]).into();
        let result = gc.young_collection_serial(&mut roots, &NoopMonitors);
        assert_eq!(result.pointer_map.iter().filter(|(k, v)| k == v).count(), 4);
        assert_eq!(gc.count_regions(RegionType::Old), 2);
        for (&from, &to) in &result.pointer_map {
            if from != to {
                let h = unsafe { &*(from as *const ObjectHeader) };
                assert!(
                    is_tlab_filler_header(h),
                    "a moved original in a kept region must be a filler"
                );
            } else {
                let h = unsafe { &*(from as *const ObjectHeader) };
                assert_ne!(
                    h.gc_flags() & GC_FLAG_OLD_GEN,
                    0,
                    "an in-place promotion must engage compiled stores' Old -> young barrier"
                );
            }
        }
        for (root, id) in roots.iter().zip([0, 4, 1, 5, 2, 6, 3, 7]) {
            assert_eq!(gc.get_field(*root, 0), Value::Int(id));
            let Value::Object(Some(child)) = gc.get_field(*root, 1) else {
                panic!("lost edge")
            };
            assert_eq!(gc.get_field(child, 0), Value::Int((id + 4) % 8));
        }
        // A direct young driver must publish kept Old -> young edges too.
        let mut kept_root = [roots[7]];
        gc.young_collection_serial(&mut kept_root, &NoopMonitors);
        let Value::Object(Some(child)) = gc.get_field(kept_root[0], 1) else {
            panic!("lost kept edge")
        };
        assert_eq!(gc.get_field(child, 0), Value::Int(3));
        // Dropped failed regions can now be freed IN PLACE with no to-space.
        gc.start_concurrent_mark(&stw());
        gc.remark(&stw(), &[]);
        while !gc.concurrent_mark_step(usize::MAX) {}
        gc.cleanup(&stw());
        assert_eq!(gc.count_regions(RegionType::Old), 0);
    }

    /// Regression for the kept-region death spiral (the serial-G1 "SteadyChurn
    /// recreation trips at -Xmx16m" wedge). When young-live exceeds the free
    /// pool at trigger time, a single pass keeps most of the heap (every
    /// region holding one self-forwarded live object survives wholesale,
    /// garbage included) and successive collections monotonically degrade to
    /// `copied == 0 && freed == 0`. `retry_after_evacuation_failure` must
    /// drain the kept garbage with same-pause retry passes (excluding earlier
    /// passes' destination regions) and compose the per-pass forward maps so
    /// pause-start addresses still remap correctly.
    #[test]
    pub fn evacuation_failure_retry_drains_kept_garbage() {
        let mut cfg = small_config();
        cfg.region_size = 1024 * 1024;
        cfg.heap_size = 10 * 1024 * 1024; // 10 regions
        let gc = G1Collector::new(cfg);

        // Interleave live chain nodes with ~2x garbage so every filled region
        // holds BOTH live and garbage (the SteadyChurn shape): ~6 MB total,
        // ~2 MB live — more than the 1-region pool left below, so pass 1 must
        // hit evacuation failure part-way through.
        let head = gc.alloc_object(ClassId::new(1), 8);
        let head_addr = head.as_ptr() as usize;
        let mut cur = head;
        let mut live_count = 1usize;
        loop {
            let _g1 = gc.alloc_object(ClassId::new(9), 8);
            let _g2 = gc.alloc_object(ClassId::new(9), 8);
            let node = gc.alloc_object(ClassId::new(1), 8);
            gc.set_field(cur, 0, Value::Object(Some(node)));
            cur = node;
            live_count += 1;
            // Stop once ~6 of the 10 regions are consumed.
            let used: usize = {
                let regions = gc.regions.read();
                regions
                    .iter()
                    .filter(|r| r.region_type() != RegionType::Free)
                    .count()
            };
            if used >= 6 {
                break;
            }
        }
        gc.set_field(cur, 0, Value::Int(424242)); // tail marker

        // Leave exactly ONE Free region as evacuation pool; deny the rest.
        {
            let mut regions = gc.regions.write();
            let mut left = 1usize;
            for r in regions.iter_mut().rev() {
                if r.region_type() == RegionType::Free {
                    if left > 0 {
                        left -= 1;
                    } else {
                        r.set_region_type(RegionType::Old);
                    }
                }
            }
        }

        let mut roots = vec![head];
        let first = gc.young_collection(&mut roots, &NoopMonitors);
        assert!(
            first.pointer_map.iter().any(|(k, v)| k == v),
            "setup failed to force evacuation failure (no self-forwards)"
        );

        let result = gc.retry_after_evacuation_failure(first, &mut roots, &NoopMonitors);

        // (a) recovery converged: no identity forward survives the pause.
        assert!(
            !result.pointer_map.iter().any(|(k, v)| k == v),
            "retry loop left unresolved self-forwards (still wedged)"
        );
        // (b) the kept garbage was actually drained: most of the ~4 MB of
        // garbage must be free again.
        let free_after = {
            let regions = gc.regions.read();
            regions
                .iter()
                .filter(|r| r.region_type() == RegionType::Free)
                .count()
        };
        assert!(
            free_after >= 3,
            "kept-region garbage not reclaimed (free regions after recovery: {free_after})"
        );
        // (c) the composed map remaps the PAUSE-START head address to the
        // final location the root was rewritten to (frame-local semantics).
        assert_eq!(
            result.pointer_map.get(&head_addr),
            Some(&(roots[0].as_ptr() as usize)),
            "composed pointer_map does not take the pause-start address to the final copy"
        );
        // (d) the live chain is fully intact.
        let mut count = 0usize;
        let mut c = roots[0];
        loop {
            count += 1;
            assert!(count <= live_count, "chain longer than {live_count}");
            match gc.get_field(c, 0) {
                Value::Object(Some(next)) => c = next,
                Value::Int(424242) => break,
                other => panic!("chain broke at node {count} -> {other:?}"),
            }
        }
        assert_eq!(count, live_count, "retry recovery dropped live nodes");
    }

    /// `g1-parallel-evac-pool-exhaustion-loses-a-held-monitor` (retired
    /// 2026-09-30), candidate 1 of its "Where to look": a parallel young pause
    /// must hand every locked object back with its lock state intact, whether
    /// the object was copied (room in to-space) or self-forwarded in place
    /// (the pool exhausted). The thin-lock owner and recursion live in the
    /// mark-word bits a self-forward overwrites, and an INFLATED word is found
    /// by address, so each case has its own way to lose the lock.
    #[test]
    pub fn a_parallel_young_pause_keeps_the_lock_state_of_copied_and_self_forwarded_objects() {
        use cratonvm_types::{MARK_INFLATED, MARK_STATE_MASK, MARK_THIN_LOCKED};
        for exhaust in [false, true] {
            let gc = make_collector();
            let thin = gc.alloc_object(ClassId::new(1), 1);
            let fat = gc.alloc_object(ClassId::new(1), 1);
            let plain = gc.alloc_object(ClassId::new(1), 1);
            gc.set_field(thin, 0, Value::Object(Some(plain)));
            let set_state = |o: ObjectRef, state: u32| {
                // SAFETY: a live object this test allocated.
                let h = unsafe { &*(o.as_ptr() as *const ObjectHeader) };
                let m = h.mark();
                h.mark_word.store((m & !0xFFFF) | state, Ordering::Relaxed);
            };
            // Owner slot 5, recursion 2.
            let thin_state = MARK_THIN_LOCKED | (5 << 2) | (2 << 13);
            set_state(thin, thin_state);
            set_state(fat, MARK_INFLATED);
            if exhaust {
                // Leave the evacuator nowhere to copy to, as in
                // `the_parallel_driver_also_leaves_no_forwarding_tag_behind`.
                for i in 0..gc.num_regions() {
                    assert!(gc.commit_through_region(i), "fixture: commit must succeed");
                }
                gc.with_regions_mut(|regions| {
                    for r in regions.iter_mut() {
                        if r.region_type() == RegionType::Free {
                            r.set_region_type(RegionType::Old);
                            r.set_cursor(r.data.len());
                        }
                    }
                });
            }
            let before = [thin.as_ptr() as usize, fat.as_ptr() as usize];
            let mut roots: Vec<ObjectRef> = vec![thin, fat];
            let result = gc.young_collection_parallel(&mut roots, &NoopMonitors);
            let after = [roots[0].as_ptr() as usize, roots[1].as_ptr() as usize];
            for i in 0..2 {
                let moved = result.pointer_map.get(&before[i]).copied();
                assert!(
                    moved.is_some(),
                    "exhaust={exhaust}: root {i} missing from the pointer map"
                );
                assert_eq!(
                    moved,
                    Some(after[i]),
                    "exhaust={exhaust}: root {i} and the map disagree"
                );
                if exhaust {
                    assert_eq!(
                        after[i], before[i],
                        "fixture: the exhausted pause must self-forward"
                    );
                } else {
                    assert_ne!(after[i], before[i], "fixture: the roomy pause must copy");
                }
            }
            let state_of = |a: usize| unsafe { (*(a as *const ObjectHeader)).mark() } & 0xFFFF;
            assert_eq!(
                state_of(after[0]),
                thin_state,
                "exhaust={exhaust}: thin lock lost"
            );
            assert_eq!(
                state_of(after[1]) & MARK_STATE_MASK,
                MARK_INFLATED,
                "exhaust={exhaust}: inflated state lost"
            );
            // The locked object's field still names a live, readable object.
            let child = match gc.get_field(roots[0], 0) {
                Value::Object(Some(c)) => c,
                other => panic!("exhaust={exhaust}: field lost: {other:?}"),
            };
            // SAFETY: `child` is the live object the field names.
            let cid = unsafe { (*(child.as_ptr() as *const ObjectHeader)).class_id.as_u32() };
            assert_eq!(
                cid, 1,
                "exhaust={exhaust}: the field names something that is not the child"
            );
        }
    }

    #[test]
    pub fn a_descheduled_claim_winner_is_waited_out_not_abandoned() {
        // `g1-parallel-evac-pool-exhaustion-loses-a-held-monitor` (retired
        // 2026-09-30). A peer claims the source and is descheduled before it
        // publishes; the bounded `forwarding_address` gives up and answers
        // "not forwarded" (the control), while the evacuator's wait must
        // return the published target.
        #[repr(C, align(8))]
        struct Obj([u64; 4]);
        let src = Obj([0; 4]);
        let dst = Obj([0; 4]);
        let src_addr = src.0.as_ptr() as usize;
        let dst_addr = dst.0.as_ptr() as usize;
        // SAFETY: 32 aligned, zeroed bytes: a NEUTRAL header with one body word.
        let hdr = unsafe { &*(src_addr as *const ObjectHeader) };
        let observed = hdr.mark();
        hdr.try_claim_forwarding(observed).expect("fixture: claim");

        // Control: the bounded wait abandons a claim that stays BUSY.
        assert!(
            hdr.forwarding_address().is_null(),
            "control: the bounded decode must give up on an unpublished claim"
        );

        let src_n = src_addr;
        let publisher = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            // SAFETY: `src` outlives the join below.
            let h = unsafe { &*(src_n as *const ObjectHeader) };
            h.publish_claimed_forwarding(dst_addr as *mut u8);
        });
        let before = parallel_evac_claim_waits();
        let mark = wait_out_peer_claim(hdr);
        publisher.join().unwrap();
        assert!(ObjectHeader::is_forwarded_mark(mark));
        assert_eq!(mark & cratonvm_types::MARK_FWD_BUSY, 0);
        assert_eq!(hdr.forwarding_address() as usize, dst_addr);
        assert!(parallel_evac_claim_waits() > before);
        std::hint::black_box(&dst);
    }

    /// `compose_forward_maps` semantics: values chase one hop (including a
    /// pass-1 self-forward resolved by a retry), new keys are added, and a
    /// colliding key keeps the FIRST pass's entry (pause-start address wins
    /// over a recycled intermediate).
    #[test]
    pub fn a_parallel_source_screen_waits_before_reading_a_claimed_arrays_shape() {
        let gc = make_collector();
        let src = gc.alloc_array(ClassId::new(0), ArrayElementType::Byte, 32);
        let dst = gc.alloc_array(ClassId::new(0), ArrayElementType::Byte, 32);
        let source = src.as_ptr() as usize;
        let target = dst.as_ptr() as usize;
        assert!(
            target > source,
            "fixture allocates the copy after its source"
        );
        let header = unsafe { &*(source as *const ObjectHeader) };
        header
            .try_claim_forwarding(header.mark())
            .expect("source claim");
        // Model the overwritten second word while the winner is still BUSY.
        // It is deliberately not a plausible array length. Only the later
        // non-BUSY publication makes the forwarding target readable.
        unsafe { &*((source + 8) as *const std::sync::atomic::AtomicU32) }
            .store(u32::MAX, Ordering::Relaxed);
        let mut regions = gc.regions.write();
        assert_eq!(
            gc.classify_candidate_header_view(RegionView::Slice(&regions), source)
                .0,
            HeaderVerdict::ImplausibleShape,
            "control: a bounded candidate shape read cannot resolve this unfinished claim"
        );
        // Model a complete copy in a worker TLAB whose cursor has not yet
        // been published. The source stays below the cursor; only the target
        // header is above it, exactly as it is during parallel evacuation.
        let target_idx = (target - gc.arena_base) >> gc.region_shift;
        let target_base = regions[target_idx].data.as_ptr() as usize;
        assert!(target >= target_base);
        let target_offset = target - target_base;
        assert!(
            target_offset > 0,
            "fixture target follows the source in-region"
        );
        regions[target_idx].set_cursor(target_offset);
        assert!(target + HEADER_SIZE > target_base + regions[target_idx].cursor());
        let view = RegionView::Raw(RegionsBase(regions.as_mut_ptr()), regions.len());
        assert!(
            !gc.forward_target_has_readable_header(view, target),
            "an unretired TLAB tail is not readable until its copy is published"
        );
        regions[target_idx].publish_parallel_copy_end(target_offset + HEADER_SIZE);
        assert!(gc.forward_target_has_readable_header(view, target));
        let publisher = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            unsafe { &*(source as *const ObjectHeader) }
                .publish_claimed_forwarding(target as *mut u8);
        });
        assert_eq!(
            gc.classify_candidate_header_view(view, source).0,
            HeaderVerdict::Object,
            "the parallel screen must adopt the completed claim instead of leaving a stale source reference"
        );
        publisher.join().unwrap();
        header.retire_forwarding_from(dst.as_ptr());
    }

    #[test]
    pub fn a_candidate_forward_into_an_uncommitted_g1_region_is_not_dereferenced() {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_size: 64 * 1024 * 1024,
            initial_heap_size: 8 * 1024 * 1024,
            ..small_config()
        });
        let array = gc.alloc_array(ClassId::new(0), ArrayElementType::Byte, 64);
        let candidate = array.as_ptr() as usize + ARRAY_DATA_OFFSET;
        let target = gc.arena_base + gc.arena.committed_len();
        assert!(
            target < gc.arena_end,
            "the reservation has an uncommitted tail"
        );
        assert_eq!(target >> 30, candidate >> 30, "same mapped provenance leaf");
        let target_region = (target - gc.arena_base) >> gc.region_shift;
        assert_eq!(
            gc.regions.read()[target_region].region_type(),
            RegionType::Free
        );

        // Make a live payload word look like an array header forwarded into
        // the first reserved, uncommitted region. `resolved_shape` used to
        // follow this plausible same-leaf address and fault on its shape word.
        unsafe {
            std::ptr::copy_nonoverlapping(
                array.as_ptr() as *const u8,
                candidate as *mut u8,
                HEADER_SIZE,
            );
        }
        let fake = unsafe { &*(candidate as *const ObjectHeader) };
        fake.set_forwarding_address(target as *mut u8);
        assert!(fake.is_forwarded() && !fake.is_self_forwarded());

        let regions = gc.regions.read();
        assert_eq!(
            gc.classify_candidate_header(&regions, candidate).0,
            HeaderVerdict::ImplausibleShape,
            "a candidate whose forwarding target is not committed is rejected before dereference"
        );
    }

    #[test]
    pub fn a_candidate_header_must_fit_inside_the_committed_cursor() {
        let gc = make_collector();
        let array = gc.alloc_array(ClassId::new(0), ArrayElementType::Byte, 64);
        let source = array.as_ptr() as usize;
        let idx = (source - gc.arena_base) >> gc.region_shift;
        let mut regions = gc.regions.write();
        let region = &regions[idx];
        let base = region.data.as_ptr() as usize;
        let cursor = region.cursor();
        assert!(
            cursor >= HEADER_SIZE,
            "fixture has a committed header-sized prefix"
        );
        let candidate = base + cursor - 8;
        assert!(candidate + HEADER_SIZE <= base + gc.config.region_size);
        // Copy a real array header to the final word of the committed prefix.
        // Its tags are readable, but `shape` begins just beyond the cursor.
        // The backing region is mapped, so the old code returned Object instead
        // of faulting; this still freezes the logical committed-boundary bug.
        unsafe {
            std::ptr::copy_nonoverlapping(source as *const u8, candidate as *mut u8, HEADER_SIZE);
        }
        assert_eq!(
            gc.classify_candidate_header_view(RegionView::Slice(&regions), candidate)
                .0,
            HeaderVerdict::AboveCursor,
            "a candidate whose complete ObjectHeader crosses the committed cursor is rejected"
        );
    }

    #[test]
    pub fn a_claim_that_never_publishes_is_given_up_after_the_safety_valve() {
        // Not a state a genuine claim can reach; the valve is what keeps a
        // word that only LOOKS like a claim from hanging the pause.
        #[repr(C, align(8))]
        struct Obj([u64; 2]);
        let src = Obj([0; 2]);
        // SAFETY: 16 aligned, zeroed bytes: a NEUTRAL header.
        let hdr = unsafe { &*(src.0.as_ptr() as *const ObjectHeader) };
        hdr.try_claim_forwarding(hdr.mark())
            .expect("fixture: claim");
        let before = PARALLEL_EVAC_CLAIM_WAITS_ABANDONED.load(Ordering::Relaxed);
        let t = std::time::Instant::now();
        let mark = wait_out_peer_claim(hdr);
        assert!(t.elapsed() >= PEER_CLAIM_WAIT_LIMIT);
        assert_ne!(mark & cratonvm_types::MARK_FWD_BUSY, 0, "still BUSY");
        assert!(PARALLEL_EVAC_CLAIM_WAITS_ABANDONED.load(Ordering::Relaxed) > before);
    }

    #[test]
    pub fn compose_forward_maps_chases_and_keeps_pause_start_keys() {
        let mut acc: cratonvm_types::PointerMap = [(0xa0, 0xb0), (0x40, 0x40), (0x70, 0x71)]
            .into_iter()
            .collect();
        let next: cratonvm_types::PointerMap =
            [(0xb0, 0xc0), (0x40, 0x90), (0xe0, 0xf0), (0x70, 0x99)]
                .into_iter()
                .collect();
        G1Collector::compose_forward_maps_on(&mut acc, &next, 1, &|jobs, job| (0..jobs).for_each(job));
        assert_eq!(
            acc.get(&0xa0),
            Some(&0xc0),
            "value not chased through pass 2"
        );
        assert_eq!(
            acc.get(&0x40),
            Some(&0x90),
            "identity self-forward not resolved"
        );
        assert_eq!(acc.get(&0xe0), Some(&0xf0), "new pass-2 key not added");
        assert_eq!(
            acc.get(&0x70),
            Some(&0x71),
            "a colliding pass-2 key (recycled intermediate address) must NOT \
             clobber the pause-start entry from pass 1"
        );
    }

    /// Regression for the Old->young remembered-set-completeness hole: an
    /// `A.a = B` edge created while both are young carries no rset entry. Once
    /// `A` promotes to Old (while `B`, allocated later, stays younger), the edge
    /// becomes Old->young — but it was created/maintained only by GC-internal
    /// pointer rewrites, never a mutator write barrier, so it was missing from
    /// the rset and the next young GC dropped `B` (reachable only via `A.a`).
    /// `update_references_in_regions` now rebuilds the Old->young rset each
    /// collection, so `B` must survive repeated young GCs after `A` is tenured.
    #[test]
    pub fn old_to_young_ref_via_gc_rewrite_not_dropped() {
        let mut cfg = small_config();
        cfg.promotion_age = 3;
        let gc = G1Collector::new(cfg);

        let a = gc.alloc_object(ClassId::new(1), 1);
        let mut roots = vec![a]; // B will be reachable ONLY via A.a
                                 // Age A two young GCs (still a Survivor; A is older than B).
        gc.young_collection(&mut roots, &NoopMonitors);
        gc.young_collection(&mut roots, &NoopMonitors);

        // Allocate B fresh (younger) and wire A.a -> B while both are young.
        let b = gc.alloc_object(ClassId::new(2), 1);
        gc.set_field(b, 0, Value::Int(777));
        gc.set_field(roots[0], 0, Value::Object(Some(b)));

        // Drive several more young GCs: A tenures to Old while B stays younger,
        // so A.a becomes an Old->young edge maintained only by GC rewrites. B
        // must never be dropped.
        for round in 0..6 {
            let _g = gc.alloc_object(ClassId::new(9), 1); // a little churn
            gc.young_collection(&mut roots, &NoopMonitors);
            match gc.get_field(roots[0], 0) {
                Value::Object(Some(bb)) => assert_eq!(
                    gc.get_field(bb, 0).as_int(),
                    Some(777),
                    "round {round}: B lost / corrupted (Old->young rset miss)"
                ),
                other => panic!("round {round}: A.a dropped -> {other:?}"),
            }
        }
    }

    /// Regression for bug C — the Old->old / humongous->old remembered-set
    /// completeness hole that left G1 *mixed* GC non-functional. This is the
    /// `PromoteMixed` repro at unit scale: a referent `B` is reachable ONLY
    /// through a humongous holder array (`keep[0] = B`, the array > region_size/2
    /// so it lives in HumongousStart/Continuation regions and is NEVER a CSet
    /// member). The `keep[0] -> B` edge is created while `B` is young (so the
    /// mutator barrier recorded it against `B`'s then-young region, since
    /// recycled) and is thereafter maintained only by GC-internal pointer
    /// rewrites as `B` promotes to Old — no barrier ever fires for the
    /// humongous->old edge. A mixed GC that selects `B`'s Old region therefore
    /// finds `B` only if `B`'s region records the humongous holder as an rset
    /// source. Before the fix `collect_outgoing_*_edges` recorded edges into
    /// young targets only, so the holder was never registered, the mixed GC
    /// never scanned `keep[]`, and `B` was dropped (the V7b verifier reported
    /// the humongous holder dangling into a freed CSet region). The Phase-4
    /// rebuild now records edges into every *collectable* (Eden/Survivor/Old)
    /// target, so `B` survives the mixed GC.
    #[test]
    pub fn humongous_to_old_ref_via_gc_rewrite_survives_mixed_gc() {
        let cfg = G1CollectorConfig {
            heap_size: 16 * 1024 * 1024,
            region_size: 1024 * 1024,
            promotion_age: 1, // age >= 1 promotes (2 young GCs)
            old_cset_region_threshold_percent: 100, // B's single Old region is eligible
            ..small_config()
        };
        let gc = G1Collector::new(cfg);

        // A humongous reference array (> 512 KiB of refs) — the `keep[]` holder.
        let len = (640 * 1024) / 8; // 640 KiB of 8-byte refs -> humongous
        let keep = gc.alloc_array(ClassId::new(1), ArrayElementType::Reference, len);
        assert!(
            gc.count_regions(RegionType::HumongousStart) >= 1,
            "keep[] must be humongous for this regression"
        );

        // B is reachable ONLY via keep[0]. Wire it while B is young.
        let b = gc.alloc_object(ClassId::new(2), 1);
        gc.set_field(b, 0, Value::Int(424242));
        gc.set_array_element(keep, 0, Value::Object(Some(b)))
            .unwrap();
        let mut roots = vec![keep]; // root reaches B only through the humongous keep

        // Tenure B to Old (promotion_age = 1). The humongous keep stays in place
        // (humongous objects are never evacuated by young/mixed GC).
        gc.young_collection(&mut roots, &NoopMonitors);
        gc.young_collection(&mut roots, &NoopMonitors);
        assert!(
            gc.old_gen_bytes() > 0,
            "B should have promoted to Old before the mixed GC"
        );

        // Force a mixed GC that can select B's Old region.
        gc.marking_complete.store(true, Ordering::Relaxed);
        gc.mixed_gc_remaining.store(1, Ordering::Relaxed);
        gc.mixed_collection(&mut roots, &NoopMonitors);

        // B must survive: reachable only through the humongous->old edge, which
        // is recorded only by the Phase-4 rset rebuild.
        match gc.get_array_element(roots[0], 0).unwrap() {
            Value::Object(Some(b2)) => assert_eq!(
                gc.get_field(b2, 0).as_int(),
                Some(424242),
                "B dropped / corrupted: humongous->old rset miss in mixed GC"
            ),
            other => panic!("keep[0] dropped in mixed GC -> {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // G1 maturation (g1-maturation.md)
    // -----------------------------------------------------------------------

    /// Overwrite the mark-start (TAMS) snapshot so a test can place TAMS at an
    /// exact offset inside one region while leaving every other region's entry
    /// consistent with its current state. Mirrors the shape
    /// `start_concurrent_mark` publishes; lock order is regions -> snapshot,
    /// same as production.
    pub fn force_mark_snapshot(gc: &G1Collector, region_idx: usize, tams_offset: usize) {
        // F-06: arm the PER-REGION mirror as well as the global snapshot.
        // `start_concurrent_mark` writes both in one loop and `cleanup`
        // debug-asserts they agree, so a fixture that wrote only one would fail
        // on the fixture rather than on what the test is about.
        let snapshot: Vec<(u64, usize, RegionType)> = {
            let mut regions = gc.regions.write();
            for (i, r) in regions.iter_mut().enumerate() {
                let cursor = if i == region_idx {
                    tams_offset
                } else {
                    r.cursor()
                };
                r.mark_start = Some((r.reuse_epoch, cursor, r.region_type()));
                r.marked_bytes_below_tams.store(0, Ordering::Relaxed);
            }
            regions
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let cursor = if i == region_idx {
                        tams_offset                    } else {
                        r.cursor()                    };
                    (r.reuse_epoch, cursor, r.region_type())                })
                .collect()
        };
        let mut snap = gc.mark_start_snapshot.lock();
        snap.clear();
        snap.extend(snapshot);
    }

    /// F-06 — the number `cleanup` reports is the one the MARKER accumulated,
    /// not one it recomputed by walking the heap.
    ///
    /// The walk is still reachable (`CRATONVM_G1_CLEANUP_WALK=1`, and every
    /// debug build runs it as an oracle beside the accumulator), so a test that
    /// only checked `live_bytes` would pass on either implementation. This one
    /// reads the accumulator directly and pins the arithmetic cleanup does with
    /// it.
    #[test]
    pub fn the_marker_accumulates_the_live_bytes_cleanup_reports() {
        let gc = make_collector();
        let live = gc.alloc_object(ClassId::new(1), 0);
        let dead = gc.alloc_object(ClassId::new(2), 0);
        let live_addr = live.as_ptr() as usize;

        let idx = {
            let regions = gc.regions.read();
            gc.region_for_ptr(&regions, live.as_ptr()).unwrap()
        };
        assert_eq!(
            gc.lookup_region_for_addr(dead.as_ptr() as usize),
            Some(idx),
            "test setup: both objects must share one region"
        );
        gc.with_regions_mut(|regions| regions[idx].set_region_type(RegionType::Old));

        // TAMS at the top: everything in the region predates the snapshot, so
        // the bitmap alone decides and nothing is implicitly live.
        let cursor = gc.regions.read()[idx].cursor();
        force_mark_snapshot(&gc, idx, cursor);

        let live_size = {
            let regions = gc.regions.read();
            assert_eq!(
                regions[idx].marked_bytes_below_tams.load(Ordering::Relaxed),
                0,
                "the accumulator is cleared when the snapshot is armed"
            );
            assert!(regions[idx].try_mark_and_account(live_addr));
            let accumulated = regions[idx].marked_bytes_below_tams.load(Ordering::Relaxed);
            assert!(
                accumulated > 0,
                "marking one object below TAMS must accumulate its size"
            );
            accumulated
        };

        gc.cleanup(&stw());

        let regions = gc.regions.read();
        assert_eq!(
            regions[idx].live_bytes, live_size,
            "cleanup reports exactly what the marker accumulated -- the dead \
             object contributes nothing and nothing is implicitly live at this TAMS"
        );
        assert!(
            regions[idx].live_bytes < cursor,
            "the unmarked object's bytes must NOT be counted, or the test is \
             not distinguishing liveness from occupancy"
        );
    }

    /// F-06 — a region recycled since the mark snapshot reports its whole
    /// content live, and its accumulator is cleared with its bitmap.
    ///
    /// This is the fail-safe direction and the one that matters: a recycled
    /// region carries no mark information, so reading its stale accumulator
    /// would under-report liveness — and an under-report is exactly what lets
    /// `cleanup` free a live Old region in place.
    #[test]
    pub fn a_region_recycled_since_mark_start_reports_its_whole_content_live() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 0);
        let idx = {
            let regions = gc.regions.read();
            gc.region_for_ptr(&regions, obj.as_ptr()).unwrap()
        };
        let cursor = gc.regions.read()[idx].cursor();
        force_mark_snapshot(&gc, idx, cursor);
        {
            let regions = gc.regions.read();
            assert!(regions[idx].try_mark_and_account(obj.as_ptr() as usize));
            assert!(regions[idx].marked_bytes_below_tams.load(Ordering::Relaxed) > 0);
        }

        // Recycle it, then refill it: a new incarnation, whose content the mark
        // snapshot says nothing about.
        gc.with_regions_mut(|regions| {
            regions[idx].reset(1);
            regions[idx].set_region_type(RegionType::Old);
            regions[idx].set_cursor(4096);
        });
        {
            let regions = gc.regions.read();
            assert_eq!(
                regions[idx].marked_bytes_below_tams.load(Ordering::Relaxed),
                0,
                "reset clears the accumulator with the bitmap it is derived from"
            );
            assert_eq!(
                regions[idx].tams(),
                0,
                "an incarnation bump means the region's ENTIRE content postdates \
                 the snapshot"
            );
        }

        gc.cleanup(&stw());

        let regions = gc.regions.read();
        assert_eq!(
            regions[idx].live_bytes, 4096,
            "every byte of a recycled region is conservatively live"
        );
        assert_ne!(
            regions[idx].region_type(),
            RegionType::Free,
            "and it must not be freed in place"
        );
    }

    /// G1MAT-1 — `cleanup` must attribute each byte of a region exactly once.
    ///
    /// Before the fix the bitmap walk covered the WHOLE region and the
    /// post-TAMS extent was then added on top, so any post-TAMS object the
    /// marker actually reached (SATB keep-alive, `push_gray_or_mark`, a fresh
    /// promotion a root still names) was counted twice. `live_bytes` could
    /// exceed `cursor`, which corrupts `gc_efficiency` — the ascending
    /// (worst-first) sort key of `mixed_collection` /
    /// `select_old_regions_for_mixed_gc` — and inflates
    /// `estimated_evac_cost_ns`, so mixed GC selects the wrong Old regions AND
    /// fewer of them.
    #[test]
    pub fn g1mat1_cleanup_does_not_double_count_marked_post_tams_objects() {
        let gc = make_collector();
        let below = gc.alloc_object(ClassId::new(1), 0);
        let above = gc.alloc_object(ClassId::new(2), 0);
        let below_addr = below.as_ptr() as usize;
        let above_addr = above.as_ptr() as usize;

        let (idx, base) = {
            let regions = gc.regions.read();
            let idx = gc.region_for_ptr(&regions, below.as_ptr()).unwrap();
            (idx, regions[idx].data.as_ptr() as usize)
        };
        assert_eq!(
            gc.lookup_region_for_addr(above_addr),
            Some(idx),
            "test setup: both objects must share one region"
        );

        // Old, so this is exactly the region class the mixed-GC selector ranks.
        gc.with_regions_mut(|regions| regions[idx].set_region_type(RegionType::Old));

        // TAMS lands between the two objects: `below` predates the snapshot,
        // `above` postdates it.
        let tams = above_addr - base;
        force_mark_snapshot(&gc, idx, tams);

        // The marker reached BOTH — including the post-TAMS object. That is the
        // case the old accounting double-counted.
        gc.with_regions_mut(|regions| {
            assert!(regions[idx].try_mark_and_account(below_addr));
            assert!(regions[idx].try_mark_and_account(above_addr));
        });

        gc.cleanup(&stw());

        let (live_bytes, cursor, efficiency) = {
            let regions = gc.regions.read();
            (
                regions[idx].live_bytes,
                regions[idx].cursor(),
                regions[idx].gc_efficiency,
            )
        };
        assert_eq!(
            live_bytes, cursor,
            "both objects are live exactly once: below-TAMS via the bitmap, \
             above-TAMS implicitly (double-count would give {} > {})",
            live_bytes, cursor
        );
        assert!(
            live_bytes <= cursor,
            "a region can never be more than 100% live"
        );
        assert!(
            efficiency <= 1.0,
            "gc_efficiency must stay in [0,1] — it is the worst-first sort key \
             for mixed GC (got {efficiency})"
        );
    }

    /// G1MAT-1 — a region whose pre-TAMS content is entirely dead but which
    /// grew after mark start must report exactly the post-TAMS bytes as live,
    /// and must NOT be freed in place.
    #[test]
    pub fn g1mat1_cleanup_counts_only_post_tams_bytes_when_pre_tams_is_dead() {
        let gc = make_collector();
        let dead = gc.alloc_object(ClassId::new(1), 0);
        let fresh = gc.alloc_object(ClassId::new(2), 0);
        let fresh_addr = fresh.as_ptr() as usize;

        let (idx, base) = {
            let regions = gc.regions.read();
            let idx = gc.region_for_ptr(&regions, dead.as_ptr()).unwrap();
            (idx, regions[idx].data.as_ptr() as usize)
        };
        gc.with_regions_mut(|regions| regions[idx].set_region_type(RegionType::Old));

        let tams = fresh_addr - base;
        force_mark_snapshot(&gc, idx, tams);
        // Nothing marked: `dead` is unreachable in the snapshot, `fresh` is
        // implicitly live because it postdates it.
        gc.cleanup(&stw());

        let (live_bytes, cursor, region_type) = {
            let regions = gc.regions.read();
            (
                regions[idx].live_bytes,
                regions[idx].cursor(),
                regions[idx].region_type(),
            )
        };
        assert_eq!(
            live_bytes,
            cursor - tams,
            "only the post-TAMS extent is implicitly live"
        );
        assert_ne!(
            region_type,
            RegionType::Free,
            "a region with post-TAMS allocation must never be freed in place"
        );
    }

    /// G1MAT-2 — `reclaim_dead_humongous_spans_locked` derives a span's extent
    /// from `regions[i].cursor()` alone. If that disagrees with the region table
    /// (a stale or corrupt cursor), the old code would `reset()` — zero-fill
    /// and retype-to-Free — regions belonging to OTHER live objects. Require
    /// every claimed continuation region to actually be typed
    /// `HumongousContinuation` before touching the span.
    #[test]
    pub fn g1mat2_humongous_reclaim_skips_span_whose_regions_are_not_continuations() {
        let gc = make_collector();
        // ~1.6 MB over 1 MB regions => HumongousStart + 1 continuation.
        let huge = gc.alloc_array(ClassId::new(0), ArrayElementType::Long, 200_000);
        let start_idx = gc.lookup_region_for_addr(huge.as_ptr() as usize).unwrap();
        assert_eq!(gc.count_regions(RegionType::HumongousContinuation), 1);

        // Simulate the table/cursor disagreement: the region the cursor claims
        // as a continuation is in fact an unrelated live Old region.
        let bystander = start_idx + 1;
        gc.with_regions_mut(|regions| {
            regions[start_idx].live_bytes = 0; // "dead" per the mark bitmap
            regions[bystander].set_region_type(RegionType::Old);
            regions[bystander].set_cursor(4096);
        });

        let reclaimed = {
            let mut regions = gc.regions.write();
            gc.reclaim_dead_humongous_spans_locked(&mut regions, &RegionSet::new())
        };

        assert_eq!(
            reclaimed, 0,
            "a malformed span must not be reclaimed at all"
        );
        let regions = gc.regions.read();
        assert_eq!(
            regions[bystander].region_type(),
            RegionType::Old,
            "the bystander region must not be retyped"
        );
        assert_eq!(
            regions[bystander].cursor(),
            4096,
            "the bystander region must not be zero-filled/reset"
        );
        assert_eq!(
            regions[start_idx].region_type(),
            RegionType::HumongousStart,
            "the start region must be left alone too"
        );
    }

    /// G1MAT-3 — SATB pre-barrier ordering for reference ARRAY element stores:
    /// the old referent must reach the SATB log, and the slot must already hold
    /// the new value afterwards (i.e. the log happened strictly before the
    /// overwrite). Read and store now share one `regions` critical section, so
    /// the read->log->store sequence cannot be interleaved by a lock release.
    #[test]
    pub fn g1mat3_satb_pre_barrier_logs_overwritten_array_element_before_store() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(1), ArrayElementType::Reference, 4);
        let old = gc.alloc_object(ClassId::new(2), 0);
        let new = gc.alloc_object(ClassId::new(3), 0);
        let old_addr = old.as_ptr() as usize;

        gc.set_array_element(arr, 0, Value::Object(Some(old)))
            .unwrap();

        // Marking inactive: no logging at all (the barrier must cost nothing
        // outside a cycle).
        cratonvm_gc_core::satb::flush_thread_satb_buffer(gc.satb_queue());
        let _ = gc.satb_queue().drain();
        gc.set_array_element(arr, 0, Value::Object(Some(old)))
            .unwrap();
        cratonvm_gc_core::satb::flush_thread_satb_buffer(gc.satb_queue());
        assert!(
            !gc.satb_queue().drain().contains(&old_addr),
            "no SATB entry may be logged while marking is idle"
        );

        // Marking active: the overwritten referent must be logged.
        gc.start_concurrent_mark(&stw());
        gc.set_array_element(arr, 0, Value::Object(Some(new)))
            .unwrap();
        cratonvm_gc_core::satb::flush_thread_satb_buffer(gc.satb_queue());
        assert!(
            gc.satb_queue().drain().contains(&old_addr),
            "the OLD element value must be logged before it is overwritten"
        );
        assert_eq!(
            gc.get_array_element(arr, 0).unwrap(),
            Value::Object(Some(new)),
            "the store must still have landed"
        );
    }

    /// G1MAT-3 — the weak-reference PROTOCOL writes suppress the pre-barrier
    /// (INT-8). `set_array_element` now honours the same TLS scope `set_field`
    /// does, so the two store paths cannot disagree about what counts as a
    /// semantic overwrite.
    #[test]
    pub fn g1mat3_satb_pre_barrier_honours_suppression_scope_for_array_stores() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(1), ArrayElementType::Reference, 2);
        let referent = gc.alloc_object(ClassId::new(2), 0);
        let referent_addr = referent.as_ptr() as usize;

        gc.set_array_element(arr, 0, Value::Object(Some(referent)))
            .unwrap();
        gc.start_concurrent_mark(&stw());
        cratonvm_gc_core::satb::flush_thread_satb_buffer(gc.satb_queue());
        let _ = gc.satb_queue().drain();

        {
            let _suppressed = SatbPreSuppressGuard::new();
            gc.set_array_element(arr, 0, Value::Object(None)).unwrap();
        }
        cratonvm_gc_core::satb::flush_thread_satb_buffer(gc.satb_queue());
        assert!(
            !gc.satb_queue().drain().contains(&referent_addr),
            "a suppressed protocol write must not resurrect the referent"
        );
        // …and the suppression scope must not leak past its guard.
        gc.set_array_element(arr, 0, Value::Object(Some(referent)))
            .unwrap();
        gc.set_array_element(arr, 0, Value::Object(None)).unwrap();
        cratonvm_gc_core::satb::flush_thread_satb_buffer(gc.satb_queue());
        assert!(
            gc.satb_queue().drain().contains(&referent_addr),
            "the pre-barrier must be restored once the guard drops"
        );
    }

    /// G1MAT-4 — `cleanup` prunes remembered-set entries naming Free regions.
    ///
    /// Nothing else in the collector ever removes an rset entry: `clear()` runs
    /// only on the TARGET's own `reset()`, so a source index recorded once is
    /// kept for the rest of that target's life and every later pause pays for
    /// it. Free sources are provably dead (a Free region holds no live object),
    /// so dropping them changes no collection decision.
    #[test]
    pub fn g1mat4_cleanup_prunes_rset_sources_naming_free_regions() {
        let gc = make_collector();
        gc.with_regions_mut(|regions| {
            regions[1].set_region_type(RegionType::Free);
            regions[2].set_region_type(RegionType::Old);
            regions[3].set_region_type(RegionType::Old);
            regions[3].rset.add_reference(1); // Free  -> pruned
            regions[3].rset.add_reference(2); // Old   -> kept
            regions[3].rset.add_reference(usize::MAX); // out of range -> pruned
        });

        // No mark-start snapshot: cleanup keeps the pure-bitmap verdict and
        // performs no in-place frees, so this isolates the rset pruning.
        gc.cleanup(&stw());

        let sources = {
            let regions = gc.regions.read();
            regions[3].rset.sources()
        };
        assert!(
            !sources.contains(&1),
            "a source region that is Free cannot hold a live edge — prune it"
        );
        assert!(
            sources.contains(&2),
            "a live source region must be retained"
        );
        assert!(
            !sources.contains(&usize::MAX),
            "an out-of-range source index must be pruned"
        );
    }

    /// Evacuation failure (to-space exhaustion), the path the retry/drain
    /// machinery is built on. With no Free region and no non-CSet Survivor
    /// region left, `evacuate_object` must SELF-FORWARD rather than drop the
    /// object: an identity entry in the pointer map, the object left at its
    /// original address with its fields intact, and its region KEPT (Eden
    /// retyped to Old) instead of reset.
    #[test]
    pub fn evacuation_failure_self_forwards_and_keeps_region() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(obj, 0, Value::Int(4242));
        let obj_addr = obj.as_ptr() as usize;

        let idx = {
            let regions = gc.regions.read();
            gc.region_for_ptr(&regions, obj.as_ptr()).unwrap()
        };

        // Starve to-space: every other region is a non-Free, non-Survivor
        // region with no room, so neither the `alloc_in_type_locked` reuse scan
        // nor `find_free_region_from` can supply a destination. `cursor = 0` keeps
        // the Phase-4 walk of these regions a no-op.
        gc.with_regions_mut(|regions| {
            for (i, r) in regions.iter_mut().enumerate() {
                if i != idx {
                    r.set_region_type(RegionType::Old);
                    r.set_cursor(0);
                }
            }
        });

        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        assert_eq!(
            result.stats.objects_copied, 0,
            "there is no to-space: nothing can be copied"
        );
        assert_eq!(
            result.pointer_map.get(&obj_addr),
            Some(&obj_addr),
            "evacuation failure must record an IDENTITY forward (self-forward), \
             not drop the object"
        );
        assert_eq!(
            roots[0].as_ptr() as usize,
            obj_addr,
            "a self-forwarded root must not move"
        );
        assert_eq!(
            gc.get_field(roots[0], 0).as_int(),
            Some(4242),
            "the self-forwarded object's payload must be intact"
        );

        let region_type = {
            let regions = gc.regions.read();
            regions[idx].region_type()
        };
        assert_eq!(
            region_type,
            RegionType::Old,
            "a kept young region must become Old for full-mark recovery, not be reset"
        );
        assert_eq!(
            result.stats.bytes_freed, 0,
            "the kept region's bytes must not be reported as freed"
        );
    }

    /// Evacuation failure must not silently degrade the pause trigger: a pause
    /// that self-forwards raises `needs_gc_free_percent` so the NEXT collection
    /// starts with a bigger to-space pool (the kept-region death-spiral fix),
    /// and a clean pause decays it back toward the 25% baseline.
    #[test]
    pub fn evacuation_failure_raises_then_decays_the_gc_trigger() {
        let gc = make_collector();
        let baseline = gc.needs_gc_free_percent.load(Ordering::Relaxed);

        let obj = gc.alloc_object(ClassId::new(1), 0);
        let idx = {
            let regions = gc.regions.read();
            gc.region_for_ptr(&regions, obj.as_ptr()).unwrap()
        };
        gc.with_regions_mut(|regions| {
            for (i, r) in regions.iter_mut().enumerate() {
                if i != idx {
                    r.set_region_type(RegionType::Old);
                    r.set_cursor(0);
                }
            }
        });

        let mut roots = vec![obj];
        let first = gc.young_collection(&mut roots, &NoopMonitors);
        let _ = gc.retry_after_evacuation_failure(first, &mut roots, &NoopMonitors);
        let raised = gc.needs_gc_free_percent.load(Ordering::Relaxed);
        assert!(
            raised > baseline,
            "an evacuation failure must trigger the next GC earlier ({raised} <= {baseline})"
        );
        assert!(raised <= 50, "the trigger must stay capped at 50%");

        // A clean pause (empty pointer map => no identity forwards) decays it.
        let clean = GcResult {
            stats: GcStats {
                objects_copied: 0,
                bytes_copied: 0,
                bytes_freed: 0,
            },
            pointer_map: cratonvm_types::PointerMap::default(),
        };
        let mut no_roots: Vec<ObjectRef> = Vec::new();
        let _ = gc.retry_after_evacuation_failure(clean, &mut no_roots, &NoopMonitors);
        assert!(
            gc.needs_gc_free_percent.load(Ordering::Relaxed) < raised,
            "a clean pause must decay the trigger back toward baseline"
        );
    }

    /// gci i18/mx (`CRATONVM_G1_YOUNG_SIZES_TO_RESERVE`, i16/oo findings 1-2)
    /// -- on the switch arm an evacuation failure leaves the Free-fraction
    /// trigger at its baseline and raises the survival prediction to 100 %
    /// instead, so the headroom arm (predicted survivors + the reserve) sizes
    /// the next young generation, as HotSpot does.
    #[test]
    pub fn gci_i18mx_reserve_sizing_answers_an_evacuation_failure_with_the_prediction() {
        let mut gc = make_collector();
        gc.young_sizes_to_reserve = true;
        let baseline = gc.needs_gc_free_percent.load(Ordering::Relaxed);
        gc.young_survival_permille.store(200, Ordering::Relaxed);

        let obj = gc.alloc_object(ClassId::new(1), 0);
        let idx = {
            let regions = gc.regions.read();
            gc.region_for_ptr(&regions, obj.as_ptr()).unwrap()
        };
        gc.with_regions_mut(|regions| {
            for (i, r) in regions.iter_mut().enumerate() {
                if i != idx {
                    r.set_region_type(RegionType::Old);
                    r.set_cursor(0);
                }
            }
        });
        let mut roots = vec![obj];
        let first = gc.young_collection(&mut roots, &NoopMonitors);
        assert!(
            first.pointer_map.iter().any(|(k, v)| k == v),
            "the fixture must fail evacuation"
        );
        let _ = gc.retry_after_evacuation_failure(first, &mut roots, &NoopMonitors);
        assert_eq!(
            gc.needs_gc_free_percent.load(Ordering::Relaxed),
            baseline,
            "the switch arm does not ratchet the Free-fraction trigger"
        );
        assert_eq!(
            gc.young_survival_permille.load(Ordering::Relaxed),
            1000,
            "it raises the survival prediction instead"
        );
    }

    /// gci i18/mx -- the Free-fraction predicate on both arms: the default
    /// arm is `free * 100 < total * pct` exactly as before; the reserve arm
    /// fires only at the `G1ReservePercent` floor.
    #[test]
    pub fn gci_i18mx_the_free_fraction_arm_fires_at_the_reserve_on_the_switch() {
        // 96 regions (the -Xmx96m probe at 1 MiB regions), trigger 25 %.
        for free in 0..=96usize {
            assert_eq!(
                free_fraction_trigger_fires(free, 96, 25, false),
                free * 100 < 96 * 25,
                "default arm, free {free}"
            );
        }
        assert_eq!(to_space_reserve_regions(96), 9);
        assert!(free_fraction_trigger_fires(9, 96, 25, true));
        assert!(free_fraction_trigger_fires(0, 96, 25, true));
        assert!(!free_fraction_trigger_fires(10, 96, 25, true));
        // At 71 of 96 MB used the default arm fires with 24 Free and young
        // collections stay a few regions big; the reserve arm waits.
        assert!(free_fraction_trigger_fires(23, 96, 25, false));
        assert!(!free_fraction_trigger_fires(23, 96, 25, true));
        // The ratchet's 50 % does not reach the reserve arm.
        assert!(free_fraction_trigger_fires(47, 96, 50, false));
        assert!(!free_fraction_trigger_fires(47, 96, 50, true));
        // A tiny heap keeps one reserve region.
        assert!(free_fraction_trigger_fires(1, 4, 25, true));
        assert!(!free_fraction_trigger_fires(2, 4, 25, true));
    }


    /// G1AUD-1 — promotion must stamp `GC_FLAG_OLD_GEN`.
    ///
    /// G1 itself never reads the bit; the JIT does. Every inline
    /// reference-store fast path in `jit/src/x64.rs` treats a receiver with the
    /// bit clear as "young, no post barrier needed". On an unstamped G1 heap
    /// every promoted object read as young, so a JIT-compiled null->non-null
    /// store into it skipped `post_write_barrier_rset` and the old->young edge
    /// never reached the remembered set.
    #[test]
    pub fn promotion_stamps_the_old_generation_bit_the_jit_barrier_reads() {
        let mut cfg = small_config();
        cfg.promotion_age = 1;
        let gc = G1Collector::new(cfg);

        let obj = gc.alloc_object(ClassId::new(1), 1);
        assert_eq!(
            gc.get_header(obj).gc_flags() & GC_FLAG_OLD_GEN,
            0,
            "a freshly allocated Eden object is young"
        );

        // Pass 1: age 0 < promotion_age 1 => Survivor, still young.
        let mut roots = vec![obj];
        gc.young_collection(&mut roots, &NoopMonitors);
        assert_eq!(
            gc.get_header(roots[0]).gc_flags() & GC_FLAG_OLD_GEN,
            0,
            "a Survivor copy is still young and MUST NOT be stamped — stamping it \
             would send every JIT store on a survivor down the helper for nothing"
        );

        // Pass 2: age 1 >= promotion_age 1 => promoted to Old, stamped.
        gc.young_collection(&mut roots, &NoopMonitors);
        let promoted = gc
            .lookup_region_for_addr(roots[0].as_ptr() as usize)
            .expect("promoted object is in a region");
        assert_eq!(
            gc.regions.read()[promoted].region_type(),
            RegionType::Old,
            "the object should have been promoted by the second pass"
        );
        assert_ne!(
            gc.get_header(roots[0]).gc_flags() & GC_FLAG_OLD_GEN,
            0,
            "a promoted object MUST carry GC_FLAG_OLD_GEN so the JIT's inline \
             reference-store fast paths bail to the full-barrier helper"
        );
    }

    /// The stamp must not clobber the layout bit that decides how the object is
    /// read back — a promoted compact instance that lost `GC_FLAG_COMPACT`
    /// would be walked with the legacy 16-byte-cell stride.
    #[test]
    pub fn the_old_generation_stamp_preserves_every_other_header_flag() {
        let mut cfg = small_config();
        cfg.promotion_age = 1;
        let gc = G1Collector::new(cfg);

        let obj = gc.alloc_object(ClassId::new(1), 1);
        // `GC_FLAG_MARKED` is the safe sentinel here: G1 keeps liveness in
        // per-region bitmaps and never reads this bit, whereas `GC_FLAG_COMPACT`
        // changes how `object_total_size` measures the object and would make
        // the test's own evacuation stride disagree with the allocation.
        let sentinel = cratonvm_types::GC_FLAG_MARKED;
        unsafe {
            let h = &mut *(obj.as_ptr() as *mut ObjectHeader);
            h.add_gc_flags(sentinel);
        }

        let mut roots = vec![obj];
        gc.young_collection(&mut roots, &NoopMonitors);
        gc.young_collection(&mut roots, &NoopMonitors);

        let flags = gc.get_header(roots[0]).gc_flags();
        assert_ne!(flags & GC_FLAG_OLD_GEN, 0, "promoted");
        assert_ne!(
            flags & sentinel,
            0,
            "the promotion stamp must OR the old-gen bit in, never overwrite the \
             flags byte"
        );
    }

    /// G1AUD-2 — `is_marking_active() => satb_queue.is_active()`.
    ///
    /// A reference store reads its old slot value on the PHASE and retains it
    /// on the QUEUE. A window where the phase says "marking" but the queue is
    /// off silently discards every edge overwritten in it.
    #[test]
    pub fn the_satb_gate_is_never_half_open_across_a_whole_cycle() {
        let gc = make_collector();
        let check = |where_: &str| {
            if gc.gc_state.is_marking_active() {
                assert!(
                    gc.satb_queue.is_active(),
                    "SATB gate half-open at {where_}: marking is active but the \
                     queue is not, so overwritten references are being dropped"
                );
            }
        };

        check("idle");
        gc.start_concurrent_mark(&stw());
        check("after start_concurrent_mark");
        assert!(gc.gc_state.is_marking_active() && gc.satb_queue.is_active());

        gc.remark(&stw(), &[]);
        check("after remark");

        gc.abort_concurrent_mark();
        check("after abort");
        assert!(
            !gc.gc_state.is_marking_active(),
            "abort must leave the marking phase"
        );
        assert!(!gc.satb_queue.is_active(), "abort must close the queue");
    }

    /// The abort path used to deactivate the queue BEFORE leaving the marking
    /// phase, which is the only ordering in the collector that opened the
    /// window above. Pin the order explicitly, because reversing it back would
    /// still leave the end state this test's sibling checks.
    #[test]
    pub fn abort_leaves_the_marking_phase_before_closing_the_satb_queue() {
        let gc = make_collector();
        gc.start_concurrent_mark(&stw());
        assert!(gc.gc_state.is_marking_active());
        assert!(gc.satb_queue.is_active());
        gc.abort_concurrent_mark();
        // Both ends observable only after the fact, so assert the invariant
        // that the ordering exists to preserve: at no point may the phase be
        // marking-active with the queue closed. The end state has BOTH off.
        assert!(!gc.gc_state.is_marking_active());
        assert!(!gc.satb_queue.is_active());
    }

    /// SATB completeness for the store paths this crate owns: a reference
    /// overwrite performed through `set_field` while marking is active must
    /// deliver the OLD value to the marker, not the new one.
    #[test]
    pub fn set_field_logs_the_overwritten_reference_while_marking() {
        let gc = make_collector();
        let holder = gc.alloc_object(ClassId::new(1), 1);
        let old = gc.alloc_object(ClassId::new(2), 0);
        let new = gc.alloc_object(ClassId::new(3), 0);
        gc.set_field(holder, 0, Value::Object(Some(old)));

        gc.start_concurrent_mark(&stw());
        gc.set_field(holder, 0, Value::Object(Some(new)));
        cratonvm_gc_core::satb::flush_thread_satb_buffer(gc.satb_queue());

        let logged = gc.satb_queue().drain();
        assert!(
            logged.contains(&(old.as_ptr() as usize)),
            "the pre-barrier must log the OLD value: {logged:?}"
        );
        assert!(
            !logged.contains(&(new.as_ptr() as usize)),
            "the pre-barrier must not log the value being stored: {logged:?}"
        );
    }

    /// Same obligation on the array path — `set_array_element` reads the old
    /// element inside the SAME regions critical section as the store.
    #[test]
    pub fn r23_l5_raw_ref_array_copy_is_admitted_only_without_marking_or_humongous() {
        let gc = make_collector();
        let small = ARRAY_DATA_OFFSET + 10 * 8;
        assert!(gc.raw_ref_array_copy_admitted(small, small));
        // Anything the accessors would reach through a humongous span.
        let huge = gc.config.region_size;
        assert!(!gc.raw_ref_array_copy_admitted(huge, small));
        assert!(!gc.raw_ref_array_copy_admitted(small, huge));
        // A marking window needs the per-element SATB read.
        gc.start_concurrent_mark(&stw());
        assert!(!gc.raw_ref_array_copy_admitted(small, small));
    }

    #[test]
    pub fn set_array_element_logs_the_overwritten_reference_while_marking() {
        let gc = make_collector();
        let arr = gc.alloc_array(ClassId::new(1), ArrayElementType::Reference, 2);
        let old = gc.alloc_object(ClassId::new(2), 0);
        let new = gc.alloc_object(ClassId::new(3), 0);
        gc.set_array_element(arr, 0, Value::Object(Some(old)))
            .expect("in bounds");

        gc.start_concurrent_mark(&stw());
        gc.set_array_element(arr, 0, Value::Object(Some(new)))
            .expect("in bounds");
        cratonvm_gc_core::satb::flush_thread_satb_buffer(gc.satb_queue());

        let logged = gc.satb_queue().drain();
        assert!(
            logged.contains(&(old.as_ptr() as usize)),
            "aastore's pre-barrier must log the OLD element: {logged:?}"
        );
    }

    /// No SATB traffic at all outside a mark cycle: the barrier's cost when
    /// idle is one Acquire load, and a run that logs while idle would grow the
    /// shards without bound with nobody to drain them.
    #[test]
    pub fn no_reference_is_logged_when_no_mark_cycle_is_active() {
        let gc = make_collector();
        let holder = gc.alloc_object(ClassId::new(1), 1);
        let old = gc.alloc_object(ClassId::new(2), 0);
        let new = gc.alloc_object(ClassId::new(3), 0);
        gc.set_field(holder, 0, Value::Object(Some(old)));
        gc.set_field(holder, 0, Value::Object(Some(new)));
        cratonvm_gc_core::satb::flush_thread_satb_buffer(gc.satb_queue());
        assert!(gc.satb_queue().is_empty());
    }

    /// Retype the region holding `obj` and hand back its index.
    pub fn retype_region_of(gc: &G1Collector, addr: usize, ty: RegionType) -> usize {
        let idx = gc
            .lookup_region_for_addr(addr)
            .expect("address is inside a region");
        gc.with_regions_mut(|rs| rs[idx].set_region_type(ty));
        idx
    }

    /// G1AUD-3 — cleanup's in-place free is sound only on a COMPLETE closure.
    /// The baseline: with the gray set drained, a zero-live Old region is
    /// recycled.
    #[test]
    pub fn cleanup_frees_a_zero_live_old_region_when_the_closure_is_complete() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let idx = retype_region_of(&gc, obj.as_ptr() as usize, RegionType::Old);

        gc.start_concurrent_mark(&stw());
        assert!(gc.mark_worklist.lock().is_empty());
        gc.cleanup(&stw());

        assert_eq!(
            gc.regions.read()[idx].region_type(),
            RegionType::Free,
            "an unmarked Old region under a complete closure is garbage"
        );
    }

    /// ...and the fail-safe: with a gray entry still outstanding, "unmarked"
    /// does not imply "unreachable", so nothing may be freed. The precondition
    /// was documented on `VmHeap::g1_final_remark_and_cleanup` and enforced by
    /// nothing — and its sibling `g1_signal_marking_complete` calls `cleanup()`
    /// with no remark and no drain at all.
    #[test]
    pub fn cleanup_with_an_undrained_gray_set_retains_every_region() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let idx = retype_region_of(&gc, obj.as_ptr() as usize, RegionType::Old);

        gc.start_concurrent_mark(&stw());
        // One gray entry the marker never got to. Its address is irrelevant —
        // what matters is that the closure is not at a fixed point.
        gc.mark_worklist.lock().push(obj.as_ptr() as usize);
        gc.cleanup(&stw());

        assert_eq!(
            gc.regions.read()[idx].region_type(),
            RegionType::Old,
            "cleanup must NOT free a region on an incomplete mark closure"
        );
        let facts = last_g1_cycle().expect("cleanup records a cycle");
        assert_eq!(facts.kind, g1_cycle_kind::CONCURRENT_CLEANUP);
        assert_ne!(
            facts.degraded & g1_degraded::CLEANUP_CLOSURE_INCOMPLETE,
            0,
            "the fail-safe must be visible in the cycle record, not silent"
        );
    }

    /// The humongous reclaimer trusts the same closure, so it must be
    /// suppressed by the same fail-safe.
    #[test]
    pub fn cleanup_with_an_undrained_gray_set_also_spares_humongous_spans() {
        let gc = make_collector();
        // Two regions' worth: HumongousStart + one continuation.
        let big = gc.alloc_array(
            ClassId::new(1),
            ArrayElementType::Long,
            small_config().region_size / 8,
        );
        let start = gc
            .lookup_region_for_addr(big.as_ptr() as usize)
            .expect("humongous start region");
        assert_eq!(
            gc.regions.read()[start].region_type(),
            RegionType::HumongousStart
        );

        gc.start_concurrent_mark(&stw());
        gc.mark_worklist.lock().push(big.as_ptr() as usize);
        gc.cleanup(&stw());

        assert_eq!(
            gc.regions.read()[start].region_type(),
            RegionType::HumongousStart,
            "an unmarked humongous span must survive an incomplete closure"
        );
    }

    /// A humongous span occupies a physically contiguous run: one
    /// `HumongousStart` carrying the whole object size as its cursor, then
    /// `HumongousContinuation` regions with cursor 0 so walkers skip them.
    /// Every walker in this file derives the span extent from that shape.
    #[test]
    pub fn a_humongous_span_is_a_contiguous_start_plus_continuations() {
        let gc = make_collector();
        let region_size = small_config().region_size;
        let big = gc.alloc_array(ClassId::new(1), ArrayElementType::Long, region_size / 8);
        let start = gc
            .lookup_region_for_addr(big.as_ptr() as usize)
            .expect("humongous start region");

        let regions = gc.regions.read();
        assert_eq!(regions[start].region_type(), RegionType::HumongousStart);
        let total = regions[start].cursor();
        assert!(total > region_size, "the object spans more than one region");
        let needed = total.div_ceil(region_size).max(1);
        for r in &regions[start + 1..start + needed] {
            assert_eq!(
                r.region_type(),
                RegionType::HumongousContinuation,
                "every region a span owns must be typed as a continuation — the \
                 reclaimer refuses to free a span whose shape disagrees"
            );
            assert_eq!(r.cursor(), 0, "continuations must be invisible to walkers");
        }
        // The object's first and last byte are inside the reserved run.
        let base = regions[start].data.addr();
        assert_eq!(big.as_ptr() as usize, base);
        assert!(total <= needed * region_size);
    }

    /// Region pinning, no-relocation vocabulary: a pinned region must never
    /// enter a collection set, because Phase 5 zero-fills and re-types every
    /// CSet region that holds no self-forwarded object. That is what lets
    /// `pin_region_for_addr` promise a JNI critical section a stable address.
    /// Audit §9 item 5 — the remembered set is bounded, and bounding it does
    /// not lose objects.
    ///
    /// The bound is coarsening: past `rset_source_cap()` distinct sources an
    /// rset stops naming them and asserts only "some region points into me".
    /// That is a memory/scan-time trade, and it is only sound if the scan side
    /// reads a coarsened set as the SUPERSET — every plausible source — rather
    /// than as the empty set it now physically contains. Reading it the cheap
    /// way would drop exactly the live objects the remembered set exists to
    /// find, which is why the second half of this test runs a real collection
    /// rather than stopping at the flag.
    #[test]
    pub fn a_coarsened_remembered_set_still_finds_every_live_edge() {
        let gc = make_collector();

        // --- the flag, and what it does to the source set ----------------
        let target = 1usize;
        {
            let regions = gc.regions.read();
            // Cap of 2: the third DISTINCT source coarsens.
            regions[target]
                .rset
                .add_reference_in_generation_within(3, 0, 2);
            regions[target]
                .rset
                .add_reference_in_generation_within(4, 0, 2);
            assert!(!regions[target].rset.is_coarsened(), "still under the cap");
            regions[target]
                .rset
                .add_reference_in_generation_within(5, 0, 2);
            assert!(
                regions[target].rset.is_coarsened(),
                "exceeding the cap must coarsen rather than grow"
            );
            assert_eq!(
                regions[target].rset.source_count(),
                0,
                "coarsening releases the precise set — that is the whole point \
                 of the bound"
            );

            let sources = G1Collector::live_rset_sources(&regions, &[target]);
            for (i, r) in regions.iter().enumerate() {
                if i != target && r.region_type() != RegionType::Free {
                    assert!(
                        sources.contains(&i),
                        "a coarsened rset must be read as EVERY plausible \
                         source; region {i} is missing"
                    );
                }
            }
        }

        // --- and the behaviour: no object is lost by the bound -----------
        //
        // P -> Q across regions, with the recorded entry then destroyed by
        // coarsening. Nothing names P any more, so Q survives only if the
        // coarsened read is the superset.
        let gc2 = make_collector();
        let p = gc2.alloc_object(ClassId::new(1), 1);
        let p_region = gc2.lookup_region_for_addr(p.as_ptr() as usize).unwrap();
        gc2.clear_eden_slots();
        let q = gc2.alloc_object(ClassId::new(2), 1);
        let q_region = gc2.lookup_region_for_addr(q.as_ptr() as usize).unwrap();
        assert_ne!(p_region, q_region);
        gc2.set_field(q, 0, Value::Int(9191));
        gc2.set_field(p, 0, Value::Object(Some(q)));
        // Hold P's region out of the CSet so it is a source rather than a
        // collection target, exactly as a JNI pin would.
        gc2.pin_region(p_region);

        {
            // Coarsen Q's rset, which destroys the entry that named P. A cap of
            // 1 coarsens on the second distinct source.
            let regions = gc2.regions.read();
            regions[q_region]
                .rset
                .add_reference_in_generation_within(999_999, 0, 1);
            regions[q_region]
                .rset
                .add_reference_in_generation_within(999_998, 0, 1);
            assert!(regions[q_region].rset.is_coarsened());
            assert!(
                !regions[q_region].rset.sources().contains(&p_region),
                "the entry naming P is gone — this test would pass vacuously otherwise"
            );
        }

        let q_addr = q.as_ptr() as usize;
        let mut roots: Vec<ObjectRef> = vec![];
        let result = gc2.young_collection(&mut roots, &NoopMonitors);
        let q_new = result
            .pointer_map
            .get(&q_addr)
            .copied()
            .expect("a coarsened remembered set must still reach Q through the wholesale walk");
        match gc2.get_field(p, 0) {
            Value::Object(Some(r)) => assert_eq!(r.as_ptr() as usize, q_new),
            other => panic!("P's reference was lost under coarsening: {other:?}"),
        }
        assert_eq!(gc2.get_field(q_new_ref(q_new), 0).as_int(), Some(9191));
    }

    /// Helper: rebuild an `ObjectRef` from a post-GC address.
    pub fn q_new_ref(addr: usize) -> ObjectRef {
        // SAFETY: `addr` came out of this pause's pointer map, so it names a
        // live, 8-byte-aligned to-space object.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// Audit §9 item 6 — the JNI-pinned-source policy, stated and pinned.
    ///
    /// There are two no-relocation pin vocabularies and they are covered
    /// DIFFERENTLY as remembered-set sources. Until now that difference lived in
    /// a comment, which is what the audit item objects to:
    ///
    /// * a **JNI-pinned** region (`G1Region::pinned` / `pin_count`, JEP 423) is
    ///   held out of the collection set but is an ORDINARY remembered-set
    ///   source. A CSet object referenced only from it survives because the
    ///   post-write barrier recorded that edge. Nothing walks the region
    ///   wholesale.
    /// * a **JIT-pinned** region is additionally walked WHOLESALE (see
    ///   `a_jit_pinned_region_is_a_wholesale_rset_source_on_the_parallel_path_too`),
    ///   because `jit_pinned_region_set()` also contains every region holding a
    ///   published un-retired TLAB tail — memory no barrier ever saw.
    ///
    /// THE POLICY, stated: the JNI half is barrier-covered, the JIT half is
    /// walk-covered, and the asymmetry is deliberate. It is sound exactly while
    /// every store that can install a reference out of a JNI-pinned region
    /// fires the barrier — which is what G1-2 was about, and G1-2 is closed:
    /// under a backend that publishes no live region bounds (G1, ZGC) every
    /// inline JIT reference store bails to `jit_putfield_object`, which fires
    /// the collector's own barrier. If that ever stops being true, this test's
    /// second half is the one that starts lying, and the fix is to promote
    /// JNI-pinned regions to wholesale sources too.
    #[test]
    pub fn a_jni_pinned_region_is_an_ordinary_rset_source_not_a_wholesale_one() {
        // --- the barrier-covered case: the edge is recorded, so Q survives ---
        let gc = make_collector();
        let p = gc.alloc_object(ClassId::new(1), 1);
        let p_region = gc.lookup_region_for_addr(p.as_ptr() as usize).unwrap();
        // Retire the current Eden so Q lands in a different region.
        gc.clear_eden_slots();
        let q = gc.alloc_object(ClassId::new(2), 1);
        let q_region = gc.lookup_region_for_addr(q.as_ptr() as usize).unwrap();
        assert_ne!(p_region, q_region, "Q must be cross-region from P");
        gc.set_field(q, 0, Value::Int(2424));
        gc.set_field(p, 0, Value::Object(Some(q))); // fires the post-write barrier

        gc.pin_region(p_region);
        assert!(gc.is_pinned(p_region));
        assert!(
            !gc.jit_pinned_region_set().contains(&p_region),
            "a JNI pin must NOT put the region in the JIT-pinned set — the two \
             vocabularies are separate, and conflating them is what would make \
             this policy vacuous"
        );

        // The mechanism: P's region is reachable as an ORDINARY rset source of
        // the CSet, i.e. because the barrier recorded the edge.
        {
            let regions = gc.regions.read();
            let sources = G1Collector::live_rset_sources(&regions, &[q_region]);
            assert!(
                sources.contains(&p_region),
                "the JNI-pinned holder must appear as a remembered-set source \
                 of the region it points into"
            );
        }

        let q_addr = q.as_ptr() as usize;
        let mut roots: Vec<ObjectRef> = vec![];
        let result = gc.young_collection(&mut roots, &NoopMonitors);
        let q_new = result
            .pointer_map
            .get(&q_addr)
            .copied()
            .expect("Q is live through the pinned region's remembered set");
        match gc.get_field(p, 0) {
            Value::Object(Some(r)) => assert_eq!(
                r.as_ptr() as usize,
                q_new,
                "P's slot must be rewritten to Q's new address"
            ),
            other => panic!("P's reference was lost: {other:?}"),
        }

        // --- the dependency, made explicit -------------------------------
        //
        // Same shape, but with the barrier's record erased: the JNI-pinned
        // region then covers NOTHING. Asserted at the source-set level rather
        // than by running a pause, deliberately — actually collecting with the
        // record erased drops a live object and leaves P pointing into a freed
        // region, which is precisely the `[g1][SECURITY V7b]` condition the
        // post-evacuation verifier exists to catch, and it does catch it. A
        // test should not have to corrupt the heap to state where a region's
        // coverage comes from.
        let gc2 = make_collector();
        let p2 = gc2.alloc_object(ClassId::new(1), 1);
        let p2_region = gc2.lookup_region_for_addr(p2.as_ptr() as usize).unwrap();
        gc2.clear_eden_slots();
        let q2 = gc2.alloc_object(ClassId::new(2), 1);
        let q2_region = gc2.lookup_region_for_addr(q2.as_ptr() as usize).unwrap();
        assert_ne!(p2_region, q2_region);
        gc2.set_field(p2, 0, Value::Object(Some(q2)));
        gc2.pin_region(p2_region);
        gc2.regions.read()[q2_region].rset.clear();

        let regions = gc2.regions.read();
        assert!(
            !G1Collector::live_rset_sources(&regions, &[q2_region]).contains(&p2_region),
            "with the entry erased the pinned holder is no longer a source"
        );
        assert!(
            !gc2.jit_pinned_region_set().contains(&p2_region),
            "and nothing else would walk it: a JNI pin is not a JIT pin, so the remembered set is the WHOLE of a JNI-pinned region's coverage. If this ever stops holding, the policy above needs rewriting rather than this assertion"
        );
    }

    #[test]
    pub fn a_pinned_region_is_never_evacuated() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let before = obj.as_ptr() as usize;
        let idx = gc.pin_region_for_addr(before).expect("region for object");
        assert!(gc.is_pinned(idx));

        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        assert_eq!(
            roots[0].as_ptr() as usize,
            before,
            "an object in a pinned region must not move"
        );
        assert!(
            result.pointer_map.is_empty(),
            "a pinned-only heap has nothing to evacuate"
        );
        assert_ne!(
            gc.regions.read()[idx].region_type(),
            RegionType::Free,
            "a pinned region must not be freed"
        );
        let facts = last_g1_cycle().expect("the pause records a cycle");
        assert_ne!(
            facts.degraded & g1_degraded::JNI_PINNED_REGIONS_EXCLUDED,
            0,
            "a pause that could not collect because of a pin must say so"
        );
    }

    /// Pins are refcounted: overlapping critical sections release
    /// independently, and the region only becomes collectable again when the
    /// last one is gone.
    #[test]
    pub fn overlapping_pins_release_independently() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let idx = gc.pin_region_for_addr(obj.as_ptr() as usize).unwrap();
        let idx2 = gc.pin_region_for_addr(obj.as_ptr() as usize).unwrap();
        assert_eq!(idx, idx2);

        gc.unpin_region(idx);
        assert!(
            gc.is_pinned(idx),
            "one release must not drop the other section's pin"
        );
        gc.unpin_region(idx);
        assert!(!gc.is_pinned(idx));

        // Unbalanced extra release must not underflow into a wrong state.
        gc.unpin_region(idx);
        assert!(!gc.is_pinned(idx));
    }

    /// Evacuation failure: with no to-space anywhere, a reached object is
    /// self-forwarded IN PLACE and its region is kept rather than freed.
    /// Dropping it instead would leave every referrer pointing into a region
    /// Phase 5 had just reset.
    #[test]
    pub fn evacuation_failure_self_forwards_and_keeps_the_region() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let before = obj.as_ptr() as usize;
        let home = gc
            .lookup_region_for_addr(before)
            .expect("object is in a region");

        // Put EVERY region into the collection set (Eden), so
        // `alloc_in_type_locked` can find neither a non-CSet destination nor a
        // Free region.
        gc.with_regions_mut(|rs| {
            for r in rs.iter_mut() {
                r.set_region_type(RegionType::Eden);
            }
        });

        let mut roots = vec![obj];
        let result = gc.young_collection(&mut roots, &NoopMonitors);

        assert_eq!(
            roots[0].as_ptr() as usize,
            before,
            "a self-forwarded object stays at its own address"
        );
        assert_eq!(
            result.pointer_map.get(&before),
            Some(&before),
            "the identity forward is what Phase 5 reads to keep the region"
        );
        assert_eq!(
            gc.regions.read()[home].region_type(),
            RegionType::Old,
            "a kept Eden region is retyped to Old, not freed"
        );
        let facts = last_g1_cycle().expect("the pause records a cycle");
        assert_ne!(
            facts.degraded & g1_degraded::EVACUATION_FAILURE,
            0,
            "an evacuation failure must be visible in the cycle record"
        );
    }


    /// Every producer of a remembered-set entry must also dirty the holder's
    /// card, because Phase 2 now uses the card to decide whether to look at the
    /// region the entry names. A producer that records the entry and not the
    /// card names a region Phase 2 then skips: the referent is not evacuated
    /// and Phase 5 frees the region it lives in.
    ///
    /// Producer 1 of 3: the mutator post-write barrier.
    /// (Its test is `the_mutator_barrier_dirties_the_holders_card`, below the
    /// card-cleaning group.)
    ///
    /// Card cleaning — the defect this exists to remove, stated as a test.
    ///
    /// Before cleaning, the ONLY thing that cleared a card was
    /// `G1Region::reset`. So a card dirtied by a store whose reference was
    /// later dropped stayed dirty for the life of the region's contents, and a
    /// long-lived Old region saturated: every card dirty, the screen answering
    /// "scan it" for everything, which is the 0.2% skip rate this branch
    /// measured.
    #[test]
    pub fn a_card_whose_edge_is_gone_is_cleaned_by_the_pause_that_walks_it() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_G1_CARD_CLEAN", Some("1"))],
            || {
                let gc = make_collector();
                // A holder in what will become an Old source region, pointing across
                // regions so the barrier dirties its card.
                let holder = gc.alloc_object(ClassId::new(1), 1);
                let holder_addr = holder.as_ptr() as usize;
                let src = gc.lookup_region_for_addr(holder_addr).expect("region");
                let mut target = gc.alloc_object(ClassId::new(2), 1);
                while gc.lookup_region_for_addr(target.as_ptr() as usize) == Some(src) {
                    target = gc.alloc_object(ClassId::new(2), 1);
                }
                gc.set_field(holder, 0, Value::Object(Some(target)));
                assert!(
                    gc.cards().is_dirty_addr(holder_addr),
                    "the barrier must have dirtied the holder's card"
                );

                // The edge goes away. Nothing cleans the card at the store — the
                // barrier only ever sets bits — so it is still dirty here, and only a
                // walk can discover that it no longer means anything.
                gc.set_field(holder, 0, Value::Object(None));
                assert!(gc.cards().is_dirty_addr(holder_addr));

                gc.with_regions_mut(|rs| rs[src].set_region_type(RegionType::Old));
                let mut roots: Vec<ObjectRef> = vec![holder];
                gc.young_collection_serial(&mut roots, &NoopMonitors);

                assert!(
                    !gc.cards().is_dirty_addr(holder_addr),
                    "a pause that walked the holder and found no cross-region reference \
             must leave its card CLEAN; leaving it dirty is what made the screen \
             useless on long-lived regions"
                );
            },
        );
    }

    /// …and the other direction, which is the soundness half: a card whose
    /// edge is still there must survive the same walk.
    #[test]
    pub fn a_card_whose_edge_survives_is_left_dirty_by_the_walk() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_G1_CARD_CLEAN", Some("1"))],
            || {
                let gc = make_collector();
                let holder = gc.alloc_object(ClassId::new(1), 1);
                let holder_addr = holder.as_ptr() as usize;
                let src = gc.lookup_region_for_addr(holder_addr).expect("region");
                let mut target = gc.alloc_object(ClassId::new(2), 1);
                while gc.lookup_region_for_addr(target.as_ptr() as usize) == Some(src) {
                    target = gc.alloc_object(ClassId::new(2), 1);
                }
                gc.set_field(target, 0, Value::Int(0x5AFE));
                gc.set_field(holder, 0, Value::Object(Some(target)));

                gc.with_regions_mut(|rs| rs[src].set_region_type(RegionType::Old));
                let mut roots: Vec<ObjectRef> = vec![holder];
                gc.young_collection_serial(&mut roots, &NoopMonitors);

                assert!(
                    gc.cards().is_dirty_addr(holder_addr),
                    "the holder still references another region, so its card must stay \
             dirty — cleaning it would make the NEXT pause step over a live edge"
                );
                // And the edge really is still live and followable.
                match gc.get_field(holder, 0) {
                    Value::Object(Some(t)) => {
                        assert_eq!(gc.get_field(t, 0).as_int(), Some(0x5AFE))
                    }
                    other => panic!("the holder lost its reference: {other:?}"),
                }
            },
        );
    }

    /// Card cleaning must never outrun the walk. A walk that stops early has
    /// not examined the bytes past the break, and cleaning those would drop
    /// edges nothing looked at — so the clean is bounded by what was walked.
    #[test]
    pub fn cleaning_is_bounded_by_what_the_walk_examined() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_G1_CARD_CLEAN", Some("1"))],
            || {
                let gc = make_collector();
                // The target lives in Eden and will be collected.
                let target = gc.alloc_object(ClassId::new(2), 1);
                let target_region = gc
                    .lookup_region_for_addr(target.as_ptr() as usize)
                    .expect("region");

                // The source is a hand-built Old region holding ONE object, so its
                // cursor stays far below its extent and there are cards past it. An
                // `alloc_object` loop would not do: escaping a region means FILLING it,
                // and then there is nothing past the cursor left to probe — which is
                // how the first version of this test managed to assert nothing (it
                // also compared an address against an offset).
                let (holder, src, far_addr) = {
                    let mut regions = gc.regions.write();
                    let src = regions
                        .iter()
                        .position(|r| r.region_type() == RegionType::Free)
                        .expect("a free region");
                    regions[src].set_region_type(RegionType::Old);
                    let (ptr, _) = regions[src]
                        .bump_alloc(HEADER_SIZE + SLOT_SIZE, 8, "test")
                        .expect("room for one object");
                    let header = ObjectHeader::new(
                        ClassId::new(1),
                        ObjectKind::Object,
                        ArrayElementType::Reference,
                        0,
                        1,
                    );
                    unsafe { std::ptr::write(ptr as *mut ObjectHeader, header) };
                    let far = regions[src].data.as_ptr() as usize + regions[src].data.len() - 64;
                    (unsafe { ObjectRef::from_raw(ptr) }, src, far)
                };
                gc.with_regions_mut(|_| {});
                assert_ne!(src, target_region);
                gc.set_field(holder, 0, Value::Object(Some(target)));

                // A card past the cursor: no walk reaches those bytes, so no walk may
                // clean them.
                let cursor_addr = {
                    let regions = gc.regions.read();
                    regions[src].data.as_ptr() as usize + regions[src].cursor()
                };
                assert!(
            far_addr > cursor_addr,
            "the probe must sit past the walked extent (far={far_addr:#x} cursor={cursor_addr:#x})"
        );
                gc.cards().dirty_addr(far_addr);

                let mut roots: Vec<ObjectRef> = vec![holder];
                gc.young_collection_serial(&mut roots, &NoopMonitors);

                assert!(
                    gc.cards().is_dirty_addr(far_addr),
                    "a card past the walked extent must be left alone"
                );
            },
        );
    }

    /// Producer 1 of 3: the mutator post-write barrier records the entry AND
    /// dirties the holder's card, because Phase 2 uses the card to decide
    /// whether to look at the region the entry names.
    #[test]
    pub fn the_mutator_barrier_dirties_the_holders_card() {
        let gc = make_collector();
        let holder = gc.alloc_object(ClassId::new(1), 1);
        let holder_addr = holder.as_ptr() as usize;
        let src = gc
            .lookup_region_for_addr(holder_addr)
            .expect("holder region");

        // A same-region store records no rset entry, so it must dirty no card
        // either -- the two structures have to agree about what an edge IS, or
        // the whole-region screen (which reads "no dirty card" as "no
        // cross-region edge") is answering a different question from the one
        // `live_rset_sources` asked.
        assert!(!gc.cards().is_dirty_addr(holder_addr));
        let same_region_target = {
            let regions = gc.regions.read();
            regions[src].data.as_ptr() as usize
        };
        gc.post_write_barrier_rset(holder, unsafe {
            ObjectRef::from_raw(same_region_target as *mut u8)
        });
        assert!(
            !gc.cards().is_dirty_addr(holder_addr),
            "a same-region store must leave the card clean, exactly as it leaves the rset empty"
        );

        // A cross-region store dirties the HOLDER's card (not the target's:
        // the card answers "where in the source is the slot", and Phase 2 walks
        // the source).
        let far_region = (src + 1) % gc.num_regions();
        let far_addr = {
            let mut regions = gc.regions.write();
            regions[far_region].set_region_type(RegionType::Old);
            let (ptr, _) = regions[far_region]
                .bump_alloc(HEADER_SIZE, 8, "test")
                .expect("room in a fresh region");
            ptr as usize
        };
        gc.post_write_barrier_rset(holder, unsafe { ObjectRef::from_raw(far_addr as *mut u8) });
        assert!(
            gc.cards().is_dirty_addr(holder_addr),
            "the cross-region post-write barrier must dirty the holder's card"
        );
        assert!(
            !gc.cards().is_dirty_addr(far_addr),
            "the TARGET's card says nothing; only the source is ever walked"
        );
    }

    /// Producer 2 of 3: the Phase-4 GC-internal edge rebuild
    /// (`collect_outgoing_cross_region_edges`), which records the edges the
    /// COLLECTOR creates by copying or rewriting and for which no mutator
    /// barrier ever fires.
    ///
    /// The card store in that producer sits BEFORE its `seen` dedup, and this
    /// test is why: `seen` is keyed on the target region and cleared once per
    /// holder REGION, so the second and later objects of a region pointing into
    /// the same target are suppressed. Suppressing the rset push is right;
    /// suppressing the card would leave those objects on clean cards.
    #[test]
    pub fn the_phase4_edge_rebuild_dirties_every_holders_card_not_just_the_first() {
        let gc = make_collector();
        let holder_region = 1usize;
        let target_region = 2usize;
        let (a, b, target) = {
            let mut regions = gc.regions.write();
            regions[holder_region].set_region_type(RegionType::Old);
            regions[target_region].set_region_type(RegionType::Old);
            let (t, _) = regions[target_region]
                .bump_alloc(HEADER_SIZE, 8, "test")
                .expect("room");
            // Two holders far enough apart to land on different cards.
            let (a, _) = regions[holder_region]
                .bump_alloc(G1_CARD_BYTES, 8, "test")
                .expect("room");
            let (b, _) = regions[holder_region]
                .bump_alloc(HEADER_SIZE + SLOT_SIZE, 8, "test")
                .expect("room");
            (a, b, t)
        };
        // Give both holders one reference slot pointing at `target`.
        for h in [a, b] {
            unsafe {
                *(h as *mut ObjectHeader) = ObjectHeader::new(
                    ClassId::new(7),
                    ObjectKind::Object,
                    ArrayElementType::Reference,
                    0,
                    1,
                );
                // Legacy (non-compact) cell: a full 16-byte `Value`, which is
                // what the reference walkers decode. A bare pointer word here
                // is not a reference at all and the walk finds nothing.
                write_flat_object_reference(h.add(HEADER_SIZE), target as usize, false);
            }
        }
        assert!(
            !gc.cards().is_dirty_addr(a as usize) && !gc.cards().is_dirty_addr(b as usize),
            "nothing has fired a barrier for these hand-built objects"
        );

        let regions = gc.regions.read();
        let mut out: Vec<(usize, usize)> = Vec::new();
        let mut seen = RegionSet::new();
        for h in [a, b] {
            let header = unsafe { &*(h as *const ObjectHeader) };
            gc.collect_outgoing_cross_region_edges(
                &regions,
                holder_region,
                h,
                header,
                &mut out,
                &mut seen,
                None,
            );
        }
        drop(regions);
        assert_eq!(
            out.len(),
            1,
            "the rset edge is deduped per holder region -- that is the behaviour \
             the card store must NOT inherit"
        );
        assert!(gc.cards().is_dirty_addr(a as usize), "first holder's card");
        assert!(
            gc.cards().is_dirty_addr(b as usize),
            "the DEDUPED second holder's card must be dirty too, or Phase 2 steps over it"
        );
    }

    /// Producer 3 of 3: the evacuation-failure fixup
    /// (`record_outgoing_rset_edges`), which records the outgoing edges of
    /// objects a wedged drain left parked in kept regions.
    #[test]
    pub fn the_evacuation_failure_fixup_dirties_the_holders_card() {
        let gc = make_collector();
        let holder_region = 3usize;
        let target_region = 4usize;
        let (h, target) = {
            let mut regions = gc.regions.write();
            regions[holder_region].set_region_type(RegionType::Old);
            regions[target_region].set_region_type(RegionType::Old);
            let (t, _) = regions[target_region]
                .bump_alloc(HEADER_SIZE, 8, "test")
                .expect("room");
            let (h, _) = regions[holder_region]
                .bump_alloc(HEADER_SIZE + SLOT_SIZE, 8, "test")
                .expect("room");
            (h, t)
        };
        unsafe {
            *(h as *mut ObjectHeader) = ObjectHeader::new(
                ClassId::new(9),
                ObjectKind::Object,
                ArrayElementType::Reference,
                0,
                1,
            );
            *(target as *mut ObjectHeader) = ObjectHeader::new(
                ClassId::new(10),
                ObjectKind::Object,
                ArrayElementType::Reference,
                0,
                0,
            );
            write_flat_object_reference(h.add(HEADER_SIZE), target as usize, false);
        }
        assert!(!gc.cards().is_dirty_addr(h as usize));

        let mut regions = gc.regions.write();
        assert!(gc.record_outgoing_rset_edges(&mut regions, h as usize));
        assert!(
            regions[target_region]
                .rset
                .sources()
                .contains(&holder_region),
            "precondition: the fixup recorded the edge at all"
        );
        drop(regions);
        assert!(
            gc.cards().is_dirty_addr(h as usize),
            "the kept-region fixup must dirty the card beside the rset entry it records"
        );
    }

    /// A region's cards are cleared by `reset`, in the same statement that
    /// clears its remembered set. Leaving them dirty is not unsound but makes
    /// the screen useless within a few cycles, because a region recycled every
    /// young pause would accumulate dirty cards forever.
    #[test]
    pub fn resetting_a_region_cleans_its_cards_and_leaves_its_neighbours_alone() {
        let gc = make_collector();
        let (base_1, base_2) = {
            let regions = gc.regions.read();
            (
                regions[1].data.as_ptr() as usize,
                regions[2].data.as_ptr() as usize,
            )
        };
        gc.cards().dirty_addr(base_1);
        gc.cards().dirty_addr(base_1 + 4 * G1_CARD_BYTES);
        gc.cards().dirty_addr(base_2);
        gc.with_regions_mut(|rs| rs[1].reset(1));
        assert!(!gc.cards().is_dirty_addr(base_1));
        assert!(!gc.cards().is_dirty_addr(base_1 + 4 * G1_CARD_BYTES));
        assert!(
            gc.cards().is_dirty_addr(base_2),
            "region 2's cards are not region 1's to clear"
        );
    }

    /// The whole-region screen: a remembered-set entry naming a source with no
    /// dirty card is a stale entry, and Phase 2 skips the region rather than
    /// walking a megabyte for it.
    ///
    /// This is the shape F-05 is about. An rset entry is additive -- its only
    /// pruning is `cleanup`'s recycled-source pass -- so an entry routinely
    /// outlives the edge that created it, and a COARSENED rset nominates every
    /// live region in the heap.
    ///
    /// Driven through `young_collection_serial` and, in the twin below,
    /// `young_collection_parallel`. Calling the dispatching `young_collection`
    /// would have exercised only whichever arm the default flags select — this
    /// file's history has a test that silently measured the parallel evacuator
    /// while claiming to cover the serial one, and the two arms have their own
    /// copies of this walk.
    #[test]
    pub fn a_source_region_with_no_dirty_card_is_skipped_entirely() {
        // The kill switch turns off the very thing this test measures. Say so
        // and skip, rather than going red on a deliberate A/B arm — the test's
        // ability to fail is established against the DEFAULT (on) arm, which is
        // the arm that ships.
        let gc = make_collector();
        // Q lives in Eden and will be in the collection set.
        let q = gc.alloc_object(ClassId::new(2), 1);
        let q_region = gc
            .lookup_region_for_addr(q.as_ptr() as usize)
            .expect("q region");

        // A stale entry: region `stale` is named as a source of Q's region but
        // holds no cross-region reference (nothing ever dirtied its cards).
        // It is given real, walkable contents so that "skipped" is a claim
        // about the screen and not about an empty region.
        let stale = (q_region + 3) % gc.num_regions();
        let stale_base = {
            let mut regions = gc.regions.write();
            regions[stale].set_region_type(RegionType::Old);
            for _ in 0..64 {
                let (p, _) = regions[stale]
                    .bump_alloc(HEADER_SIZE, 8, "test")
                    .expect("room");
                unsafe {
                    *(p as *mut ObjectHeader) = ObjectHeader::new(
                        ClassId::new(5),
                        ObjectKind::Object,
                        ArrayElementType::Reference,
                        0,
                        0,
                    )
                };
            }
            regions[q_region].rset.add_reference(stale);
            regions[stale].data.as_ptr() as usize
        };
        assert!(
            !gc.cards().any_dirty_in(stale_base, gc.config.region_size),
            "precondition: nothing dirtied the stale source's cards"
        );

        let before = gc.card_regions_skipped.load(Ordering::Relaxed);
        let mut roots: Vec<ObjectRef> = vec![q];
        let _ = gc.young_collection_serial(&mut roots, &NoopMonitors);
        assert!(
            gc.card_regions_skipped.load(Ordering::Relaxed) > before,
            "a source region with no dirty card must be skipped, not walked"
        );
    }

    /// Parallel-evacuator twin of
    /// [`a_source_region_with_no_dirty_card_is_skipped_entirely`]. The seed
    /// walk in `SharedEvac::seed_source_region` is a separate copy of the same
    /// scan, and defect G1-9 was exactly a divergence between the two.
    #[test]
    pub fn a_source_region_with_no_dirty_card_is_skipped_entirely_parallel() {
        // The kill switch turns off the very thing this test measures. Say so
        // and skip, rather than going red on a deliberate A/B arm — the test's
        // ability to fail is established against the DEFAULT (on) arm, which is
        // the arm that ships.
        let gc = G1Collector::new(parallel_config(2, 8));
        let q = gc.alloc_object(ClassId::new(2), 1);
        let q_region = gc
            .lookup_region_for_addr(q.as_ptr() as usize)
            .expect("q region");
        let stale = (q_region + 3) % gc.num_regions();
        let stale_base = {
            let mut regions = gc.regions.write();
            regions[stale].set_region_type(RegionType::Old);
            for _ in 0..64 {
                let (p, _) = regions[stale]
                    .bump_alloc(HEADER_SIZE, 8, "test")
                    .expect("room");
                unsafe {
                    *(p as *mut ObjectHeader) = ObjectHeader::new(
                        ClassId::new(5),
                        ObjectKind::Object,
                        ArrayElementType::Reference,
                        0,
                        0,
                    )
                };
            }
            regions[q_region].rset.add_reference(stale);
            regions[stale].data.as_ptr() as usize
        };
        assert!(!gc.cards().any_dirty_in(stale_base, gc.config.region_size));

        let before = gc.card_regions_skipped.load(Ordering::Relaxed);
        let mut roots: Vec<ObjectRef> = vec![q];
        let _ = gc.young_collection_parallel(&mut roots, &NoopMonitors);
        assert!(
            gc.card_regions_skipped.load(Ordering::Relaxed) > before,
            "the parallel seed walk must skip a source with no dirty card too"
        );
    }

    /// A JIT-PINNED source region: screened when
    /// `CRATONVM_G1_CARD_SCREEN_JIT_PINNED` is on, walked wholesale when it is
    /// off — and correct either way.
    ///
    /// This is the carve-out §12.4 measured: a JIT-pinned source used to bypass
    /// the screen unconditionally, and with a warm JIT that is most sources,
    /// which is why the screen skipped 0.79% of bytes there against 20-50%
    /// without the JIT. Both halves are asserted, for the reason the
    /// per-object screen's own test gives: without the skip assertion this
    /// passes on a screen that never engages, and without the evacuation
    /// assertion it passes on one that skips everything.
    #[test]
    pub fn a_jit_pinned_source_is_screened_or_walked_wholesale_by_the_flag() {
        // `(flag, expect_skips)` — the flag decides whether the pinned source
        // is screened; correctness is asserted in both arms.
        for (on, expect_skips) in [("1", true), ("0", false)] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_G1_CARD_SCREEN_JIT_PINNED", Some(on))],
                || {
                    let gc = make_collector();
                    // Q lives in Eden and will be collected.
                    let q = gc.alloc_object(ClassId::new(2), 1);
                    let q_region = gc
                        .lookup_region_for_addr(q.as_ptr() as usize)
                        .expect("region");
                    gc.set_field(q, 0, Value::Int(31337));

                    // The source is a hand-built Old region: many fillers that
                    // reference nothing (clean cards) and ONE holder that does.
                    // Built by hand rather than by an `alloc_object` loop
                    // because escaping a region means FILLING it, and this
                    // fixture needs room past the cursor for the skip span.
                    let (holder, holder_region) = {
                        let mut regions = gc.regions.write();
                        let src = regions
                            .iter()
                            .position(|r| r.region_type() == RegionType::Free)
                            .expect("a free region");
                        regions[src].set_region_type(RegionType::Old);
                        let mut place = |slots: u32| -> ObjectRef {
                            let size = HEADER_SIZE + slots as usize * SLOT_SIZE;
                            let (p, _) = regions[src].bump_alloc(size, 8, "test").expect("room");
                            let h = ObjectHeader::new(
                                ClassId::new(1),
                                ObjectKind::Object,
                                ArrayElementType::Reference,
                                0,
                                slots,
                            );
                            unsafe {
                                std::ptr::write(p as *mut ObjectHeader, h);
                                ObjectRef::from_raw(p)
                            }
                        };
                        for _ in 0..256 {
                            let _ = place(0);
                        }
                        let holder = place(1);
                        (holder, src)
                    };
                    gc.with_regions_mut(|_| {});
                    assert_ne!(holder_region, q_region);
                    // The barrier dirties the holder's card and records the
                    // remembered-set edge; the fillers' cards stay clean.
                    gc.set_field(holder, 0, Value::Object(Some(q)));

                    // Make the holder's region JIT-PINNED. A published TLAB
                    // skip span is the entry point `jit_pinned_region_set`
                    // reads that a test can drive without a live compiled
                    // frame — and it must sit PAST the cursor, where a real
                    // un-retired tail sits. A span planted among live objects
                    // is not a weaker fixture but a corrupt one: every region
                    // walker treats it as "skip these bytes", so the walk
                    // resyncs mid-object and reads a header out of payload
                    // (an access violation, which is how the first version of
                    // this test announced itself).
                    let (base, tail, cursor) = {
                        let regions = gc.regions.read();
                        let b = regions[holder_region].data.as_ptr() as usize;
                        (
                            b,
                            b + regions[holder_region].data.len(),
                            regions[holder_region].cursor(),
                        )
                    };
                    assert!(
                        tail - 16 > base + cursor,
                        "the skip span must sit past the cursor"
                    );
                    gc.set_jit_tlab_skip_regions(&[(tail - 16, tail)]);

                    let skipped_before = gc.card_bytes_skipped.load(Ordering::Relaxed);
                    let mut roots: Vec<ObjectRef> = vec![];
                    let result = gc.young_collection_serial(&mut roots, &NoopMonitors);
                    let skipped = gc.card_bytes_skipped.load(Ordering::Relaxed) - skipped_before;
                    gc.clear_jit_tlab_skip_regions();

                    // Correctness, both arms: Q was reachable only through the
                    // holder in the pinned region.
                    let q_new = result
                        .pointer_map
                        .get(&(q.as_ptr() as usize))
                        .copied()
                        .expect("Q, referenced only from the pinned source, must be evacuated");
                    let q_new_ref = unsafe { ObjectRef::from_raw(q_new as *mut u8) };
                    assert_eq!(
                        gc.get_field(holder, 0),
                        Value::Object(Some(q_new_ref)),
                        "flag={on}: the holder's slot must be rewritten"
                    );
                    assert_eq!(gc.get_field(q_new_ref, 0).as_int(), Some(31337));

                    // Engagement.
                    eprintln!("[screen-jit-pinned] flag={on} skipped={skipped}");
                    assert_eq!(
                        skipped > 0,
                        expect_skips,
                        "flag={on}: expected the pinned source to be {} (skipped={skipped})",
                        if expect_skips {
                            "screened"
                        } else {
                            "walked wholesale"
                        }
                    );
                },
            );
        }
    }

    /// The per-object screen, and the correctness half beside it: the ONE
    /// object in a source region that holds a CSet-bound reference is still
    /// found, while the objects around it are stepped over.
    ///
    /// Both halves matter and each catches a different mistake. Without the
    /// `skipped` assertion the test passes on a screen that never engages;
    /// without the evacuation assertion it passes on a screen that skips
    /// everything.
    ///
    /// Serial arm; the parallel twin follows. See the note on
    /// `a_source_region_with_no_dirty_card_is_skipped_entirely` for why the
    /// dispatching entry point is not used.
    #[test]
    pub fn the_per_object_screen_skips_the_clean_objects_and_finds_the_dirty_one() {
        // The kill switch turns off the very thing this test measures. Say so
        // and skip, rather than going red on a deliberate A/B arm — the test's
        // ability to fail is established against the DEFAULT (on) arm, which is
        // the arm that ships.
        let gc = make_collector();
        // Fill the first Eden region with objects that reference nothing, then
        // put the holder at the end of it. The fillers occupy whole cards that
        // no barrier ever dirties.
        let first = gc.alloc_object(ClassId::new(1), 1);
        let holder_region = gc
            .lookup_region_for_addr(first.as_ptr() as usize)
            .expect("region");
        let mut holder = first;
        for _ in 0..512 {
            let o = gc.alloc_object(ClassId::new(1), 1);
            if gc.lookup_region_for_addr(o.as_ptr() as usize) != Some(holder_region) {
                break;
            }
            holder = o;
        }
        // Q in a different region, reached ONLY through `holder`.
        let mut q = gc.alloc_object(ClassId::new(2), 1);
        while gc.lookup_region_for_addr(q.as_ptr() as usize) == Some(holder_region) {
            q = gc.alloc_object(ClassId::new(2), 1);
        }
        gc.set_field(q, 0, Value::Int(31337));
        gc.set_field(holder, 0, Value::Object(Some(q)));

        // Retype the holder's region to Old so it is a remembered-set SOURCE
        // rather than a collection-set member.
        gc.with_regions_mut(|rs| rs[holder_region].set_region_type(RegionType::Old));

        let scanned_before = gc.card_bytes_scanned.load(Ordering::Relaxed);
        let skipped_before = gc.card_bytes_skipped.load(Ordering::Relaxed);
        let mut roots: Vec<ObjectRef> = vec![];
        let result = gc.young_collection_serial(&mut roots, &NoopMonitors);
        let scanned = gc.card_bytes_scanned.load(Ordering::Relaxed) - scanned_before;
        let skipped = gc.card_bytes_skipped.load(Ordering::Relaxed) - skipped_before;

        // Correctness: Q was reachable only from the holder, so the screen must
        // not have stepped over the holder.
        let q_new = result
            .pointer_map
            .get(&(q.as_ptr() as usize))
            .copied()
            .expect("Q, referenced only from the screened source region, must be evacuated");
        let q_new_ref = unsafe { ObjectRef::from_raw(q_new as *mut u8) };
        assert_eq!(
            gc.get_field(holder, 0),
            Value::Object(Some(q_new_ref)),
            "the holder's slot must be rewritten to Q's new location"
        );
        assert_eq!(gc.get_field(q_new_ref, 0).as_int(), Some(31337));

        // Engagement: the fillers were stepped over without their slots being
        // visited. Printed as well as asserted so a future change that makes
        // this vacuous is visible in the log.
        eprintln!("[F-05] per-object screen: scanned={scanned} skipped={skipped}");
        assert!(
            skipped > 0,
            "the card screen must have stepped over the clean fillers \
             (scanned={scanned} skipped={skipped}); a screen that engages on nothing \
             is indistinguishable from no screen at all"
        );
        assert!(
            skipped > scanned,
            "one holder among hundreds of fillers should leave the skipped bytes \
             dominant (scanned={scanned} skipped={skipped})"
        );
    }

    /// Parallel-evacuator twin of
    /// [`the_per_object_screen_skips_the_clean_objects_and_finds_the_dirty_one`].
    #[test]
    pub fn the_per_object_screen_skips_the_clean_objects_and_finds_the_dirty_one_parallel() {
        // The kill switch turns off the very thing this test measures. Say so
        // and skip, rather than going red on a deliberate A/B arm — the test's
        // ability to fail is established against the DEFAULT (on) arm, which is
        // the arm that ships.
        let gc = G1Collector::new(parallel_config(2, 8));
        let first = gc.alloc_object(ClassId::new(1), 1);
        let holder_region = gc
            .lookup_region_for_addr(first.as_ptr() as usize)
            .expect("region");
        let mut holder = first;
        for _ in 0..512 {
            let o = gc.alloc_object(ClassId::new(1), 1);
            if gc.lookup_region_for_addr(o.as_ptr() as usize) != Some(holder_region) {
                break;
            }
            holder = o;
        }
        let mut q = gc.alloc_object(ClassId::new(2), 1);
        while gc.lookup_region_for_addr(q.as_ptr() as usize) == Some(holder_region) {
            q = gc.alloc_object(ClassId::new(2), 1);
        }
        gc.set_field(q, 0, Value::Int(31337));
        gc.set_field(holder, 0, Value::Object(Some(q)));
        gc.with_regions_mut(|rs| rs[holder_region].set_region_type(RegionType::Old));

        let scanned_before = gc.card_bytes_scanned.load(Ordering::Relaxed);
        let skipped_before = gc.card_bytes_skipped.load(Ordering::Relaxed);
        let mut roots: Vec<ObjectRef> = vec![];
        let result = gc.young_collection_parallel(&mut roots, &NoopMonitors);
        let scanned = gc.card_bytes_scanned.load(Ordering::Relaxed) - scanned_before;
        let skipped = gc.card_bytes_skipped.load(Ordering::Relaxed) - skipped_before;

        let q_new = result
            .pointer_map
            .get(&(q.as_ptr() as usize))
            .copied()
            .expect("Q must be evacuated on the parallel arm too");
        let q_new_ref = unsafe { ObjectRef::from_raw(q_new as *mut u8) };
        assert_eq!(gc.get_field(holder, 0), Value::Object(Some(q_new_ref)));
        assert_eq!(gc.get_field(q_new_ref, 0).as_int(), Some(31337));
        eprintln!("[F-05] per-object screen (parallel): scanned={scanned} skipped={skipped}");
        assert!(
            skipped > scanned,
            "the parallel seed walk must engage the screen too \
             (scanned={scanned} skipped={skipped})"
        );
    }

    /// Remembered sets: the mutator post-barrier records EVERY cross-region
    /// edge (including young->young, which a JNI-pinned holder depends on),
    /// and never records a same-region one.
    #[test]
    pub fn the_post_write_barrier_records_every_cross_region_edge() {
        let gc = make_collector();
        let holder = gc.alloc_object(ClassId::new(1), 1);
        let src = gc
            .lookup_region_for_addr(holder.as_ptr() as usize)
            .expect("holder region");

        // Same-region target: nothing to remember.
        let near = gc.alloc_object(ClassId::new(2), 0);
        gc.set_field(holder, 0, Value::Object(Some(near)));
        let near_region = gc
            .lookup_region_for_addr(near.as_ptr() as usize)
            .expect("target region");
        if near_region == src {
            assert!(
                gc.regions.read()[near_region].rset.sources().is_empty(),
                "a same-region edge must not enter a remembered set"
            );
        }

        // Cross-region target: the edge must be remembered against the TARGET.
        let far_region = (src + 1) % gc.num_regions();
        let far_addr = {
            let mut regions = gc.regions.write();
            regions[far_region].set_region_type(RegionType::Old);
            let (ptr, _) = regions[far_region]
                .bump_alloc(HEADER_SIZE, 8, "test")
                .expect("room in a fresh region");
            ptr as usize
        };
        gc.post_write_barrier_rset(holder, unsafe { ObjectRef::from_raw(far_addr as *mut u8) });
        assert!(
            gc.regions.read()[far_region].rset.sources().contains(&src),
            "the holder's region must be recorded as a source of the target's rset"
        );
    }

    /// Region recycling must not leave a target remembering a source that no
    /// longer holds anything: `reset()` clears the recycled region's OWN rset,
    /// and `cleanup` prunes entries naming a now-Free source. Without the
    /// prune a recycled index is re-walked wholesale once it is re-typed,
    /// resurrecting dead objects' referents.
    #[test]
    pub fn recycling_a_region_drops_the_stale_remembered_set_edges() {
        let gc = make_collector();
        let target = 1usize;
        let source = 2usize;
        gc.with_regions_mut(|rs| {
            // Survivor, not Old: cleanup's in-place free applies only to Old
            // regions, and the point of this test is the PRUNE, not the free.
            rs[target].set_region_type(RegionType::Survivor);
            rs[source].set_region_type(RegionType::Old);
            rs[target].rset.add_reference(source);
        });
        assert!(gc.regions.read()[target].rset.sources().contains(&source));

        // The SOURCE is recycled. Its own rset is cleared by `reset`...
        gc.with_regions_mut(|rs| rs[source].reset(0));
        assert!(gc.regions.read()[source].rset.sources().is_empty());

        // ...and cleanup prunes the now-dangling entry naming it.
        gc.start_concurrent_mark(&stw());
        gc.cleanup(&stw());
        assert!(
            !gc.regions.read()[target].rset.sources().contains(&source),
            "an entry naming a Free source must be pruned, not carried forever"
        );
    }

    /// G1AUD-5 (defect G1-8) — the "undead" entry.
    ///
    /// The Free-source prune above only catches a source that is STILL Free when
    /// cleanup runs. The expensive case is the other one: a source that was
    /// recycled and then immediately re-typed (a young pause frees a region, the
    /// next allocation claims it as Eden). Such an entry is not Free, so it
    /// survived every prune and every later pause re-walked that region
    /// *wholesale* on behalf of an object zero-filled cycles ago — resurrecting
    /// its referents. Entries now carry the generation they were recorded in and
    /// regions the generation they were recycled in, which makes the stale ones
    /// nameable on both the scan side and in cleanup.
    #[test]
    pub fn a_recycled_and_retyped_remembered_set_source_is_dropped_not_rewalked() {
        let gc = make_collector();
        let target = 1usize;
        let source = 2usize;

        let recorded_in = gc.rset_generation();
        gc.with_regions_mut(|rs| {
            rs[target].set_region_type(RegionType::Survivor);
            rs[source].set_region_type(RegionType::Old);
            rs[target]
                .rset
                .add_reference_in_generation(source, recorded_in);
        });

        // A pause recycles the source and the allocator immediately re-types it.
        // Every recycle phase bumps the generation before it touches a region,
        // so the reset stamp is strictly newer than the edge's.
        gc.rset_cache_epoch.fetch_add(1, Ordering::Release);
        let recycled_in = gc.rset_generation();
        assert!(recycled_in > recorded_in);
        gc.with_regions_mut(|rs| {
            rs[source].reset(recycled_in);
            // Re-typed, NOT left Free — that is exactly the case the old
            // Free-only prune could not see. `Eden` also keeps cleanup's
            // in-place free (Old-only) out of the picture, so the assertion
            // below can only be satisfied by the generation prune.
            rs[source].set_region_type(RegionType::Eden);
        });

        // The entry still exists, and the OLD Free-only test would keep it...
        assert!(
            gc.regions.read()[target].rset.sources().contains(&source),
            "precondition: the entry is still recorded and the source is not Free"
        );
        // ...but the scan side no longer offers it as a source to walk.
        {
            let regions = gc.regions.read();
            assert!(
                G1Collector::rset_entry_is_stale(&regions, source, recorded_in),
                "an edge recorded before the source was recycled is dead"
            );
            assert!(
                !G1Collector::live_rset_sources(&regions, &[target]).contains(&source),
                "a recycled-then-retyped source must NOT be walked wholesale again"
            );
        }

        // And cleanup drops the entry outright, so it stops costing memory and
        // a per-pause lookup.
        gc.start_concurrent_mark(&stw());
        gc.cleanup(&stw());
        assert!(
            !gc.regions.read()[target].rset.sources().contains(&source),
            "cleanup must prune an entry whose source was recycled, not only \
             one whose source happens to still be Free"
        );
    }

    /// The other half of G1AUD-5, and the one that would be a use-after-free if
    /// it broke: an edge recorded AFTER the recycle names a live holder and must
    /// survive both the scan-side filter and cleanup's prune. An entry with no
    /// generation available at record time (`RSET_GENERATION_PINNED`) must also
    /// survive — that is the fail-safe direction.
    #[test]
    pub fn a_remembered_set_edge_recorded_after_the_recycle_is_never_pruned() {
        let gc = make_collector();
        let target = 1usize;
        let fresh = 2usize;
        let unstamped = 3usize;

        gc.rset_cache_epoch.fetch_add(1, Ordering::Release);
        let recycled_in = gc.rset_generation();
        gc.with_regions_mut(|rs| {
            rs[target].set_region_type(RegionType::Survivor);
            // `Eden`, not `Old`: cleanup frees a zero-live OLD region in place,
            // which would turn these into Free sources and prune them for the
            // wrong reason.
            rs[fresh].reset(recycled_in);
            rs[fresh].set_region_type(RegionType::Eden);
            rs[unstamped].reset(recycled_in);
            rs[unstamped].set_region_type(RegionType::Eden);
        });

        // Same generation as the reset: the Phase-4 rebuild records edges
        // BEFORE Phase 5's frees, so `stale` is a strict `<` and this is kept.
        gc.with_regions_mut(|rs| {
            rs[target]
                .rset
                .add_reference_in_generation(fresh, recycled_in);
            // No generation to hand — the deprecated/prototype entry point.
            rs[target].rset.add_reference(unstamped);
        });
        assert_eq!(
            gc.regions.read()[target]
                .rset
                .recorded_generation(unstamped),
            Some(cratonvm_gc_core::region::RSET_GENERATION_PINNED),
            "the generation-less entry point must record the never-prune stamp"
        );

        {
            let regions = gc.regions.read();
            let live = G1Collector::live_rset_sources(&regions, &[target]);
            assert!(live.contains(&fresh), "a same-generation edge is live");
            assert!(
                live.contains(&unstamped),
                "an unstamped edge must be walked, never dropped — dropping a \
                 live cross-region edge is a use-after-free"
            );
        }

        gc.start_concurrent_mark(&stw());
        gc.cleanup(&stw());
        let sources = gc.regions.read()[target].rset.sources();
        assert!(sources.contains(&fresh));
        assert!(sources.contains(&unstamped));
    }

    /// G1AUD-1, parallel half — the promotion stamp must be on BOTH evacuators.
    ///
    /// The JIT's inline reference-store fast paths read `GC_FLAG_OLD_GEN` to
    /// decide "young receiver, no post barrier needed". If the parallel
    /// evacuator ever stops stamping it, every promoted object reads as young
    /// there, the remembered-set post barrier is skipped for JIT-compiled stores
    /// into it, and the next young pause frees a still-live referent. The serial
    /// twin is `promotion_stamps_the_old_generation_bit_the_jit_barrier_reads`.
    #[test]
    pub fn promotion_stamps_the_old_generation_bit_on_the_parallel_evacuator_too() {
        let mut cfg = parallel_config(2, 8);
        cfg.promotion_age = 1;
        let gc = G1Collector::new(cfg);

        let obj = gc.alloc_object(ClassId::new(1), 1);
        assert_eq!(
            gc.get_header(obj).gc_flags() & GC_FLAG_OLD_GEN,
            0,
            "a freshly allocated Eden object is young"
        );

        let mut roots = vec![obj];
        gc.young_collection_parallel(&mut roots, &NoopMonitors);
        assert_eq!(
            gc.get_header(roots[0]).gc_flags() & GC_FLAG_OLD_GEN,
            0,
            "a Survivor copy is still young and MUST NOT be stamped"
        );

        gc.young_collection_parallel(&mut roots, &NoopMonitors);
        let promoted = gc
            .lookup_region_for_addr(roots[0].as_ptr() as usize)
            .expect("promoted object is in a region");
        assert_eq!(
            gc.regions.read()[promoted].region_type(),
            RegionType::Old,
            "the object should have been promoted by the second parallel pass"
        );
        assert_ne!(
            gc.get_header(roots[0]).gc_flags() & GC_FLAG_OLD_GEN,
            0,
            "a promoted object MUST carry GC_FLAG_OLD_GEN on the parallel path \
             too — the JIT reads the header, not the region table"
        );
    }

    /// G1AUD-6 (defect G1-9) — the parallel evacuator must walk JIT-pinned
    /// regions wholesale as remembered-set sources, exactly as the serial path
    /// has since the original CSet-straddle UAF fix.
    ///
    /// A JIT-pinned region is excluded from the CSet, so it is never traced as a
    /// from-space object; it is reached ONLY as a source. `jit_pinned_region_set`
    /// is not gated on `gc_quiescence::is_active()` — it also contains every
    /// region holding a published un-retired TLAB tail — so this set is
    /// routinely non-empty with no thread in JIT at all, which is the
    /// configuration G1-9's live-object corruption reproduces under.
    ///
    /// The test removes the remembered-set entry the barrier recorded, leaving
    /// the wholesale walk as the only coverage. That is the store class the
    /// serial path's defence exists for (a JIT-compiled store the collector
    /// cannot assume went through `post_write_barrier_rset`). Before the fix the
    /// parallel path lost Q here: not evacuated, not rewritten, and its region
    /// zero-filled by Phase 5.
    #[test]
    pub fn a_jit_pinned_region_is_a_wholesale_rset_source_on_the_parallel_path_too() {
        let gc = G1Collector::new(parallel_config(2, 8));
        let p = gc.alloc_object(ClassId::new(1), 1);
        let p_region = gc.lookup_region_for_addr(p.as_ptr() as usize).unwrap();
        // Retire the current Eden so Q lands in a DIFFERENT region without
        // filling P's region to the brim — this test publishes a reserved tail
        // at the top of P's region and needs it to sit above the cursor, which
        // the "allocate until the region rolls over" idiom used elsewhere would
        // make impossible.
        gc.clear_eden_slots();
        let q = gc.alloc_object(ClassId::new(2), 1);
        let q_region = gc.lookup_region_for_addr(q.as_ptr() as usize).unwrap();
        assert_ne!(p_region, q_region, "Q must be cross-region from P");
        gc.set_field(q, 0, Value::Int(1717));
        gc.set_field(p, 0, Value::Object(Some(q)));

        // Erase the barrier's record so the ONLY remaining coverage for Q is the
        // wholesale walk of the pinned source.
        gc.regions.read()[q_region].rset.clear();
        assert!(gc.regions.read()[q_region].rset.sources().is_empty());

        // Publish an un-retired TLAB tail in P's region. This is what puts the
        // region into `jit_pinned_region_set()` with no thread in JIT. The span
        // sits above the region's allocation cursor so no walk ever reaches it.
        let (skip_start, skip_end) = {
            let regions = gc.regions.read();
            let r = &regions[p_region];
            let base = r.data.addr();
            let len = r.data.len();
            assert!(r.cursor() + 64 < len, "the tail must sit above the cursor");
            (base + len - 64, base + len)
        };
        gc.set_jit_tlab_skip_regions(&[(skip_start, skip_end)]);
        assert!(
            gc.jit_pinned_region_set().contains(&p_region),
            "a published reserved tail must pin its region, JIT active or not"
        );

        let mut roots: Vec<ObjectRef> = vec![];
        let result = gc.young_collection_parallel(&mut roots, &NoopMonitors);
        gc.clear_jit_tlab_skip_regions();

        let q_new = result
            .pointer_map
            .get(&(q.as_ptr() as usize))
            .copied()
            .expect(
                "Q is reachable only from a JIT-pinned region with no rset entry; \
                 the parallel path must walk that region wholesale as a source, \
                 like the serial path does",
            );
        let q_new_ref = unsafe { ObjectRef::from_raw(q_new as *mut u8) };
        assert_eq!(
            gc.get_field(p, 0),
            Value::Object(Some(q_new_ref)),
            "the pinned holder's slot must be rewritten to Q's new location"
        );
        assert_eq!(gc.get_field(q_new_ref, 0).as_int(), Some(1717));
        assert_eq!(
            gc.regions.read()[p_region].region_type(),
            RegionType::Eden,
            "the pinned region itself must stay out of the CSet"
        );
    }


    // -----------------------------------------------------------------------
    // The flat 16-byte-slot walk refuses an ARRAY header (option D of
    // `what-should-a-walker-do-with-an-unvalidated-header-count-FIXED-20260826.md`).
    // -----------------------------------------------------------------------

    /// Build a legacy-layout body of `slots` `Value::Object` cells behind a
    /// header, and return the backing store plus the object pointer. The store
    /// must outlive the pointer; the caller keeps it alive.
    pub fn legacy_body_with_object_cells(
        header: ObjectHeader,
        slots: usize,
        target: *mut u8,
    ) -> (Vec<u64>, *mut u8) {
        // `u64` backing => 8-byte alignment, which is what both `ObjectHeader`
        // and `Value` require.
        let mut store = vec![0u64; (HEADER_SIZE + slots * SLOT_SIZE) / 8];
        let base = store.as_mut_ptr() as *mut u8;
        // SAFETY: `store` is sized for the header plus `slots` cells, and every
        // write below stays inside it.
        unsafe {
            std::ptr::write(base as *mut ObjectHeader, header);
            for i in 0..slots {
                let slot = base.add(HEADER_SIZE + i * SLOT_SIZE);
                value_to_unaligned_ptr(Value::Object(Some(ObjectRef::from_raw(target))), slot);
            }
        }
        (store, base)
    }

    #[test]
    pub fn flat_walk_refuses_an_array_header_and_counts_it() {
        // A header whose kind is `Array` and whose `shape` dword therefore
        // means LENGTH, not `num_slots` — the two share offset 4. The body
        // below is deliberately large enough to hold every cell the unrefused
        // walk would visit, so a regression fails this assertion instead of
        // reading off the end of the allocation.
        const LEN: u32 = 8;
        let mut target = [0u8; HEADER_SIZE];
        let header = ObjectHeader::new(
            ClassId::new(7),
            ObjectKind::Array,
            ArrayElementType::Reference,
            LEN,
            0,
        );
        assert_eq!(
            header.num_slots(),
            LEN,
            "the premise of the whole guard: an Array's length IS what \
             `num_slots()` returns, because they are the same `shape` dword"
        );
        let (_store, obj) =
            legacy_body_with_object_cells(header, LEN as usize, target.as_mut_ptr());
        let header_ref = unsafe { &*(obj as *const ObjectHeader) };

        let before = flat_walks_refused_for_array();
        let mut visited = 0usize;
        for_each_flat_object_reference_trusting_header(obj, header_ref, 0, |_, _, _| {
            visited += 1;
        });

        assert_eq!(
            visited, 0,
            "the flat walk must not stride SLOT_SIZE over array payload: every \
             one of its fifteen call sites pre-branches on kind and handles \
             arrays itself, so an Array arriving here is a kind confusion"
        );
        assert!(
            flat_walks_refused_for_array() > before,
            "the refusal must be counted — the number is what makes \
             `FLAT_WALK_REFUSED_ARRAY` evidence rather than an anecdote"
        );
    }

    #[test]
    pub fn flat_walk_still_visits_a_legacy_objects_reference_cells() {
        // The refusal must be narrow: a non-array legacy object still walks
        // every cell. Without this, "refuse arrays" could silently become
        // "refuse everything" and no other test in this file would notice,
        // because the collector tests allocate through the real heap.
        const SLOTS: u32 = 5;
        let mut target = [0u8; HEADER_SIZE];
        let header = ObjectHeader::new(
            ClassId::new(0),
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            SLOTS,
        );
        let (_store, obj) =
            legacy_body_with_object_cells(header, SLOTS as usize, target.as_mut_ptr());
        let header_ref = unsafe { &*(obj as *const ObjectHeader) };

        let mut visited = 0usize;
        for_each_flat_object_reference_trusting_header(obj, header_ref, 0, |_, raw, compact| {
            assert!(!compact, "no CompactLayout is registered for ClassId(0)");
            assert_eq!(raw, target.as_mut_ptr() as usize);
            visited += 1;
        });
        assert_eq!(visited, SLOTS as usize);

        // `first_index` still skips a prefix.
        let mut skipped = 0usize;
        for_each_flat_object_reference_trusting_header(obj, header_ref, 2, |_, _, _| skipped += 1);
        assert_eq!(skipped, SLOTS as usize - 2);
    }

    #[test]
    pub fn capped_walk_bounds_a_legacy_object_below_its_claimed_slot_count() {
        // The other half of the page: a caller that DOES hold geometry passes a
        // bound, and the walk stops there even though the header claims more.
        const SLOTS: u32 = 6;
        let mut target = [0u8; HEADER_SIZE];
        let header = ObjectHeader::new(
            ClassId::new(0),
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            SLOTS,
        );
        let (_store, obj) =
            legacy_body_with_object_cells(header, SLOTS as usize, target.as_mut_ptr());
        let header_ref = unsafe { &*(obj as *const ObjectHeader) };

        let mut visited = 0usize;
        for_each_flat_object_reference_capped(obj, header_ref, 0, 2, |_, _, _| visited += 1);
        assert_eq!(
            visited, 2,
            "a region-derived cap must win over `num_slots()`; that is the \
             whole point of the `_capped` entry point"
        );
    }

    // -----------------------------------------------------------------------
    // lane B (2026-09-20) — the Phase-4 walk set, and the tripwire that is
    // supposed to catch a wrong one.
    // -----------------------------------------------------------------------

    /// A JNI-pinned region is held out of every collection set and appears in
    /// NEITHER set the callers build (`live_rset_sources` + the JIT-pinned
    /// set), so narrowing removed the only walk that ever covered it.
    ///
    /// This is the shape that made the narrowing a behaviour change rather than
    /// a pure optimisation: the wide walk rewrote a pinned region's slots for
    /// free, and the narrow one stopped. The term is added inside
    /// `phase4_regions_to_walk` rather than at the four call sites so a fifth
    /// driver cannot be written without it.
    #[test]
    pub fn lane_b_narrow_fixup_walks_a_jni_pinned_region() {
        let gc = G1Collector::new(many_region_config());
        {
            let mut regions = gc.regions.write();
            // Two Old regions this pause does not touch. They differ in exactly
            // one bit: one of them is JNI-pinned.
            for i in [7usize, 9usize] {
                regions[i].set_region_type(RegionType::Old);
                regions[i].set_cursor(128);
            }
            regions[9].pinned = true;
        }
        // Snapshot AFTER the setup, so neither region reads as "changed" — the
        // pin must be the only reason 9 is in the set.
        let pre: Vec<(RegionType, usize)> = {
            let regions = gc.regions.read();
            regions
                .iter()
                .map(|r| (r.region_type(), r.cursor()))
                .collect()
        };
        let sources = RegionSet::new();
        let regions = gc.regions.read();
        let narrow = gc
            .phase4_regions_to_walk(&regions, Some(&pre), &sources)
            .expect("narrowing is on");

        assert!(
            !narrow.contains(&7),
            "an untouched, unpinned Old region must stay OUT, or the control \
             arm of this test proves nothing"
        );
        assert!(
            narrow.contains(&9),
            "a JNI-pinned region is excluded from the CSet and named by neither \
             source set, so the fix-up must walk it explicitly — otherwise its \
             slots into the CSet are the ones nothing rewrites"
        );
    }

    /// The walk reaches a humongous object's slots only from its
    /// `HumongousStart` index — a continuation index breaks out on the filler
    /// header and rewrites nothing — so the walk set has to be closed under
    /// span containment.
    #[test]
    pub fn lane_b_narrow_fixup_maps_a_humongous_continuation_to_its_span_start() {
        let gc = G1Collector::new(many_region_config());
        {
            let mut regions = gc.regions.write();
            regions[4].set_region_type(RegionType::HumongousStart);
            regions[5].set_region_type(RegionType::HumongousContinuation);
            regions[6].set_region_type(RegionType::HumongousContinuation);
        }
        let pre: Vec<(RegionType, usize)> = {
            let regions = gc.regions.read();
            regions
                .iter()
                .map(|r| (r.region_type(), r.cursor()))
                .collect()
        };
        // A source set that names only a CONTINUATION of the span.
        let sources: RegionSet = [6usize].into_iter().collect();
        let regions = gc.regions.read();
        let narrow = gc
            .phase4_regions_to_walk(&regions, Some(&pre), &sources)
            .expect("narrowing is on");

        assert!(
            narrow.contains(&4),
            "a continuation index in the walk set must pull in its span START, \
             which is the only index from which the object's slots are reachable"
        );
    }

    /// Build a fixture in which one surviving object holds a reference into a
    /// second region, and hand back both region indices.
    ///
    /// The filler between the two allocations is what forces them into
    /// different regions under `many_region_config`'s 8 KiB regions; without it
    /// every edge in a unit-test heap is intra-region, which is exactly why the
    /// existing suite could not discriminate a broken Phase-4 narrowing.
    pub fn lane_b_cross_region_holder(gc: &G1Collector) -> (ObjectRef, ObjectRef, usize, usize) {
        let target = gc.alloc_object(ClassId::new(2), 1);
        gc.set_field(target, 0, Value::Int(7));
        // Reference-free filler: an Int-only object can never itself hold a
        // slot the verifier would classify, so the only edge in this heap is
        // the one the test installs.
        while gc.lookup_region_for_addr(target.as_ptr() as usize)
            == gc.lookup_region_for_addr(gc.alloc_object(ClassId::new(3), 4).as_ptr() as usize)
        {
        }
        let holder = gc.alloc_object(ClassId::new(1), 1);
        gc.set_field(holder, 0, Value::Object(Some(target)));
        let t_idx = gc
            .lookup_region_for_addr(target.as_ptr() as usize)
            .expect("target is in a region");
        let h_idx = gc
            .lookup_region_for_addr(holder.as_ptr() as usize)
            .expect("holder is in a region");
        assert_ne!(t_idx, h_idx, "the fixture needs a CROSS-region edge");
        (holder, target, h_idx, t_idx)
    }

    /// THE CHECK THIS LANE EXISTS TO ADD.
    ///
    /// `verify_no_dangling_into_cset` is the stated tripwire for the Phase-4
    /// narrowing, and before this it could not see a narrowing bug at all: a
    /// slot pointing at a from-space address that IS a forwarding key was read
    /// as proof the slot was fine, when it is the opposite — the referent
    /// demonstrably moved (something copied it) and this slot was not rewritten
    /// to follow it.
    ///
    /// The fixture is the post-Phase-5 state that a missed region produces: the
    /// target's region is `Free`, the map says where the object went, and the
    /// holder still names the old address.
    #[test]
    pub fn lane_b_verifier_reports_a_slot_left_pointing_at_an_object_that_moved() {
        let gc = G1Collector::new(many_region_config());
        let (holder, target, _h_idx, t_idx) = lane_b_cross_region_holder(&gc);

        let cset: RegionSet = [t_idx].into_iter().collect();
        let mut map = cratonvm_types::PointerMap::default();
        // Moved SOMEWHERE ELSE — the holder's own address is a convenient
        // 8-aligned in-arena value; what matters is only that it differs from
        // the key.
        map.insert(target.as_ptr() as usize, holder.as_ptr() as usize);

        {
            // Phase 5, simulated: the CSet region is freed.
            let mut regions = gc.regions.write();
            regions[t_idx].set_region_type(RegionType::Free);
        }

        let before = PHASE4_STALE_FORWARD_SLOTS.load(Ordering::Relaxed);
        {
            let regions = gc.regions.read();
            gc.verify_no_dangling_into_cset_within(
                &regions,
                &cset,
                &map,
                &[holder],
                usize::MAX,
                usize::MAX,
            );
        }
        assert!(
            PHASE4_STALE_FORWARD_SLOTS.load(Ordering::Relaxed) > before,
            "a slot still naming a from-space address of an object this pause \
             MOVED and then freed is the exact failure mode of a wrong Phase-4 \
             walk set, and the verifier must report it"
        );
        // …and it must be THIS collector that reported it. The counter above is
        // a process-global `static` shared with every other test in the binary;
        // the ledger is per instance.
        assert_eq!(
            gc.v7b_reported.lock().len(),
            1,
            "exactly one holder/target pair, reported once"
        );
    }

    /// Arm a trustworthy completed mark in which `h_idx` is an Old region whose
    /// whole allocated prefix predates the snapshot and nothing is marked.
    pub fn arm_completed_mark_with_dead_old_region(gc: &G1Collector, h_idx: usize) {
        gc.with_regions_mut(|regions| regions[h_idx].set_region_type(RegionType::Old));
        let tams = gc.regions.read()[h_idx].cursor();
        force_mark_snapshot(gc, h_idx, tams);
        gc.marking_complete.store(true, Ordering::Release);
        gc.last_mark_trusted.store(true, Ordering::Release);
    }

    /// A V7b-shaped slot in floating garbage is not a use-after-free. The
    /// native-allocation reproducer drops its old `byte[][]` holder before the
    /// collection, and the old linear verifier mistook those dead slots for a
    /// live remembered-set failure. A completed mark that proves the holder
    /// dead must silence the report (or the debug assertion behind one).
    #[test]
    pub fn lane_b_verifier_ignores_an_unreachable_holder() {
        let gc = G1Collector::new(many_region_config());
        let (holder, target, h_idx, t_idx) = lane_b_cross_region_holder(&gc);
        let cset: RegionSet = [t_idx].into_iter().collect();
        let map = cratonvm_types::PointerMap::default();
        arm_completed_mark_with_dead_old_region(&gc, h_idx);

        {
            let mut regions = gc.regions.write();
            regions[t_idx].set_region_type(RegionType::Free);
        }

        let before = gc.v7b_reported.lock().len();
        {
            let regions = gc.regions.read();
            gc.verify_no_dangling_into_cset_within(
                &regions,
                &cset,
                &map,
                &[],
                usize::MAX,
                usize::MAX,
            );
        }
        assert_eq!(
            gc.v7b_reported.lock().len(),
            before,
            "an unreachable holder {holder:?} with dead target {target:?} is floating garbage, not V7b"
        );
    }

