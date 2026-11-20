use super::*;
use std::time::{Duration, Instant};

const M: usize = 1024 * 1024;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
}

/// A heap that has freed ~24 MB in a collection, with a 1 s delay and
/// the readable give-back on; `None` on the wholly-committed store.
fn freed_heap(delay_secs: u64) -> Option<ZgcRealHeap> {
    let heap = ZgcRealHeap::with_capacity(64 * M);
    heap.set_tlab_enabled(false);
    heap.set_readable_give_back(true);
    heap.set_uncommit_options(true, delay_secs);
    heap.set_uncommit_floor(0);
    if heap.commit_stats().0 == heap.heap_capacity() {
        return None;
    }
    let mut roots: Vec<ObjectRef> = Vec::new();
    while heap.allocated_bytes() < 24 * M {
        heap.alloc_object(ClassId::new(4), 8);
    }
    // SAFETY: single-threaded unit test.
    let stw = unsafe { StopTheWorldToken::new() };
    heap.collect_garbage(&stw, &mut roots, &NoMonitors);
    Some(heap)
}

#[test]
fn the_log_line_has_hotspots_shape() {
    assert_eq!(
        zgc_uncommitter_log_line(136 * M, 512 * M, Duration::from_micros(36_786)),
        "Uncommitter (0) Uncommitted: 136M(27%) in 36.786ms"
    );
    assert_eq!(
        zgc_uncommitter_log_line(2 * M, 512 * M, Duration::from_micros(174)),
        "Uncommitter (0) Uncommitted: 2M(0%) in 0.174ms"
    );
}

/// The point of the lane: memory a collection freed goes back a delay
/// later with NO further collection, and `totalMemory()` falls with it.
#[test]
fn an_idle_heap_gives_memory_back_without_a_collection() {
    let Some(heap) = freed_heap(1) else {
        return;
    };
    let collections = heap.gc_count();
    let before = heap.os_committed_bytes();
    // Not yet due: the sweep stamped the freed granules a moment ago.
    let early = heap.uncommit_pass_at(Instant::now());
    assert_eq!(early.reset + early.decommitted, 0, "{early:?}");
    assert!(early.next_wait <= Duration::from_secs(1), "{early:?}");
    let pass = heap.uncommit_pass_at(Instant::now() + Duration::from_secs(2));
    let released = pass.reset + pass.decommitted;
    assert!(released >= 16 * M, "released {released}: {pass:?}");
    assert_eq!(heap.gc_count(), collections, "no collection ran");
    assert!(
        heap.os_committed_bytes() + 16 * M <= before,
        "totalMemory must fall: {} -> {}",
        before,
        heap.os_committed_bytes()
    );
    let s = heap.footprint_stats();
    assert_eq!(s.uncommit_passes, 2, "{s:?}");
    assert_eq!(s.uncommit_passes_released, 1, "{s:?}");
    assert_eq!(
        s.uncommit_reset_bytes + s.uncommit_decommitted_bytes,
        released
    );
    // Nothing left to do.
    let again = heap.uncommit_pass_at(Instant::now() + Duration::from_secs(4));
    assert_eq!(again.reset + again.decommitted, 0);
}

/// zr z4/jf X1: a collection whose start gives memory back (here by
/// RESETTING, the default mode, which decommits nothing) files the bytes
/// and the time on its JFR facts, so `jdk.ZUncommit` fires.
#[test]
fn a_collection_start_give_back_files_a_jfr_uncommit() {
    let Some(heap) = freed_heap(0) else {
        return;
    };
    let _ = heap.take_jfr_cycle();
    let mut roots: Vec<ObjectRef> = Vec::new();
    // SAFETY: single-threaded unit test.
    let stw = unsafe { StopTheWorldToken::new() };
    heap.collect_garbage(&stw, &mut roots, &NoMonitors);
    let cycle = heap.take_jfr_cycle().expect("a timed cycle");
    let (bytes, _ns) = cycle.uncommit.expect("the give-back was filed");
    assert!(bytes >= 16 * M as u64, "{bytes}");
}

