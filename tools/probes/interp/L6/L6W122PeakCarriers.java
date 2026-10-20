// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 122, lane L6 -- the peak-carrier measurement of
// proposal i26-L4-proposal-let-a-virtual-thread-unmount-under-the-method-handle-natives
// ("Wave 115": 1 000 virtual threads, each calling `Method.invoke` on a `void`
// method that sleeps 100 ms; `ThreadMXBean.getPeakThreadCount` and wall time).
// HotSpot unmounts each sleeper (`Method.invoke` is Java there), so the peak
// is about the scheduler's carriers plus the JVM's own threads and the wall
// time ~100-200 ms. A VM that pins the carrier under the `invoke` native holds
// one carrier per sleeper (the starvation compensation adds them): peak near
// 1 000 and a longer wall.
//
// The compared lines are deterministic (every call completed, the sum); the
// measurement is on `TIME` lines, which the battery ignores:
//   TIME peak-threads=N        ThreadMXBean.getPeakThreadCount() after the run
//   TIME peak-over-baseline=N  that peak minus the count before the run
//   TIME wall-ms=N             the 1 000 threads, start to last join
//   TIME carriers=N            ForkJoinPool carrier threads alive at the end
//
// Run, both modes, switch off vs on (i26-L4 `CRATONVM_MH_VOID_TAIL_UNMOUNT`):
//   cratonvm L6W122PeakCarriers
//   CRATONVM_MH_VOID_TAIL_UNMOUNT=1 cratonvm L6W122PeakCarriers
// Flip evidence: with the switch on, `peak-over-baseline` falls to about the
// scheduler parallelism (HotSpot's shape) and `wall-ms` to ~100-300.
//
// HotSpot 25.0.3 (Windows, 32 logical CPUs; three runs, 2026-10-19):
//   TIME peak-threads=41 peak-over-baseline=34 wall-ms=151..204 carriers=32
//
// HOTSPOT_EXPECTED_BEGIN
// invoke: completed=1000 sum=499500
// HOTSPOT_EXPECTED_END

import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.lang.reflect.Method;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

public class L6W122PeakCarriers {
    static final AtomicInteger COMPLETED = new AtomicInteger();
    static final AtomicLong SUM = new AtomicLong();

    /** The `void` target: sleeps 100 ms, then counts. */
    public static void sleeper(int id) throws InterruptedException {
        Thread.sleep(100);
        COMPLETED.incrementAndGet();
        SUM.addAndGet(id);
    }

    static int carriers() {
        int n = 0;
        for (Thread t : Thread.getAllStackTraces().keySet()) {
            String name = t.getName();
            if (name.startsWith("ForkJoinPool-") && name.contains("-worker-")) {
                n++;
            }
        }
        return n;
    }

    public static void main(String[] args) throws Exception {
        Method m = L6W122PeakCarriers.class.getMethod("sleeper", int.class);
        // Warm the reflective path on a platform thread first, so the
        // measured run does not include the accessor's first link.
        Thread warm = new Thread(() -> {
            try {
                for (int i = 0; i < 3; i++) {
                    m.invoke(null, 0);
                }
            } catch (ReflectiveOperationException e) {
                throw new AssertionError(e);
            }
        });
        warm.start();
        warm.join();
        COMPLETED.set(0);
        SUM.set(0);

        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        int before = mx.getThreadCount();
        mx.resetPeakThreadCount();
        int n = 1000;
        Thread[] ts = new Thread[n];
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            final int id = i;
            ts[i] = Thread.ofVirtual().start(() -> {
                try {
                    m.invoke(null, id);
                } catch (ReflectiveOperationException e) {
                    throw new AssertionError(e);
                }
            });
        }
        for (Thread t : ts) {
            t.join();
        }
        long wallMs = (System.nanoTime() - t0) / 1_000_000;
        int peak = mx.getPeakThreadCount();
        System.out.println("invoke: completed=" + COMPLETED.get() + " sum=" + SUM.get());
        System.out.println("TIME peak-threads=" + peak);
        System.out.println("TIME peak-over-baseline=" + (peak - before));
        System.out.println("TIME wall-ms=" + wallMs);
        System.out.println("TIME carriers=" + carriers());
    }
}
