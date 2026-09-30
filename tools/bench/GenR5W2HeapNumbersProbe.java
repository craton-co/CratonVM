// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import java.lang.management.MemoryMXBean;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.lang.management.MemoryUsage;
import java.util.ArrayList;
import java.util.List;
import java.util.TreeSet;

/**
 * gen r5w2/obs6 (2026-09-26): the heap numbers a Java program can read --
 * {@code Runtime.totalMemory/freeMemory/maxMemory}, the heap
 * {@code MemoryMXBean} usage and the heap {@code MemoryPoolMXBean}s -- must
 * obey the relations HotSpot Serial ({@code -XX:+UseSerialGC}) guarantees
 * between them. Pages:
 * {@code docs/internal/gc/gengc-r4w3-hunter-runtime-memory-counts-the-copy-reserve-FIXED-20260927.md},
 * {@code docs/internal/gc/gengc-r5w1-oldgen5-tenured-pool-reports-the-reservation-as-committed-FIXED-20260927.md}.
 *
 * <p>Each relation is checked at four points (start, after young churn, with
 * 48 MiB retained, after dropping it), from a STABLE sample: the committed
 * figures are read twice around the pool reads and the sample is retaken if a
 * collection moved them in between. Relations (HotSpot's source in brackets):
 * <ol>
 *   <li>{@code bounds}: {@code 0 <= free <= total <= max}
 *       [{@code Runtime}: {@code unused() <= capacity() <= max_capacity()}];
 *   <li>{@code max-constant}: {@code maxMemory()} never changes;
 *   <li>{@code max-below-xmx}: {@code maxMemory() < -Xmx} -- the copying young
 *       generation's to-space is not usable heap
 *       [{@code DefNewGeneration::max_capacity()} leaves one survivor out];
 *   <li>{@code heap-max-is-maxMemory}: heap {@code MemoryUsage.getMax()} ==
 *       {@code maxMemory()} [both are {@code max_capacity()}];
 *   <li>{@code heap-committed-is-totalMemory}: heap
 *       {@code MemoryUsage.getCommitted()} == {@code totalMemory()} [both are
 *       {@code capacity()}];
 *   <li>{@code pools-committed-sum-to-heap}: the heap pools' {@code committed}
 *       sum to the heap's [{@code jmm_GetMemoryUsage} sums its pools];
 *   <li>{@code pools-max-sum-to-heap}: the heap pools' DEFINED {@code max} sum
 *       to the heap's [eden max + survivor max + tenured max =
 *       {@code max_capacity()}];
 *   <li>{@code pools-used-near-heap}: the pools' {@code used} sum is within
 *       8 MiB of the heap's (the two are read by separate calls, and this
 *       probe allocates between them);
 *   <li>{@code heap-init-is-xms}: heap {@code MemoryUsage.getInit()} == -Xms;
 *   <li>{@code tenured-committed-below-reservation}: at start, with a small
 *       -Xms, the tenured pool's committed size is below its max (it grows on
 *       demand; it is not the reservation);
 *   <li>{@code eden-peak-sampled-at-gc}: after 512 MiB of young churn, the
 *       Eden pool's peak {@code used} is at least a quarter of its committed
 *       size -- the pool is sampled at every collection's start, when eden is
 *       full, not only when {@code getPeakUsage()} is called
 *       [{@code MemoryPool::record_peak_memory_usage} at {@code gc_begin}];
 *   <li>{@code used-grows}: retaining 48 MiB raises the pools' used sum by at
 *       least 40 MiB.
 * </ol>
 * Deterministic stdout on HotSpot Serial (the command's -Xms/-Xmx must match
 * the two arguments):
 * <pre>
 *   heap-pools Eden Space,Survivor Space,Tenured Gen
 *   bounds ok
 *   max-constant ok
 *   max-below-xmx ok
 *   heap-max-is-maxMemory ok
 *   heap-committed-is-totalMemory ok
 *   pools-committed-sum-to-heap ok
 *   pools-max-sum-to-heap ok
 *   pools-used-near-heap ok
 *   heap-init-is-xms ok
 *   tenured-committed-below-reservation ok
 *   eden-peak-sampled-at-gc ok
 *   used-grows ok
 *   PASS
 * </pre>
 * Commands (arguments: -Xmx then -Xms, in MiB):
 * <pre>
 *   java -XX:+UseSerialGC -Xms16m -Xmx256m -cp tools/bench GenR5W2HeapNumbersProbe 256 16
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xms16m -Xmx256m -cp tools/bench GenR5W2HeapNumbersProbe 256 16
 * </pre>
 * A failing line prints the offending numbers; every sample's raw numbers go
 * to stderr (not compared).
 */
public final class GenR5W2HeapNumbersProbe {
    static final long MiB = 1024L * 1024L;
    static final Runtime RT = Runtime.getRuntime();
    static final MemoryMXBean MEM = ManagementFactory.getMemoryMXBean();
    static final List<MemoryPoolMXBean> HEAP_POOLS = new ArrayList<>();

    static volatile Object sink;

    /** First failure detail per relation, or null while it holds. */
    static String bounds, maxConstant, heapMax, heapCommitted, poolsCommitted, poolsMax,
            poolsUsed, heapInit;
    static long max0 = -1;

    static final class Sample {
        long max, total, free;
        long heapInit, heapUsed, heapCommitted, heapMax;
        long poolsUsed, poolsCommitted, poolsDefinedMax;
        long tenuredCommitted = -1, tenuredMax = -1;

        public String toString() {
            return "max=" + max + " total=" + total + " free=" + free
                    + " heap(init=" + heapInit + " used=" + heapUsed + " committed="
                    + heapCommitted + " max=" + heapMax + ") pools(used=" + poolsUsed
                    + " committed=" + poolsCommitted + " definedMax=" + poolsDefinedMax
                    + ") tenured(committed=" + tenuredCommitted + " max=" + tenuredMax + ")";
        }
    }

