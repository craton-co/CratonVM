// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// The MECHANISM of `bug-h2-testcachedqueryresults-zgc-oom-livelock-20260829`,
// without H2's SQL.
//
// That page's assertion is `Expected: 100000 actual: 98304`, and 98304 is
// exactly `ConcurrentHashMap`'s resize threshold for a 131072-bucket table. Its
// failing allocation is one thing, 1696 times: a REFERENCE ARRAY OF LENGTH
// 65536 -- 524 304 bytes -- which is the CHM table at that resize. So the two
// numbers are one event, and this probe is that event: five threads filling a
// `ConcurrentHashMap.newKeySet()` to 100000 while small-object churn fragments
// the arena underneath them.
//
// WHY A PROBE AND NOT THE CLASS. `TestCachedQueryResults` cannot be measured on
// a shared host. Its inner statement is
// `SELECT counter FROM Counter WHERE id = 1 FOR UPDATE WAIT 0.5`, so under
// contention it fails with `Timeout trying to lock table "COUNTER"` -- a
// starved 0.5 s SQL lock that says nothing about the heap, and which is what
// two capped runs on a load-20+ host produced. It runs 641 s idle and past
// 3600 s loaded, and a run killed by `timeout` prints NO `[GC]` summary, so the
// counters the question turns on are simply absent.
//
// The page's own accounting is the oracle here: every entry that never reaches
// the set is one `OutOfMemoryError` swallowed by a task nobody calls `get()`
// on. This probe catches them instead of dropping them, so `missing` and
// `oom` should agree, and on HotSpot both are zero.
//
// Read it with `CRATONVM_GC_STATS=1`. `relocation_on_proven_jit=0` VOIDS the
// run -- see the page -- so check that before comparing anything.
//
// IT DOES NOT REPRODUCE, and that is why it is checked in.
//
// MEASURED 2026-09-01, `--Xmx 1g`, both `CRATONVM_XT_HELPER_WINDOW_PIN` arms and
// HotSpot: `size=100000 missing=0 oom=0 PASS` every time, with `arena=0` and no
// `native reference array of length 65536` failure at all. So this eliminates
// two things the page's own history had left open:
//
//   * **`ConcurrentHashMap` growth to 100000 is not sufficient**, even with four
//     churn threads shattering the arena underneath it. The page retracted
//     `98304 == 131072 - (131072 >>> 2)` as "a coincidence" on the strength of
//     `ChmKeySetGrowth`, then the 2026-08-30 addendum un-retracted it on the
//     strength of the failing allocation being that table. Both stand: the
//     number IS the CHM resize, and the resize alone does NOT fail. What fails
//     it is the ARENA STATE H2 builds -- `spans=219425 largest_span=246704` --
//     which needs sustained mixed-lifetime allocation over hundreds of seconds,
//     not a burst.
//   * **`helper_windows=0` in every run of this probe.** A pure-Java allocation
//     workload does not put peers inside Rust helpers at collection time; H2
//     does, through file I/O, MVStore chunk compression and the JDBC path. So a
//     helper-window repair cannot be A/B'd here -- an instrument armed where it
//     cannot fire -- and `TestMultiThread` is the class that does arm it.
//
// The first probe shape tried here allocated `Object[65536]` directly in a loop
// and is also recorded as not reproducing: `served=185964 failed=0` on CratonVM
// against `served=254483 failed=0` on HotSpot, with relocation engaging
// (`relocation_on_proven_jit=118`, `compaction_cycles=116`). A heap that
// compacts does not fragment into this failure, which is the whole point.
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;

public final class ZgcRefArrayFragProbe {
    private static final int TARGET = 100_000;
    private static final int ADDER_THREADS = 5;
    private static final int CHURN_THREADS = 4;
    private static final int LIVE_WINDOW = 512;

    private static volatile boolean stop = false;

    public static void main(String[] args) throws Exception {
        int target = args.length > 0 ? Integer.parseInt(args[0]) : TARGET;

        // Small-object churn: what shatters the arena into the
        // `spans=219425 largest_span=246704` state the page reports, and what
        // keeps peers inside compiled code and VM helpers at collection time.
        Thread[] churn = new Thread[CHURN_THREADS];
        for (int t = 0; t < CHURN_THREADS; t++) {
            churn[t] = new Thread(() -> {
                Object[] live = new Object[LIVE_WINDOW];
                int i = 0;
                while (!stop) {
                    int n = 8 + ((i * 31) & 255);
                    live[i % LIVE_WINDOW] = new long[n];
                    // A String too: a different size class, and it routes
                    // through VM helpers rather than a bare inline TLAB bump.
                    live[(i + 7) % LIVE_WINDOW] = ("k" + i).intern();
                    i++;
                }
            }, "churn-" + t);
            churn[t].setDaemon(true);
            churn[t].start();
        }

        // The set the page's assertion is about, grown the way it grows there.
        final Set<Integer> set = ConcurrentHashMap.newKeySet();
        final java.util.concurrent.atomic.AtomicInteger oom =
                new java.util.concurrent.atomic.AtomicInteger();
        final java.util.concurrent.atomic.AtomicInteger next =
                new java.util.concurrent.atomic.AtomicInteger();

        Thread[] adders = new Thread[ADDER_THREADS];
        for (int t = 0; t < ADDER_THREADS; t++) {
            adders[t] = new Thread(() -> {
                for (;;) {
                    int v = next.getAndIncrement();
                    if (v >= target) {
                        return;
                    }
                    try {
                        set.add(v);
                    } catch (OutOfMemoryError e) {
                        // The page's silent failure, made loud. There, the
                        // `OutOfMemoryError` goes past a `catch (SQLException)`
                        // into a `FutureTask` nobody calls `get()` on, so the
                        // only trace is the final count.
                        oom.incrementAndGet();
                    }
                }
            }, "adder-" + t);
            adders[t].start();
        }
        for (Thread t : adders) {
            t.join();
        }
        stop = true;
        for (Thread t : churn) {
            t.join(1000);
        }

        int size = set.size();
        System.out.println("ZgcRefArrayFragProbe target=" + target
                + " size=" + size + " missing=" + (target - size)
                + " oom=" + oom.get()
                + (size == target ? " PASS" : " FAIL"));
    }
}
