use super::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
use std::sync::Arc;

/// A component class id no fresh VM defines, so the quickened `aastore`'s
/// covariance memo answers "`Object[]`" and serves the store.
const COMPONENT: u32 = 77_777;
const MARKERS: usize = 300;
const SPINS: usize = 128;

fn vm(gc_algorithm: crate::config::GcAlgorithm) -> Arc<SharedVm> {
    Arc::new(SharedVm::new(crate::config::VmConfig {
        gc_algorithm,
        ..crate::config::VmConfig::default()
    }))
}

fn raw_of(v: Value) -> u64 {
    match v {
        Value::Object(Some(r)) => r.as_ptr() as u64,
        _ => 0,
    }
}

/// Every backend, the default one first, takes the HARDWARE
/// reference-element CAS. The w6 tests would have failed this on the
/// default (ZGC) backend; it fails again if any backend is re-declined.
#[test]
fn nio21_w8_every_backend_serves_a_reference_element_cas_in_hardware() {
    // zt t19/x15: no zb1 arming test may flip the gate mid-run.
    let _gate = crate::ZGC_READ_GATE_TEST_LOCK.lock();
    if cratonvm_types::zgc_read_barrier_armed() {
        return; // an armed ZGC keeps the lock path by design (ZGC page)
    }
    let mut backends = vec![
        crate::config::VmConfig::default().gc_algorithm,
        crate::config::GcAlgorithm::Generational,
        crate::config::GcAlgorithm::G1,
    ];
    backends.dedup();
    for gc in backends {
        let shared = vm(gc);
        let heap = &shared.mem.heap;
        let arr = heap.alloc_array(ClassId::new(COMPONENT), ArrayElementType::Reference, 2);
        let a = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 1);
        let served = crate::vm::vm_exec::hw_cas_reference_array_element(
            &shared,
            arr,
            1,
            Value::Object(None),
            Value::Object(Some(a)),
        );
        assert_eq!(
            served,
            Some((0, true)),
            "{gc:?}: the element CAS must be one LOCK CMPXCHG"
        );
        assert_eq!(
            raw_of(heap.get_array_element(arr, 1).unwrap()),
            a.as_ptr() as u64,
            "{gc:?}"
        );
    }
}

/// JIT round 22 wave 2, lane runtime (W8-L3-2 of
/// `jit-r21-nio-proposals-w8-L3.md`): the per-backend ratchet for the
/// PRIMITIVE array element arm too -- every backend a default or explicit
/// configuration can pick serves an `int[]` / `long[]` element CAS with one
/// hardware compare-exchange, so a new backend screen fails here, not on a
/// host battery.
#[test]
fn r22w2_every_backend_serves_a_primitive_element_cas_in_hardware() {
    if !crate::runtime::env_cache::unsafe_array_hw_cas_enabled() {
        return; // the kill switch's own arm
    }
    let mut backends = vec![
        crate::config::VmConfig::default().gc_algorithm,
        crate::config::GcAlgorithm::Generational,
        crate::config::GcAlgorithm::G1,
    ];
    backends.dedup();
    for gc in backends {
        let shared = vm(gc);
        let heap = &shared.mem.heap;
        let ints = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 2);
        let served = crate::vm::vm_exec::hw_cas_primitive_array_element(
            &shared,
            ints,
            1,
            Value::Int(0),
            Value::Int(5),
        );
        assert_eq!(served.map(|(_, swapped)| swapped), Some(true), "{gc:?}: int[]");
        assert_eq!(heap.get_array_element(ints, 1), Ok(Value::Int(5)), "{gc:?}");
        let longs = heap.alloc_array(ClassId::new(0), ArrayElementType::Long, 2);
        let served = crate::vm::vm_exec::hw_cas_primitive_array_element(
            &shared,
            longs,
            0,
            Value::Long(0),
            Value::Long(-7),
        );
        assert_eq!(served.map(|(_, swapped)| swapped), Some(true), "{gc:?}: long[]");
        assert_eq!(heap.get_array_element(longs, 0), Ok(Value::Long(-7)), "{gc:?}");
    }
}

