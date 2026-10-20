// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 95, lane L6 -- the isolating rows of the
// virtual-thread lifecycle switches: 10 000 trivial tasks on
// newVirtualThreadPerTaskExecutor, so a row is almost only the per-thread
// registration, mount and death.
//
// Rows:
//   plain    -- the lifecycle alone. The row of CRATONVM_VTHREAD_ONE_PUBLISH
//               (page i91-L6-virtual-thread-code-is-never-compiled, wave 95);
//   inflated -- the same after 4000 objects' monitors were inflated (an
//               Object.wait(1) on each, from 64 platform threads) and kept
//               reachable: every death's monitor sweep walks them. The row of
//               CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT (page
//               i94-L6-thread-death-sweeps-every-inflated-monitor).
//
// A/B (one binary, interleaved, medians; both modes; java for HotSpot):
//   cratonvm L6W95VirtualThreadDeathBench 5 plain
//   CRATONVM_VTHREAD_ONE_PUBLISH=1 cratonvm L6W95VirtualThreadDeathBench 5 plain
//   cratonvm L6W95VirtualThreadDeathBench 5 inflated
//   CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT=1 cratonvm L6W95VirtualThreadDeathBench 5 inflated
//   (both switches together, both rows)
// Census (stderr): CRATONVM_DBG_VTHREAD_CENSUS=1 adds the per-thread
// register_lookup / register_publish / register_manager / teardown_registry /
// teardown_purge / teardown_monitor_sweep split, the per-slice pickup /
// return / mount_registry split, death_sweeps_skipped and
// death_sweep_skip_misses (must be 0), and the at-exit manager / registry
// entry counts (the leak row of
// i94-L6-terminated-jdk-virtual-threads-stay-in-the-manager-map, with
// CRATONVM_VTHREAD_RETIRE_MANAGER_ENTRIES=0 as its B arm).
//
// Timings go to stderr ("[bench] <row> ms=..."), best of `reps` (arg 0,
// default 5) after one warm-up rep; stdout is only the checksum lines.
//
// HOTSPOT_EXPECTED_BEGIN
// checksum plain virtual=10000
// checksum inflated virtual=10000 monitors=4000
// HOTSPOT_EXPECTED_END

import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicInteger;

public class L6W95VirtualThreadDeathBench {
    static final int TASKS = 10_000;
    static final int MONITORS = 4000;
    static final int INFLATERS = 64;
    /** Kept reachable for the life of the run, so their monitors stay. */
    static final List<Object> KEPT = new ArrayList<>();

    static long rep() {
        AtomicLong sum = new AtomicLong();
        try (ExecutorService ex = Executors.newVirtualThreadPerTaskExecutor()) {
            for (int i = 0; i < TASKS; i++) {
                ex.submit(() -> {
                    sum.incrementAndGet();
                });
            }
        }
        return sum.get();
    }

    static long timed(String row, int reps) {
        long check = rep();
        long best = Long.MAX_VALUE;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            check = rep();
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.err.println("[bench] " + row + " ms=" + (best / 1_000_000.0));
        return check;
    }

    static int inflate() throws InterruptedException {
        Object[] objs = new Object[MONITORS];
        for (int i = 0; i < MONITORS; i++) {
            objs[i] = new Object();
            KEPT.add(objs[i]);
        }
        AtomicInteger inflated = new AtomicInteger();
        List<Thread> workers = new ArrayList<>();
        for (int w = 0; w < INFLATERS; w++) {
            int from = w;
            workers.add(Thread.ofPlatform().start(() -> {
                for (int i = from; i < MONITORS; i += INFLATERS) {
                    Object o = objs[i];
                    synchronized (o) {
                        try {
                            o.wait(1);
                        } catch (InterruptedException e) {
                            return;
                        }
                    }
                    inflated.incrementAndGet();
                }
            }));
        }
        for (Thread t : workers) {
            t.join();
        }
        return inflated.get();
    }

    public static void main(String[] args) throws Exception {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        String row = args.length > 1 ? args[1] : "all";
        if (row.equals("all") || row.equals("plain")) {
            System.out.println("checksum plain virtual=" + timed("plain", reps));
        }
        if (row.equals("all") || row.equals("inflated")) {
            int monitors = inflate();
            System.out.println("checksum inflated virtual=" + timed("inflated", reps)
                    + " monitors=" + monitors);
        }
    }
}
