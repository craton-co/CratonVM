// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z1/al (2026-10-12): `ZgcRealHeap::alloc_raw` zeroed a reused span while
// holding the arena lock.
//
// Every object a TLAB will not take (above `max_tlab_alloc`, at most 32 KiB)
// goes through `alloc_raw`, and a span reused from the free list is dirty, so
// it is `memset` before it is handed out. That `memset` ran inside the arena
// guard -- the one lock every TLAB refill and every other direct allocation
// takes -- so `L` threads allocating 1 MiB arrays serialised each other AND the
// `S` small-object threads behind every 1 MiB `memset`. The TLAB carve has
// always zeroed outside the guard; `CRATONVM_ZGC_RAW_ZERO_OUTSIDE_LOCK`
// (default ON, `=0` restores the old order) makes `alloc_raw` do the same.
//
// Usage: java -Xmx512m Zr1AlLargeAllocContention [large=4] [small=4] [seconds=10] [kib=1024]
// Prints per-class operation counts and the process CPU time (ThreadMXBean
// sum over the worker threads). The figure of merit is OPS PER CPU SECOND of
// the small-object threads and of the large-array threads, interleaved A/B
// against `CRATONVM_ZGC_RAW_ZERO_OUTSIDE_LOCK=0`; wall time on a loaded host
// is noise, but a lock that parks its waiters costs throughput, not CPU, so
// `*_ops_per_s` is printed too: compare it only within one interleaved run.
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.concurrent.atomic.AtomicLong;

public final class Zr1AlLargeAllocContention {
    static volatile boolean stop;
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        int large = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        int small = args.length > 1 ? Integer.parseInt(args[1]) : 4;
        int seconds = args.length > 2 ? Integer.parseInt(args[2]) : 10;
        int kib = args.length > 3 ? Integer.parseInt(args[3]) : 1024;
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        AtomicLong largeOps = new AtomicLong();
        AtomicLong smallOps = new AtomicLong();
        AtomicLong largeCpu = new AtomicLong();
        AtomicLong smallCpu = new AtomicLong();
        Thread[] ts = new Thread[large + small];
        for (int i = 0; i < ts.length; i++) {
            final boolean isLarge = i < large;
            ts[i] = new Thread(() -> {
                long n = 0;
                long acc = 0;
                while (!stop) {
                    if (isLarge) {
                        long[] a = new long[(kib << 10) / 8];
                        a[a.length - 1] = n;
                        acc += a[0] + a[a.length - 1];
                    } else {
                        for (int k = 0; k < 64; k++) {
                            Object[] o = new Object[4];
                            o[0] = o;
                            acc += o.length;
                        }
                    }
                    n++;
                }
                sink = acc;
                long cpu = mx.getCurrentThreadCpuTime();
                if (isLarge) {
                    largeOps.addAndGet(n);
                    largeCpu.addAndGet(cpu);
                } else {
                    smallOps.addAndGet(n);
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
        double lc = Math.max(1, largeCpu.get()) / 1e9;
        double sc = Math.max(1, smallCpu.get()) / 1e9;
        System.out.printf(
                "Zr1AlLargeAllocContention large=%d small=%d kib=%d large_ops=%d large_cpu_s=%.3f"
                        + " large_ops_per_cpu_s=%.1f large_ops_per_s=%.1f small_ops=%d small_cpu_s=%.3f"
                        + " small_ops_per_cpu_s=%.1f small_ops_per_s=%.1f%n",
                large, small, kib, largeOps.get(), lc, largeOps.get() / lc,
                largeOps.get() / (double) seconds, smallOps.get(), sc, smallOps.get() / sc,
                smallOps.get() / (double) seconds);
    }
}
