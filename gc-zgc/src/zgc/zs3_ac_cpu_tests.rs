//! zs s3/ac: the collection's CPU outside the mark (sweep pool, slide-pass
//! pullers, the collecting thread's whole pause), the shutdown
//! `[GC] zgc-cpu:` line, and the SATB direct hand-off.
use super::*;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
}

fn set_accounting(heap: &ZgcRealHeap, on: bool) {
    heap.counters
        .mark_loop
        .cpu_accounting
        .store(on, Ordering::Relaxed);
}

/// Burn `ms` of THIS thread's CPU (a wall spin under-charges on a loaded
/// runner: zs s2/mc2 finding 12).
fn spin_cpu_ms(ms: u64) {
    let Some(t0) = mark::thread_cpu_nanos() else {
        return;
    };
    let mut x = 0u64;
    while mark::thread_cpu_nanos().unwrap_or(u64::MAX) < t0 + ms * 1_000_000 {
        x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
    }
}

#[test]
fn zs3ac_the_totals_add_only_the_disjoint_shares() {
    let t = ZCollectionCpuTotals {
        mark_stw_pool_ns: 1_000_000,
        mark_stw_driver_ns: 50_000_000, // inside pause_thread
        mark_serial_ns: 60_000_000,     // inside pause_thread
        mark_conc_pool_ns: 2_000_000,
        sweep_pool_ns: 3_000_000,
        relocate_pool_ns: 4_000_000,
        uncommit_ns: 5_000_000, // the ZUncommitter, its own share (zt t3)
        vm_pause_ns: 6_000_000, // zt t6/ob4: the VM's side of the pauses
        pause_thread_ns: 100_000_000,
        pause_cycles: 7,
        satb_flush_ns: 9_000_000, // zt t3/mk: mutator CPU, printed, not summed
    };
    assert_eq!(t.collection_ns(), 121_000_000);
    let line = t.summary_line(Some(1_210_000_000), true);
    assert_eq!(
        line,
        "[GC] zgc-cpu: mark_stw_pool_ms=1 mark_stw_driver_ms=50 mark_serial_ms=60 \
         mark_conc_pool_ms=2 sweep_pool_ms=3 relocate_pool_ms=4 uncommit_ms=5 \
         pause_thread_ms=100 collection_ms=121 process_ms=1210 collection_pct=10.0 \
         pause_cycles=7 accounting=1 satb_flush_ms=9 vm_pause_ms=6"
    );
    let none = ZCollectionCpuTotals::default().summary_line(None, false);
    assert!(
        none.ends_with(
            "collection_ms=0 process_ms=- collection_pct=- pause_cycles=0 accounting=0 \
             satb_flush_ms=0 vm_pause_ms=0"
        ),
        "{none}"
    );
}


#[test]
fn zs3ac_the_process_clock_reads_and_advances() {
    let Some(a) = mark::process_cpu_nanos() else {
        return; // no process clock on this platform
    };
    spin_cpu_ms(40);
    let b = mark::process_cpu_nanos().expect("the clock read once");
    assert!(
        b > a,
        "40 ms of CPU must advance the process clock ({a} -> {b})"
    );
}

/// X2: the spawned pullers of a slide pass charge `relocate_pool_ms=`
/// through the heap's `par_bal()` sink; the kill arm hands no sink and
/// charges nothing (positive and kill controls in one).
#[test]
fn zs3ac_slide_pass_pullers_charge_the_relocate_pool() {
    let heap = ZgcRealHeap::with_capacity(64 * 1024);
    let items: Vec<usize> = (0..64).collect();
    for on in [false, true] {
        set_accounting(&heap, on);
        let before = heap.collection_cpu_totals().relocate_pool_ns;
        let got: Vec<usize> = zgc_par_chunks(heap.par_bal(), &items, 4, |c: &[usize]| {
            spin_cpu_ms(10);
            c.len()
        });
        assert_eq!(got.iter().sum::<usize>(), items.len());
        let charged = heap.collection_cpu_totals().relocate_pool_ns - before;
        if !on {
            assert_eq!(charged, 0, "the kill arm reads no clock");
        } else if mark::thread_cpu_nanos().is_some() {
            // 32 chunks x 10 ms over 4 pullers: the 3 spawned ones run
            // most of them.
            assert!(
                charged >= 50_000_000,
                "spawned pullers charged {charged} ns"
            );
        }
    }
    // A bare `bool` pass (the tests' spelling) carries no sink.
    let p: ZParPass<'_> = true.into();
    assert!(p.balance && p.cpu.is_none());
    set_accounting(&heap, true);
}

