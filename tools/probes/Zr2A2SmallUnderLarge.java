// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z2/a2 (2026-10-12): what a SMALL allocation pays while other threads
// allocate large arrays -- the split `Zr1AlLargeAllocContention` cannot make.
//
// That probe's small threads allocate `new Object[4]` that never escapes, so
// HotSpot's escape analysis may scalar-replace them and its twin row is not an
// allocation figure; and it has no no-contention baseline, so a slowdown from
// GC pauses (every collection on this backend is stop-the-world) cannot be
// told from a slowdown on the allocation path itself. This one:
//
// * makes every small object ESCAPE into a per-thread ring, so both VMs
//   really allocate it;
// * lets the small shape be chosen: `ref` = `new Object[4]` (a reference
//   array: no compiled tier on CratonVM bumps it inline -- both tiers refuse
//   a reference element and call `jit_anewarray_object`), `prim` =
//   `new long[4]` (a primitive array: inline TLAB bump under ZGC since round
//   16), `obj` = `new Node()` (an instance: inline bump), `refstore` = `ref`
//   plus one reference store into the new array (the old probe's shape);
// * prints the collector's own cycle count and accumulated collection time
//   (GarbageCollectorMXBean) beside the CPU time, so a row says how much of
//   the run the world was stopped.
//
// Usage: java -Xmx512m Zr2A2SmallUnderLarge [large=4] [small=4] [seconds=10] [kib=1024] [kind=ref]
//
// The rows that decide (interleaved, `/usr/bin/time` user+sys beside them):
//   large=0 small=4 kind=ref   vs  large=4 small=4 kind=ref    -> the contention cost
//   large=4 small=4 kind=ref   vs  large=4 small=4 kind=prim   -> the helper-call cost
//   HotSpot twins of each (`java -XX:+UseZGC -Xmx512m ...`).
// The figure of merit is small_ns_per_alloc_cpu (CPU nanoseconds per small
// allocation, summed over the small threads); small_allocs_per_s is printed
// too but is wall-clock and only comparable within one interleaved run.
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.concurrent.atomic.AtomicLong;

public final class Zr2A2SmallUnderLarge {
    static volatile boolean stop;
    static volatile Object sink;

    static final class Node {
        Object a;
        long b;
        long c;
    }

    // One method per shape: 64 allocations, each escaping into the ring and
    // nothing else, so the shapes differ only in what is allocated.
    // `refstore` adds one reference store into the new array (an `aastore`
    // with its store check and barrier) to price that separately.
    static void burstRef(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new Object[4];
        }
    }

    static void burstPrim(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new long[4];
        }
    }

    static void burstObj(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new Node();
        }
    }

    static void burstRefStore(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            Object[] r = new Object[4];
            r[0] = ring;
            ring[(int) ((n + k) & 1023)] = r;
        }
    }

    public static void main(String[] args) throws Exception {
        int large = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        int small = args.length > 1 ? Integer.parseInt(args[1]) : 4;
        int seconds = args.length > 2 ? Integer.parseInt(args[2]) : 10;
        int kib = args.length > 3 ? Integer.parseInt(args[3]) : 1024;
        String kind = args.length > 4 ? args[4] : "ref";
        final int code = switch (kind) {
            case "ref" -> 0;
            case "prim" -> 1;
            case "obj" -> 2;
            case "refstore" -> 3;
            default -> throw new IllegalArgumentException(
                    "kind must be ref, prim, obj or refstore: " + kind);
        };
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        AtomicLong largeOps = new AtomicLong();
        AtomicLong smallAllocs = new AtomicLong();
        AtomicLong largeCpu = new AtomicLong();
        AtomicLong smallCpu = new AtomicLong();
        long gcCount0 = 0;
        long gcTime0 = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            gcCount0 += Math.max(0, gc.getCollectionCount());
            gcTime0 += Math.max(0, gc.getCollectionTime());
        }
        Thread[] ts = new Thread[large + small];
        for (int i = 0; i < ts.length; i++) {
            final boolean isLarge = i < large;
            ts[i] = new Thread(() -> {
                long n = 0;
                long acc = 0;
                Object[] ring = new Object[1024];
                while (!stop) {
                    if (isLarge) {
                        long[] a = new long[(kib << 10) / 8];
                        a[a.length - 1] = n;
                        acc += a[0] + a[a.length - 1];
                        n++;
                    } else {
                        switch (code) {
                            case 0 -> burstRef(ring, n);
                            case 1 -> burstPrim(ring, n);
                            case 2 -> burstObj(ring, n);
                            default -> burstRefStore(ring, n);
                        }
                        n += 64;
                    }
                }
                sink = acc + ring.length;
                long cpu = mx.getCurrentThreadCpuTime();
                if (isLarge) {
                    largeOps.addAndGet(n);
                    largeCpu.addAndGet(cpu);
                } else {
                    smallAllocs.addAndGet(n);
                    smallCpu.addAndGet(cpu);
                }
            });
            ts[i].start();
        }
        Thread.sleep(seconds * 1000L);
        stop = true;
        for (Thread t : ts) {
            t.join();
        }
        long gcCount = -gcCount0;
        long gcTime = -gcTime0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            gcCount += Math.max(0, gc.getCollectionCount());
            gcTime += Math.max(0, gc.getCollectionTime());
        }
        double lc = Math.max(1, largeCpu.get()) / 1e9;
        double sc = Math.max(1, smallCpu.get()) / 1e9;
        System.out.printf(java.util.Locale.ROOT,
                "Zr2A2SmallUnderLarge kind=%s large=%d small=%d kib=%d large_ops=%d large_cpu_s=%.3f"
                        + " large_ops_per_cpu_s=%.1f small_allocs=%d small_cpu_s=%.3f"
                        + " small_ns_per_alloc_cpu=%.2f small_allocs_per_s=%.1f gc_count=%d gc_time_ms=%d%n",
                kind, large, small, kib, largeOps.get(), lc, largeOps.get() / lc,
                smallAllocs.get(), sc,
                smallAllocs.get() == 0 ? 0.0 : smallCpu.get() / (double) smallAllocs.get(),
                smallAllocs.get() / (double) seconds, gcCount, gcTime);
    }
}