/// One writer stores marker `j` (an object of group `j`) with a PLAIN
/// store; two CAS threads toggle the element between the two objects of
/// whichever group they read. Once the writer has stored group `j`, no read
/// may see a lower group: a CAS that expected an older object must fail. A
/// lost marker is a CAS that landed over a plain store. No ABA is possible:
/// group `j - 1`'s objects never return to the slot once `j` is stored.
fn race_on(gc: crate::config::GcAlgorithm) {
    let shared = vm(gc);
    let heap = &shared.mem.heap;
    let arr = heap.alloc_array(ClassId::new(COMPONENT), ArrayElementType::Reference, 1);
    let mut w = Vec::with_capacity(MARKERS + 1);
    let mut d = Vec::with_capacity(MARKERS + 1);
    let mut group: HashMap<u64, usize> = HashMap::new();
    for j in 0..=MARKERS {
        let wj = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 1);
        let dj = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 1);
        group.insert(wj.as_ptr() as u64, j);
        group.insert(dj.as_ptr() as u64, j);
        w.push(wj.as_ptr() as u64);
        d.push(dj.as_ptr() as u64);
    }
    // SAFETY: `w[0]` is the live marker just allocated.
    let w0 = unsafe { ObjectRef::from_raw(w[0] as *mut u8) };
    heap.set_array_element(arr, 0, Value::Object(Some(w0)))
        .expect("in range");
    let arr_addr = arr.as_ptr() as u64;
    let lost = AtomicU64::new(0);
    let fast_hits = AtomicUsize::new(0);
    let done = AtomicBool::new(false);
    let (shared_ref, group, w, d): (&SharedVm, &HashMap<u64, usize>, &Vec<u64>, &Vec<u64>) =
        (&*shared, &group, &w, &d);
    let (lost, fast_hits, done) = (&lost, &fast_hits, &done);
    // SAFETY (every use): the array and the markers are live for the whole
    // test, and nothing allocates or collects while the threads run.
    let obj = |raw: u64| unsafe { ObjectRef::from_raw(raw as *mut u8) };
    let read = move || {
        let raw = raw_of(
            shared_ref
                .mem
                .heap
                .get_array_element(obj(arr_addr), 0)
                .unwrap(),
        );
        *group.get(&raw).expect("the element holds a marker")
    };
    std::thread::scope(|s| {
        for _ in 0..2 {
            s.spawn(move || {
                let arr = obj(arr_addr);
                let mut top = 0usize;
                while !done.load(Ordering::Acquire) {
                    let cur = raw_of(shared_ref.mem.heap.get_array_element(arr, 0).unwrap());
                    let g = group[&cur];
                    if g < top {
                        lost.fetch_add(1, Ordering::Relaxed);
                    }
                    top = top.max(g);
                    let next = if cur == w[g] { d[g] } else { w[g] };
                    let _ = crate::vm::vm_exec::compare_and_swap_field_shared(
                        shared_ref,
                        arr,
                        0,
                        Value::Object(Some(obj(cur))),
                        Value::Object(Some(obj(next))),
                    );
                }
            });
        }
        s.spawn(move || {
            let arr = obj(arr_addr);
            let zgc: Option<&crate::memory::vm_heap::VmHeap> = match &shared_ref.mem.heap {
                _ if shared_ref.mem.heap.is_zgc() => Some(&shared_ref.mem.heap),
                _ => None,
            };
            for j in 1..=MARKERS {
                // Odd markers through the interpreter's quickened plain
                // `aastore` where the backend has one; the rest (and any
                // decline) through the heap's plain element store, the one
                // `putReferenceRelease` / `AtomicReferenceArray.lazySet`
                // reach.
                let served = j % 2 == 1
                    && zgc.is_some_and(|z| {
                        array_store_ref(
                            shared_ref,
                            z,
                            arr,
                            0,
                            CompactValue::object(w[j]),
                            0,
                            false,
                        )
                    });
                if served {
                    fast_hits.fetch_add(1, Ordering::Relaxed);
                } else {
                    shared_ref
                        .mem
                        .heap
                        .set_array_element(arr, 0, Value::Object(Some(obj(w[j]))))
                        .expect("in range");
                }
                for _ in 0..SPINS {
                    if read() < j {
                        lost.fetch_add(1, Ordering::Relaxed);
                    }
                }
                std::thread::yield_now();
            }
            done.store(true, Ordering::Release);
        });
    });
    assert_eq!(
        lost.load(Ordering::Relaxed),
        0,
        "{gc:?}: a CAS overwrote a plain store"
    );
    assert_eq!(read(), MARKERS, "{gc:?}: the last marker survives");
    if shared.mem.heap.is_zgc() {
        assert!(
            fast_hits.load(Ordering::Relaxed) > 0,
            "the quickened aastore arm must take part in the race"
        );
    }
}

#[test]
fn nio21_w8_plain_store_never_lost_to_a_reference_cas_on_the_default_backend() {
    // zt t19/x15: no zb1 arming test may flip the gate mid-run.
    let _gate = crate::ZGC_READ_GATE_TEST_LOCK.lock();
    if cratonvm_types::zgc_read_barrier_armed() {
        return;
    }
    race_on(crate::config::VmConfig::default().gc_algorithm);
}

#[test]
fn nio21_w8_plain_store_never_lost_to_a_reference_cas_on_generational() {
    race_on(crate::config::GcAlgorithm::Generational);
}

#[test]
fn nio21_w8_plain_store_never_lost_to_a_reference_cas_on_g1() {
    race_on(crate::config::GcAlgorithm::G1);
}