/// zt t8/rl2 (`zgc-zs3ac-collection-cpu-residual-shares` item 1, zs4rv3
/// item 1): the POOLED relocation pass (`CRATONVM_ZGC_RELOCATE_POOL=1`)
/// charges its helpers to `relocate_pool_ms=` as the scoped pass does,
/// and the kill arm (no sink) charges nothing. Then, on both arms, a pass
/// whose EVERY chunk panics after 10 ms of CPU still charges the
/// helpers' spins (each puller takes one chunk and unwinds): before the
/// on-drop span, an unwinding helper skipped its booking and this read 0.
#[test]
fn zt8rl2_pooled_and_panicking_relocation_pullers_charge_the_relocate_pool() {
    let heap = ZgcRealHeap::with_capacity(64 * 1024);
    let pool = cratonvm_gc_core::evac_pool::EvacPool::per_heap(0, "zgc-reloc-zt8rl2");
    let items: Vec<usize> = (0..64).collect();
    let clock = mark::thread_cpu_nanos().is_some();
    for on in [false, true] {
        set_accounting(&heap, on);
        let before = heap.collection_cpu_totals().relocate_pool_ns;
        let cpu = heap.par_bal().cpu;
        let got: Vec<usize> = zgc_par_chunks_pooled(&pool, &items, 4, 8, cpu, |c: &[usize]| {
            spin_cpu_ms(10);
            c.len()
        });
        assert_eq!(got.iter().sum::<usize>(), items.len());
        let charged = heap.collection_cpu_totals().relocate_pool_ns - before;
        if !on {
            assert_eq!(charged, 0, "the kill arm reads no clock");
        } else if clock {
            // 32 chunks x 10 ms over 4 pullers, 3 of them pool helpers.
            // zt t11/x8 (x6 item 6): the bound is ONE helper chunk. The
            // calling thread pulls too and is not priced, so on a loaded
            // box whose helpers wake late it can take most of the 32
            // chunks; 50 ms failed there without anything being wrong.
            assert!(
                charged >= 10_000_000,
                "pooled helpers charged {charged} ns"
            );
        }
    }
    set_accounting(&heap, true);
    for pooled in [false, true] {
        let before = heap.collection_cpu_totals().relocate_pool_ns;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let every_chunk_panics = |_c: &[usize]| -> usize {
                spin_cpu_ms(10);
                panic!("zt8rl2: every chunk panics");
            };
            if pooled {
                zgc_par_chunks_pooled(
                    &pool,
                    &items,
                    4,
                    8,
                    heap.par_bal().cpu,
                    every_chunk_panics,
                )
            } else {
                zgc_par_chunks_with(&items, 4, 8, heap.par_bal().cpu, every_chunk_panics)
            }
        }));
        assert!(r.is_err(), "pooled={pooled}: the panic propagates");
        let charged = heap.collection_cpu_totals().relocate_pool_ns - before;
        if clock {
            // 3 helpers x at least 10 ms each, booked while unwinding.
            assert!(
                charged >= 20_000_000,
                "pooled={pooled}: panicking helpers charged {charged} ns"
            );
        }
    }
    // The pool survives the panicked passes.
    let ok = zgc_par_chunks_pooled(&pool, &items, 4, 8, None, |c: &[usize]| c.len());
    assert_eq!(ok.iter().sum::<usize>(), 64);
}

fn churn(heap: &ZgcRealHeap, live: usize, dead: usize) -> Vec<ObjectRef> {
    let every = 1 + dead / live.max(1);
    let mut keep = Vec::with_capacity(live);
    for i in 0..live + dead {
        let o = heap.alloc_object(ClassId::new(1), 2);
        if i % every == 0 && keep.len() < live {
            keep.push(o);
        }
    }
    keep
}

/// X1 + X3: a collection books the collecting thread's pause CPU and one
/// pause cycle, and a parallel sweep books its workers; the kill arm
/// books nothing at all. The `> 0` checks need a fine thread clock
/// (Linux); a Windows quantum can round a short collection to 0.
#[test]
fn zs3ac_a_collection_books_the_pause_and_the_sweep_pool_and_the_kill_arm_nothing() {
    for on in [false, true] {
        let heap = ZgcRealHeap::with_capacity(32 * 1024 * 1024);
        set_accounting(&heap, on);
        // zt t27/gn5 (gn page item 10): single generation -- a
        // generational cycle sweeps on one thread (`sweep_workers(true)`),
        // so the pool would book nothing.
        heap.set_generational_enabled(false);
        heap.set_sweep_workers(4);
        let mut roots = churn(&heap, 2_000, 60_000);
        {
            // SAFETY: single-threaded test; no mutator runs.
            let stw = unsafe { StopTheWorldToken::new() };
            let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
        }
        let t = heap.collection_cpu_totals();
        if on {
            assert_eq!(t.pause_cycles, 1, "one collection booked");
            if cfg!(target_os = "linux") {
                assert!(t.pause_thread_ns > 0, "{t:?}");
                if heap.sweep_workers(false) > 1 {
                    assert!(t.sweep_pool_ns > 0, "{t:?}");
                }
            }
            assert!(
                t.pause_thread_ns >= t.mark_stw_driver_ns + t.mark_serial_ns,
                "the driver and the serial marker run on the pausing thread: {t:?}"
            );
        } else {
            assert_eq!(
                t,
                ZCollectionCpuTotals::default(),
                "the kill arm books no share and no cycle"
            );
        }
    }
}

