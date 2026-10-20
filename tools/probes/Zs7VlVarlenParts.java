// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zs s7/vl (2026-10-15): splits `Zs2Ca2AllocWindows`' `varlen` gap into its
// three possible terms, at EQUAL mean bytes, in one process.
//
// `zgc-zs6ca3-varlen-allocation-gap-is-outside-the-inline-sequence-CLOSED-20261019.md`
// priced `varlen` (`new long[(n + k) & 63]`, mean 268 bytes) against `fixed8`
// (80 bytes) with the fixed0 -> fixed8 slope (0.61 ns per 64 bytes) and found
// 18 ns unexplained. That slope was taken where the loop is instruction-bound
// (ca2: ~85 instructions, 4.3 ns), so it says nothing about the per-byte cost
// above that floor. This probe compares shapes of the SAME size instead:
//
//   store0    the floor: a preallocated `long[0]`, as Zs2Ca2AllocWindows
//   fixed8    `new long[8]`            80 bytes (the page's reference)
//   fixed32   `new long[32]`          272 bytes, constant length
//   rt32      `new long[LEN32]`       272 bytes, run-time length (a static
//                                      field), one size
//   varlen    `new long[(n+k) & 63]`  268 bytes mean, 64 sizes (the page's)
//   fixed63   `new long[63]`          520 bytes, constant length
//
// stderr `parts:` line (all within-process differences of window minima):
//
//   size_variety = min(varlen) - min(rt32)     64 sizes vs one, same bytes
//   rt_length    = min(rt32) - min(fixed32)    run-time length, same size
//   bytes_192    = min(fixed32) - min(fixed8)  192 more bytes, same shape
//   per_byte     = (min(fixed63) - min(fixed32)) / 248
//
// Reading: if `size_variety` and `rt_length` are near zero and `bytes_192`
// carries the gap, `varlen` is a per-byte (memory) cost, and the HotSpot twin
// decides whether it is ours. If `size_variety` carries it, the variable
// sizes themselves cost (start-bit words, refill tails, the owned window).
//
// Every window records the collection count and the thread CPU clock; a
// window that saw a collection is dropped (as Zs2Ca2AllocWindows). It also
// records the PROCESS CPU clock when the platform has one
// (`com.sun.management.OperatingSystemMXBean`): `other_cpu` on the `window:`
// line is process minus thread CPU per op, i.e. concurrent GC / compiler /
// other-thread work that ran during the window (candidate 2 of the page).
//
// stdout: one deterministic CHECK line (must equal HotSpot's).
//
// Usage: java -Xmx4g Zs7VlVarlenParts [seconds=10] [bursts_per_window=2000]
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.Arrays;
import java.util.List;
import java.util.Locale;

public final class Zs7VlVarlenParts {
    static final long[] PRE = new long[0];
    static int LEN32 = 32;
    static final String[] SHAPES = {"store0", "fixed8", "fixed32", "rt32", "varlen", "fixed63"};