/// zt t3/rl (t1/mk X-rl2): the booked pass is the plain pass plus a CPU
/// span into `uncommit_ns`. It releases what the plain pass releases;
/// under the accounting kill (`cpu_accounting` off) the share stays 0.
/// A positive reading (> 0) is the host row's: a 16 MiB give-back can
/// cost less than one tick of a coarse thread clock.
#[test]
fn zt3_rl_the_booked_uncommit_pass_charges_only_its_own_share() {
    let Some(heap) = freed_heap(1) else {
        return;
    };
    heap.counters
        .mark_loop
        .cpu_accounting
        .store(false, Ordering::Relaxed);
    let before = heap.collection_cpu_totals();
    let pass = heap.uncommit_pass_booked(Instant::now() + Duration::from_secs(2));
    assert!(pass.reset + pass.decommitted >= 16 * M, "{pass:?}");
    let after = heap.collection_cpu_totals();
    assert_eq!(after.uncommit_ns, 0, "kill: no clock read");
    assert_eq!(after.pause_thread_ns, before.pause_thread_ns);
    heap.counters
        .mark_loop
        .cpu_accounting
        .store(true, Ordering::Relaxed);
    let again = heap.uncommit_pass_booked(Instant::now() + Duration::from_secs(4));
    assert_eq!(again.reset + again.decommitted, 0);
    let last = heap.collection_cpu_totals();
    assert!(last.uncommit_ns >= after.uncommit_ns);
    assert_eq!(
        last.collection_ns() - last.uncommit_ns,
        after.collection_ns() - after.uncommit_ns,
        "only the uncommit share moves"
    );
    // zt t6/rv3 (`zgc-zt4x3-*` item 4): the positive half, which the
    // `>=` above cannot be. A fresh heap's pass releases >= 16 MiB with
    // accounting ON; on Linux's exact per-thread clock
    // (`CLOCK_THREAD_CPUTIME_ID`) that booking is strictly positive, as in
    // mk's X1-13 arm. Elsewhere the clock may be quantum-rounded (Windows
    // `GetThreadTimes`, ~15.6 ms) and 0 is a legal reading.
    let Some(fresh) = freed_heap(1) else {
        return;
    };
    fresh
        .counters
        .mark_loop
        .cpu_accounting
        .store(true, Ordering::Relaxed);
    let base = fresh.collection_cpu_totals().uncommit_ns;
    let booked = fresh.uncommit_pass_booked(Instant::now() + Duration::from_secs(2));
    assert!(booked.reset + booked.decommitted >= 16 * M, "{booked:?}");
    let charged = fresh.collection_cpu_totals().uncommit_ns;
    if cfg!(target_os = "linux") {
        assert!(charged > base, "a 16 MiB give-back books its CPU ({base} -> {charged})");
    } else {
        assert!(charged >= base);
    }
}


/// A collection in progress (it holds the gate) or a forwarding-word span
/// over the freed memory keeps the pass from touching anything.
#[test]
fn a_pass_defers_to_a_collection_and_spares_forwarding_words() {
    let Some(heap) = freed_heap(1) else {
        return;
    };
    let later = Instant::now() + Duration::from_secs(2);
    {
        let _gate = heap.counters.fp.uncommit_gate.lock();
        let busy = heap.uncommit_pass_at(later);
        assert!(busy.busy);
        assert_eq!(busy.reset + busy.decommitted, 0);
    }
    // The last slide's records cover the whole arena: nothing goes.
    heap.stamp_lo.store(heap.arena_base, Ordering::Release);
    heap.stamp_hi.store(heap.arena_end, Ordering::Release);
    let spared = heap.uncommit_pass_at(later);
    assert_eq!(spared.reset + spared.decommitted, 0, "{spared:?}");
    // Positive control: retired, the same pass releases.
    heap.retire_forwarding_words();
    let freed = heap.uncommit_pass_at(later);
    assert!(freed.reset + freed.decommitted >= 16 * M, "{freed:?}");
    assert!(heap.footprint_stats().uncommit_busy >= 1);
}


/// The slide's buffer trims change no answer: same moves, same map, same
/// rewritten field, on both screen arms; with the trims the fused arm
/// holds less forward index.
#[test]
fn slide_buffer_trims_change_no_answer() {
    for fused in [true, false] {
        let mut results = Vec::new();
        for trim in [true, false] {
            FUSED_SLIDE_SCREEN_OVERRIDE.with(|c| c.set(Some(fused)));
            let heap = ZgcRealHeap::with_capacity(256 * 1024);
            heap.set_tlab_enabled(false);
            heap.counters
                .fp
                .slide_trim_on
                .store(trim, Ordering::Relaxed);
            let base = heap.arena_base;
            let mut live = Vec::new();
            let mut prev: Option<ObjectRef> = None;
            for _ in 0..120 {
                heap.alloc_object(ClassId::new(1), 48); // garbage
                let o = heap.alloc_object(ClassId::new(1), 1);
                if let Some(p) = prev {
                    heap.set_field(o, 0, Value::Object(Some(p)));
                }
                prev = Some(o);
                live.push(o.as_ptr() as usize);
            }
            let (moved, reclaimed, map) = heap.relocate_stw_for_test(&live);
            let mut rel: Vec<(usize, usize)> =
                map.iter().map(|(f, t)| (*f - base, *t - base)).collect();
            rel.sort_unstable();
            let last = live[live.len() - 1];
            let new_last = map.get(&last).copied().unwrap_or(last);
            let field = heap.get_field(unsafe { ObjectRef::from_raw(new_last as *mut u8) }, 0);
            let field = match field {
                Value::Object(Some(r)) => r.as_ptr() as usize - base,
                other => panic!("chain broken: {other:?}"),
            };
            let s = heap.footprint_stats();
            assert!(
                s.slide_pairs_peak > 0,
                "moved={moved} fused={fused} trim={trim} {s:?}"
            );
            assert!(s.slide_map_peak > 0, "{s:?}");
            results.push((moved, reclaimed, rel, field, s.slide_index_peak));
        }
        FUSED_SLIDE_SCREEN_OVERRIDE.with(|c| c.set(None));
        let (on, off) = (&results[0], &results[1]);
        assert!(on.0 >= 100, "moved {}", on.0);
        assert_eq!((on.0, on.1, &on.2, on.3), (off.0, off.1, &off.2, off.3));
        if fused {
            assert!(on.4 < off.4, "index peak trim={} kill={}", on.4, off.4);
        }
    }
}
