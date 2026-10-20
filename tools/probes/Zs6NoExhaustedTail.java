// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zs s6/no (2026-10-15): the work a ZGC heap does between "nearly full" and
// the OutOfMemoryError, and that the program recovers after it.
//
// Below ~1.33 MiB free no occupancy rule fires (the zr6tp near-full floor),
// so every collection there is an allocation failure's, and each frees only
// what was allocated since the previous one: a fill that keeps ~21 % of what
// it allocates collects ~30 more times before its OOME, one whole-heap cycle
// per sliver of free space. HotSpot's ZGC stops at the first collection that
// leaves no whole 2 MiB page free. The opt-in
// `CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR=on` makes this VM do the same; this probe
// prices it (CPU, collections) and checks it changes no answer.
//
// Shapes:
//   grow   one thread: ArrayList of int[64], ~400 B of boxes per element (the
//          Zr2CpCapacity grow / ProxyBoxing shape), until OOME; drop; repeat
//          `rounds` times (each round is one more trip through the tail).
//   soft   grow, plus a SoftReference cache of 64 KiB arrays filled first: the
//          OOME must come with the cache cleared (SoftReference's guarantee).
//   mt     `threads` threads, each growing its own list until its OOME and
//          holding it until every thread has had one; then all drop.
//
// stdout (deterministic, compare with HotSpot -XX:+UseZGC):
//   zs6no exhausted-tail shape=<s> rounds=<r> oome=<count> recovered=true [soft_cleared=true] [oome_threads=<t>]
// stderr (the measurement; never compared):
//   zs6no tail retained=<n,...> gcs=<bean deltas> cpu_ms=<process CPU>
//
// Run: cratonvm -Xmx256m -Xlog:gc -cp tools/probes Zs6NoExhaustedTail grow 3
//      java -XX:+UseZGC -Xmx256m -Xlog:gc -cp tools/probes Zs6NoExhaustedTail grow 3
// Loop counters are `int` stepping by 1 on purpose, as elsewhere in this kit.
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.SoftReference;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;

public class Zs6NoExhaustedTail {
    static volatile Object sink;

    static long collections() {
        long n = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            long c = b.getCollectionCount();
            if (c > 0) {
                n += c;
            }
        }
        return n;
    }

    static long cpuMs() {
        java.lang.management.OperatingSystemMXBean os = ManagementFactory.getOperatingSystemMXBean();
        if (os instanceof com.sun.management.OperatingSystemMXBean) {
            return ((com.sun.management.OperatingSystemMXBean) os).getProcessCpuTime() / 1_000_000L;
        }
        return -1;
    }

    /** ~400 bytes of short-lived boxes per retained int[64], as boxing does. */
    static void churn(int i) {
        for (int k = 0; k < 8; k++) {
            Object[] args = {Integer.valueOf(i + 1000 + k), Long.valueOf(i * 31L + k),
                    Double.valueOf(i + 0.5)};
            sink = args;
        }
    }

    /** Grow `list` until an OutOfMemoryError; returns the elements added. */
    static int fill(List<Object> list) {
        int n = 0;
        try {
            for (int i = 0; i < 50_000_000; i++) {
                list.add(new int[64]);
                n++;
                churn(i);
            }
        } catch (OutOfMemoryError e) {
            return -n - 1;
        }
        return n;
    }

    /** After the drop: the heap serves a few MiB again. */
    static boolean recovered() {
        List<Object> l = new ArrayList<>();
        for (int i = 0; i < 1024; i++) {
            l.add(new byte[4096]);
        }
        return l.size() == 1024;
    }

    public static void main(String[] args) throws Exception {
        String shape = args.length > 0 ? args[0] : "grow";
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 1;
        int threads = args.length > 2 ? Integer.parseInt(args[2]) : 4;
        long gc0 = collections();
        StringBuilder retained = new StringBuilder();
        int oomes = 0;
        boolean recovered = true;
        String extra = "";
        switch (shape) {
            case "grow":
            case "soft": {
                boolean softCleared = true;
                for (int r = 0; r < rounds; r++) {
                    List<SoftReference<byte[]>> cache = new ArrayList<>();
                    if (shape.equals("soft")) {
                        for (int i = 0; i < 256; i++) {
                            cache.add(new SoftReference<>(new byte[64 * 1024]));
                        }
                    }
                    List<Object> list = new ArrayList<>();
                    int n = fill(list);
                    if (n < 0) {
                        oomes++;
                        n = -n - 1;
                    }
                    list = null;
                    if (shape.equals("soft")) {
                        for (SoftReference<byte[]> s : cache) {
                            if (s.get() != null) {
                                softCleared = false;
                            }
                        }
                    }
                    cache = null;
                    recovered &= recovered();
                    retained.append(r == 0 ? "" : ",").append(n);
                }
                if (shape.equals("soft")) {
                    extra = " soft_cleared=" + softCleared;
                }
                break;
            }
            case "mt": {
                // No barrier object: awaiting one allocates, and on a full
                // heap that is an OutOfMemoryError inside the barrier. Each
                // thread spins (allocation-free) until every thread had its
                // OOME, holding its list.
                AtomicInteger oomed = new AtomicInteger();
                AtomicInteger arrived = new AtomicInteger();
                AtomicInteger total = new AtomicInteger();
                Thread[] ts = new Thread[threads];
                for (int t = 0; t < threads; t++) {
                    ts[t] = new Thread(() -> {
                        List<Object> list = new ArrayList<>();
                        int n = fill(list);
                        if (n < 0) {
                            oomed.incrementAndGet();
                            n = -n - 1;
                        }
                        total.addAndGet(n);
                        arrived.incrementAndGet();
                        while (arrived.get() < threads) {
                            Thread.onSpinWait();
                        }
                        list.clear();
                    });
                    ts[t].start();
                }
                for (Thread t : ts) {
                    t.join();
                }
                oomes = 1;
                recovered = recovered();
                retained.append(total.get());
                extra = " oome_threads=" + oomed.get();
                break;
            }
            default:
                throw new IllegalArgumentException(shape);
        }
        System.err.println("zs6no tail retained=" + retained + " gcs=" + (collections() - gc0)
                + " cpu_ms=" + cpuMs());
        System.out.println("zs6no exhausted-tail shape=" + shape + " rounds="
                + (shape.equals("mt") ? 1 : rounds) + " oome=" + oomes + " recovered=" + recovered
                + extra);
    }
}