/// zt t9/x6 (review of t8/rl2, zs3ac item 2): the CONCURRENT mark pool is
/// seeded by this heap's accounting switch, as the stop-the-world pool is
/// since t8; it read the process flag at its construction, so a heap
/// seeded off still had its concurrent workers price themselves. Both
/// seeds, each against the process default (ON in this test binary), so
/// the `false` arm fails without the fix.
#[test]
fn zt9x6_the_concurrent_pool_follows_the_heaps_accounting_seed() {
    for on in [false, true] {
        let heap = ZgcRealHeap::new_shared(64 * 1024 * 1024);
        set_accounting(&heap, on);
        let root = heap.alloc_object(ClassId::new(1), 1);
        {
            // SAFETY: no other thread touches the heap in this test.
            let stw = unsafe { StopTheWorldToken::new() };
            assert!(heap.start_concurrent_mark(&stw, &[root.as_ptr() as u64]));
        }
        let seeded = heap
            .counters
            .conc_pool
            .lock()
            .as_ref()
            .map(|pool| pool.cpu_accounting_on());
        assert_eq!(seeded, Some(on), "the pool follows the heap's seed");
        {
            // SAFETY: as above.
            let stw = unsafe { StopTheWorldToken::new() };
            let mut roots = [root];
            let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
        }
        if !on {
            assert_eq!(heap.mark_cpu_totals().2, 0, "seeded off: no concurrent pool CPU");
        }
    }
}

/// X4: with a concurrent pool running, a full SATB thread buffer goes
/// straight to `push_mutator_batch` (engagement counted, the heap
/// ingress untouched); the kill arm goes through the ingress. Either
/// way every recorded white value survives the cycle -- SATB's promise.
#[test]
fn zs3ac_a_full_satb_buffer_goes_straight_to_the_pool_and_both_arms_keep_it() {
    let cap = mark::Z_MARK_MUTATOR_BUFFER_CAPACITY;
    for direct in [true, false] {
        let heap = ZgcRealHeap::new_shared(64 * 1024 * 1024);
        heap.dbg_set_satb_filter_mode(Z_SATB_FILTER_FLUSH);
        heap.counters
            .satb_direct_handoff
            .store(direct, Ordering::Relaxed);
        let root = heap.alloc_object(ClassId::new(1), 1);
        let whites: Vec<usize> = (0..cap)
            .map(|_| heap.alloc_object(ClassId::new(1), 1).as_ptr() as usize)
            .collect();
        {
            // SAFETY: no other thread touches the heap in this test.
            let stw = unsafe { StopTheWorldToken::new() };
            assert!(heap.start_concurrent_mark(&stw, &[root.as_ptr() as u64]));
        }
        let pool_running = heap.counters.conc_pool.lock().is_some();
        assert!(pool_running, "the cycle runs a concurrent pool");
        let ingress_before = heap.counters.mark_ingress.pushed_total();
        for &w in &whites {
            heap.satb_pre_barrier(w);
        }
        let ingress_pushed = heap.counters.mark_ingress.pushed_total() - ingress_before;
        let (handed, on) = heap.satb_direct_handoff_report();
        assert_eq!(on, direct);
        if direct && pool_running {
            assert_eq!(handed, cap, "positive control: the whole batch went direct");
            assert_eq!(ingress_pushed, 0, "the direct arm skips the heap ingress");
        } else {
            assert_eq!(handed, 0, "the kill arm hands nothing directly");
            if pool_running {
                assert!(ingress_pushed > 0, "the kill arm goes through the ingress");
            }
        }
        {
            // SAFETY: as above.
            let stw = unsafe { StopTheWorldToken::new() };
            let mut roots = [root];
            let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
        }
        let kept = whites
            .iter()
            .filter(|&&w| heap.is_object_address(w).is_some())
            .count();
        assert_eq!(kept, cap, "direct={direct}: every recorded value survived");
    }
}