    static Sample take() {
        Sample s = null;
        for (int attempt = 0; attempt < 64; attempt++) {
            s = new Sample();
            final long total1 = RT.totalMemory();
            final MemoryUsage heap = MEM.getHeapMemoryUsage();
            for (MemoryPoolMXBean p : HEAP_POOLS) {
                final MemoryUsage u = p.getUsage();
                s.poolsUsed += u.getUsed();
                s.poolsCommitted += u.getCommitted();
                if (u.getMax() >= 0) {
                    s.poolsDefinedMax += u.getMax();
                }
                if (p.getName().endsWith("Tenured Gen")) {
                    s.tenuredCommitted = u.getCommitted();
                    s.tenuredMax = u.getMax();
                }
            }
            final MemoryUsage heap2 = MEM.getHeapMemoryUsage();
            s.max = RT.maxMemory();
            s.total = RT.totalMemory();
            s.free = RT.freeMemory();
            s.heapInit = heap.getInit();
            s.heapUsed = heap.getUsed();
            s.heapCommitted = heap.getCommitted();
            s.heapMax = heap.getMax();
            // Stable: no collection resized the heap while the pools were read.
            if (total1 == s.total && heap.getCommitted() == heap2.getCommitted()) {
                break;
            }
        }
        return s;
    }

    static String firstFailure(String prev, boolean holds, String detail) {
        return prev != null || holds ? prev : detail;
    }

    static Sample check(String where, long xmsBytes) {
        final Sample s = take();
        System.err.println("[probe] " + where + ": " + s);
        final String at = where + ": " + s;
        bounds = firstFailure(bounds, 0 <= s.free && s.free <= s.total && s.total <= s.max, at);
        if (max0 < 0) {
            max0 = s.max;
        }
        maxConstant = firstFailure(maxConstant, s.max == max0, at + " max0=" + max0);
        heapMax = firstFailure(heapMax, s.heapMax == s.max, at);
        heapCommitted = firstFailure(heapCommitted, s.heapCommitted == s.total, at);
        poolsCommitted = firstFailure(poolsCommitted, s.poolsCommitted == s.heapCommitted, at);
        poolsMax = firstFailure(poolsMax, s.poolsDefinedMax == s.heapMax, at);
        poolsUsed = firstFailure(poolsUsed, Math.abs(s.poolsUsed - s.heapUsed) <= 8 * MiB, at);
        heapInit = firstFailure(heapInit, s.heapInit == xmsBytes, at + " xms=" + xmsBytes);
        return s;
    }

    static boolean ok = true;

    static void line(String name, String failure) {
        System.out.println(name + (failure == null ? " ok" : " FAILED " + failure));
        ok &= failure == null;
    }

    public static void main(String[] args) {
        final long xmxBytes = (args.length > 0 ? Long.parseLong(args[0]) : 256) * MiB;
        final long xmsBytes = (args.length > 1 ? Long.parseLong(args[1]) : 16) * MiB;
        final TreeSet<String> names = new TreeSet<>();
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() == MemoryType.HEAP && p.isValid()) {
                HEAP_POOLS.add(p);
                names.add(p.getName());
            }
        }
        System.out.println("heap-pools " + String.join(",", names));

        final Sample start = check("start", xmsBytes);
        final boolean tenuredBelow =
                start.tenuredCommitted >= 0 && start.tenuredCommitted < start.tenuredMax;
        final String tenuredWhy = tenuredBelow ? null : "start: " + start;

        // Churn: 512 MiB of 1 KiB temporaries, several young collections on
        // either VM at -Xmx256m.
        for (int i = 0; i < 512; i++) {
            for (int j = 0; j < 1024; j++) {
                sink = new byte[1024];
            }
        }
        sink = null;
        System.gc();
        final Sample churned = check("churned", xmsBytes);
        // Eden peaks at a young collection's start (it is full then); a peak
        // sampled only when queried would read the near-empty eden of now.
        String edenPeakWhy = "no Eden Space pool";
        for (MemoryPoolMXBean p : HEAP_POOLS) {
            if (p.getName().endsWith("Eden Space")) {
                final long peak = p.getPeakUsage().getUsed();
                final long committed = p.getUsage().getCommitted();
                System.err.println("[probe] eden peak_used=" + peak + " committed=" + committed);
                edenPeakWhy = peak >= committed / 4 ? null
                        : "peak_used=" + peak + " committed=" + committed;
            }
        }

        final List<byte[]> keep = new ArrayList<>();
        for (int i = 0; i < 48; i++) {
            keep.add(new byte[(int) MiB]);
        }
        System.gc();
        final Sample grown = check("retained", xmsBytes);
        final boolean grows = grown.poolsUsed - churned.poolsUsed >= 40 * MiB;
        keep.clear();
        System.gc();
        check("dropped", xmsBytes);

        line("bounds", bounds);
        line("max-constant", maxConstant);
        line("max-below-xmx", max0 < xmxBytes ? null : "max=" + max0 + " xmx=" + xmxBytes);
        line("heap-max-is-maxMemory", heapMax);
        line("heap-committed-is-totalMemory", heapCommitted);
        line("pools-committed-sum-to-heap", poolsCommitted);
        line("pools-max-sum-to-heap", poolsMax);
        line("pools-used-near-heap", poolsUsed);
        line("heap-init-is-xms", heapInit);
        line("tenured-committed-below-reservation", tenuredWhy);
        line("eden-peak-sampled-at-gc", edenPeakWhy);
        line("used-grows", grows ? null
                : "churned=" + churned.poolsUsed + " retained=" + grown.poolsUsed);
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
