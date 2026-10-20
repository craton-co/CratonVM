// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z7/ca (2026-10-13): where the compiled small-array allocation's time goes.
//
// `Zr3JaRefArrayAlloc 1 10 64 prim 400` costs ~54 ns per allocation on ZGC
// against HotSpot 25's ~10 ns; perf put ~46 % of it in the compiled burst loop
// and ~25 % in the collector (sweep, memset, collect_garbage). The burst loop
// does more than allocate: a 64-bit `%` (idiv), the ring index, a reference
// store into `Object[]` with its covariance check and store barrier, and the
// safepoint poll. This probe prices the parts one at a time, with the SAME
// loop shape (64 iterations per call, a 1024-slot ring), so a row against its
// HotSpot twin says which part is the gap:
//
//   loop    no allocation: the ring gets a preallocated array of the same
//           length (the idiv, the aastore, the barrier, the loop). Baseline.
//   prim    `new long[(n + k) % mod]` -- Zr3JaRefArrayAlloc's `prim` shape.
//   fixed8  `new long[8]`: no idiv, constant size (header + 64 bytes).
//   fixed0  `new long[0]`: a bare header, i.e. the allocation path alone
//           (bump, header stores, ZGC start-bit announce) plus the store.
//   obj     `new Object()`: the `new` arm of the same inline sequence.
//
// (prim - loop) is the allocation's own cost on a variable-length array;
// fixed0 against HotSpot's fixed0 is the inline sequence's instruction gap;
// fixed8 - fixed0 is the zeroing and footprint; GC cost is in all but `loop`.
//
// A deterministic CHECK line first (must equal HotSpot's): the shapes' arrays
// have the right length, are all zero, and have the right class, checked in
// the compiled kernel itself.
//
// Usage: java -Xmx512m Zr7CaAllocShapes [shape=prim] [seconds=5] [maxlen=64] [rounds=200]
// Figure of merit: ns_per_op_cpu on the `timing:` line (the mutator thread's
// CPU from ThreadMXBean; collector worker threads are not in it -- read
// /usr/bin/time user+sys for the process total).
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;

public final class Zr7CaAllocShapes {
    static volatile boolean stop;
    static volatile Object sink;
    static long[][] pool;

    static void burstLoop(Object[] ring, long n, int mod) {
        long[][] p = pool;
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = p[(int) ((n + k) % mod)];
        }
    }

    static void burstPrim(Object[] ring, long n, int mod) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new long[(int) ((n + k) % mod)];
        }
    }

    static void burstFixed8(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new long[8];
        }
    }

    static void burstFixed0(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new long[0];
        }
    }

    static void burstObj(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new Object();
        }
    }

    static void burst(int code, Object[] ring, long n, int mod) {
        switch (code) {
            case 0 -> burstLoop(ring, n, mod);
            case 1 -> burstPrim(ring, n, mod);
            case 2 -> burstFixed8(ring, n);
            case 3 -> burstFixed0(ring, n);
            default -> burstObj(ring, n);
        }
    }

    /// One CHECK round over a fresh ring; returns the number of failed checks.
    static long checkRound(int code, Object[] ring, long n, int mod) {
        burst(code, ring, n, mod);
        long bad = 0;
        for (int k = 0; k < 64; k++) {
            Object o = ring[(int) ((n + k) & 1023)];
            if (code == 4) {
                if (o == null || o.getClass() != Object.class) {
                    bad++;
                }
                continue;
            }
            if (!(o instanceof long[] a)) {
                bad++;
                continue;
            }
            int want = switch (code) {
                case 0, 1 -> (int) ((n + k) % mod);
                case 2 -> 8;
                default -> 0;
            };
            if (a.length != want) {
                bad++;
            }
            for (long v : a) {
                if (v != 0) {
                    bad++;
                }
            }
        }
        return bad;
    }

    public static void main(String[] args) throws Exception {
        String shape = args.length > 0 ? args[0] : "prim";
        int seconds = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        int maxlen = args.length > 2 ? Integer.parseInt(args[2]) : 64;
        int rounds = args.length > 3 ? Integer.parseInt(args[3]) : 200;
        final int code = switch (shape) {
            case "loop" -> 0;
            case "prim" -> 1;
            case "fixed8" -> 2;
            case "fixed0" -> 3;
            case "obj" -> 4;
            default -> throw new IllegalArgumentException(
                    "shape must be loop, prim, fixed8, fixed0 or obj: " + shape);
        };
        final int mod = maxlen + 1;
        pool = new long[mod][];
        for (int i = 0; i < mod; i++) {
            pool[i] = new long[i];
        }

        // Phase 1: CHECK.
        Object[] ring = new Object[1024];
        long bad = 0;
        long n = 0;
        for (int r = 0; r < rounds; r++) {
            bad += checkRound(code, ring, n, mod);
            n += 64;
        }
        System.out.println("Zr7CaAllocShapes check shape=" + shape + " rounds=" + rounds
                + " ops=" + n + " bad=" + bad + " ok=" + (bad == 0));

        // Phase 2: TIMED, on one thread.
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        long gcCount0 = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            gcCount0 += Math.max(0, gc.getCollectionCount());
        }
        final long[] out = new long[2];
        Thread t = new Thread(() -> {
            Object[] r = new Object[1024];
            long m = 0;
            long cpu0 = mx.getCurrentThreadCpuTime();
            while (!stop) {
                burst(code, r, m, mod);
                m += 64;
            }
            out[1] = mx.getCurrentThreadCpuTime() - cpu0;
            out[0] = m;
            sink = r;
        });
        t.start();
        Thread.sleep(seconds * 1000L);
        stop = true;
        t.join();
        long gcCount = -gcCount0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            gcCount += Math.max(0, gc.getCollectionCount());
        }
        double cs = Math.max(1, out[1]) / 1e9;
        System.out.printf(java.util.Locale.ROOT,
                "timing: Zr7CaAllocShapes shape=%s maxlen=%d ops=%d cpu_s=%.3f ns_per_op_cpu=%.2f"
                        + " gc_count=%d%n",
                shape, maxlen, out[0], cs, out[0] == 0 ? 0.0 : out[1] / (double) out[0], gcCount);
    }
}