    static void burstStore0(Object[] ring, long n) {
        long[] p = PRE;
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = p;
        }
    }

    static void burstFixed8(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new long[8];
        }
    }

    static void burstFixed32(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new long[32];
        }
    }

    static void burstRt32(Object[] ring, long n) {
        int len = LEN32;
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new long[len];
        }
    }

    static void burstVarlen(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new long[(int) ((n + k) & 63)];
        }
    }

    static void burstFixed63(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new long[63];
        }
    }

    static void burst(int code, Object[] ring, long n) {
        switch (code) {
            case 0 -> burstStore0(ring, n);
            case 1 -> burstFixed8(ring, n);
            case 2 -> burstFixed32(ring, n);
            case 3 -> burstRt32(ring, n);
            case 4 -> burstVarlen(ring, n);
            default -> burstFixed63(ring, n);
        }
    }

    static int wantLength(int code, long n, int k) {
        return switch (code) {
            case 0 -> 0;
            case 1 -> 8;
            case 2, 3 -> 32;
            case 4 -> (int) ((n + k) & 63);
            default -> 63;
        };
    }

    static long window(int code, Object[] ring, long n, int bursts) {
        for (int r = 0; r < bursts; r++) {
            burst(code, ring, n);
            n += 64;
        }
        return n;
    }

    static long checkBurst(int code, Object[] ring, long n) {
        burst(code, ring, n);
        long bad = 0;
        for (int k = 0; k < 64; k++) {
            Object o = ring[(int) ((n + k) & 1023)];
            if (!(o instanceof long[] a)) {
                bad++;
                continue;
            }
            if (a.length != wantLength(code, n, k)) {
                bad++;
            }
            for (long v : a) {
                if (v != 0) {
                    bad++;
                }
            }
            if (code == 0 && a != PRE) {
                bad++;
            }
        }
        return bad;
    }

    static long gcCount(List<GarbageCollectorMXBean> beans) {
        long c = 0;
        for (GarbageCollectorMXBean b : beans) {
            c += Math.max(0, b.getCollectionCount());
        }
        return c;
    }

    /// Process CPU in ns, or -1 when the platform bean has no such clock.
    static long processCpu(Object os) {
        if (os == null) {
            return -1;
        }
        try {
            return ((com.sun.management.OperatingSystemMXBean) os).getProcessCpuTime();
        } catch (Throwable t) {
            return -1;
        }
    }

    static Object processCpuBean() {
        try {
            Object os = ManagementFactory.getOperatingSystemMXBean();
            if (os instanceof com.sun.management.OperatingSystemMXBean b && b.getProcessCpuTime() >= 0) {
                return os;
            }
        } catch (Throwable t) {
            // No process clock: `other_cpu` prints NaN.
        }
        return null;
    }

    static double median(double[] v, int n) {
        double[] s = Arrays.copyOf(v, n);
        Arrays.sort(s);
        return n == 0 ? Double.NaN : (n % 2 == 1 ? s[n / 2] : (s[n / 2 - 1] + s[n / 2]) / 2);
    }

    public static void main(String[] args) {
        int seconds = args.length > 0 ? Integer.parseInt(args[0]) : 10;
        int bursts = args.length > 1 ? Integer.parseInt(args[1]) : 2000;
        Object[] ring = new Object[1024];

        long bad = 0;
        long n = 0;
        int checks = 0;
        for (int r = 0; r < 200; r++) {
            for (int code = 0; code < SHAPES.length; code++) {
                bad += checkBurst(code, ring, n);
                n += 64;
                checks += 64;
            }
        }
        System.out.println("Zs7VlVarlenParts check shapes=" + SHAPES.length + " ops=" + checks
                + " bad=" + bad + " ok=" + (bad == 0));

        for (int r = 0; r < 40; r++) {
            for (int code = 0; code < SHAPES.length; code++) {
                n = window(code, ring, n, 500);
            }
        }

        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        List<GarbageCollectorMXBean> beans = ManagementFactory.getGarbageCollectorMXBeans();
        Object os = processCpuBean();
        long step = Long.MAX_VALUE;
        for (int i = 0; i < 5; i++) {
            long a = mx.getCurrentThreadCpuTime();
            long b;
            do {
                b = mx.getCurrentThreadCpuTime();
            } while (b == a);
            step = Math.min(step, b - a);
        }
        long want = Math.max(2_000_000L, 50 * step);
        int[] perShape = new int[SHAPES.length];
        for (int code = 0; code < SHAPES.length; code++) {
            int w = bursts;
            for (int i = 0; i < 30; i++) {
                long c0 = mx.getCurrentThreadCpuTime();
                n = window(code, ring, n, w);
                long spent = mx.getCurrentThreadCpuTime() - c0;
                if (spent >= want || w >= (1 << 22)) {
                    break;
                }
                w *= 2;
            }
            perShape[code] = w;
        }
        System.err.printf(Locale.ROOT, "calib: cpu_clock_step_ns=%d window_cpu_target_ns=%d process_clock=%b bursts=%s%n",
                step, want, os != null, Arrays.toString(perShape));

        int cap = 100_000;
        double[][] ns = new double[SHAPES.length][cap];
        double[][] other = new double[SHAPES.length][cap];
        int[] kept = new int[SHAPES.length];
        int[] dropped = new int[SHAPES.length];
        long deadline = System.nanoTime() + seconds * 1_000_000_000L;
        while (System.nanoTime() < deadline) {
            for (int code = 0; code < SHAPES.length; code++) {
                long g0 = gcCount(beans);
                long p0 = processCpu(os);
                long c0 = mx.getCurrentThreadCpuTime();
                n = window(code, ring, n, perShape[code]);
                long c1 = mx.getCurrentThreadCpuTime();
                long p1 = processCpu(os);
                long g1 = gcCount(beans);
                if (g1 != g0) {
                    dropped[code]++;
                } else if (kept[code] < cap) {
                    double ops = 64.0 * perShape[code];
                    ns[code][kept[code]] = (c1 - c0) / ops;
                    other[code][kept[code]] = (p0 < 0 || p1 < 0)
                            ? Double.NaN
                            : ((p1 - p0) - (c1 - c0)) / ops;
                    kept[code]++;
                }
            }
        }
        double[] min = new double[SHAPES.length];
        for (int code = 0; code < SHAPES.length; code++) {
            double m = Double.NaN;
            for (int i = 0; i < kept[code]; i++) {
                m = Double.isNaN(m) ? ns[code][i] : Math.min(m, ns[code][i]);
            }
            min[code] = m;
            System.err.printf(Locale.ROOT,
                    "window: shape=%s min_ns_per_op=%.3f median_ns_per_op=%.3f other_cpu_median_ns_per_op=%.3f kept=%d dropped_gc=%d ops_per_window=%d%n",
                    SHAPES[code], m, median(ns[code], kept[code]), median(other[code], kept[code]),
                    kept[code], dropped[code], 64L * perShape[code]);
        }
        System.err.printf(Locale.ROOT,
                "parts: size_variety=%.3f rt_length=%.3f bytes_192=%.3f per_byte=%.4f varlen_over_fixed8=%.3f floor_store0=%.3f%n",
                min[4] - min[3], min[3] - min[2], min[2] - min[1], (min[5] - min[2]) / 248.0,
                min[4] - min[1], min[0]);
        // zt t6/tc (zt2ca item 7): re-check every shape AFTER the timed
        // windows, against the compiled kernels they measured (the first
        // check line runs before warm-up).
        long rebad = 0;
        int rechecks = 0;
        for (int r = 0; r < 2000; r++) {
            for (int code = 0; code < SHAPES.length; code++) {
                rebad += checkBurst(code, ring, n);
                n += 64;
                rechecks += 64;
            }
        }
        System.out.println("Zs7VlVarlenParts recheck shapes=" + SHAPES.length + " ops=" + rechecks
                + " bad=" + rebad + " ok=" + (rebad == 0));
        if (ring[(int) (n & 1023)] == null && ring[0] == null) {
            System.err.println("ring empty");
        }
    }
}
