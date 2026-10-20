// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zs s2/ca2 (2026-10-14): a compiled small allocation's MUTATOR cost per
// part, measured so the reading survives a loaded host.
//
// Why another probe: zs s1/ca's `Zs1CaAllocParts` times one shape per
// process for N seconds and reports the mean. On the loaded build host its
// `ns_per_op_cpu` ranged 7.1-11.3 across rounds of the SAME binary, which
// swamps any sub-nanosecond step (s1's three emitter cuts were invisible).
// Three things make that number noisy, and this probe removes each:
//
//   1. Collections: a stop-the-world cycle runs on the allocating thread and
//      lands in its CPU time. Here every window records the collection count
//      before and after, and a window that saw a collection is DISCARDED.
//      Run it with a large heap (-Xmx2g or more) so most windows are clean.
//   2. Load from other processes: it only ever ADDS time to a window (cache
//      and SMT interference, frequency drops), so the per-shape MINIMUM over
//      many short windows estimates the unloaded cost far better than a mean.
//   3. Drift between processes: every shape runs in the SAME process, in
//      round-robin windows, so `fixed0 - store0` is a within-process
//      difference whose two terms saw the same host conditions.
//
// Shapes (one 64-iteration loop per call into a 1024-slot `Object[]` ring,
// index `(n + k) & 1023`, no division -- the same loop for every shape):
//
//   store0   a preallocated `long[0]`: the floor (loop, aastore, barrier, poll)
//   fixed0   `new long[0]`: + the bare allocation sequence (16 bytes)
//   fixed8   `new long[8]`: + 64 zeroed bytes
//   byte16   `new byte[16]`: the `new StringBuilder()` backing array
//   obj      `new Object()`: the `new` arm of the same sequence
//   varlen   `new long[(n + k) & 63]`: a run-time length (mean 272 bytes)
//
// stdout: one deterministic CHECK line (must equal HotSpot's). stderr: one
// `window:` line per shape (min / median / kept windows) and one `parts:`
// line with the within-process differences:
//
//   alloc_fixed0 = min(fixed0) - min(store0)   the inline sequence's cost
//   alloc_obj    = min(obj)    - min(store0)
//   bytes64      = min(fixed8) - min(fixed0)   64 zeroed bytes
//   varlen_extra = min(varlen) - min(fixed8)   run-time length + 192 bytes
//
// For a value-identical emitter change, compare `alloc_fixed0` (and
// `alloc_obj`) between an ON and an OFF process, interleaved.
//
// Usage: java -Xmx2g Zs2Ca2AllocWindows [seconds=10] [bursts_per_window=2000]
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.Arrays;
import java.util.List;
import java.util.Locale;

public final class Zs2Ca2AllocWindows {
    static final long[] PRE = new long[0];
    static final String[] SHAPES = {"store0", "fixed0", "fixed8", "byte16", "obj", "varlen"};

    static void burstStore0(Object[] ring, long n) {
        long[] p = PRE;
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = p;
        }
    }

    static void burstFixed0(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new long[0];
        }
    }

    static void burstFixed8(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new long[8];
        }
    }

    static void burstByte16(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new byte[16];
        }
    }

    static void burstObj(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new Object();
        }
    }

    static void burstVarlen(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new long[(int) ((n + k) & 63)];
        }
    }

    static void burst(int code, Object[] ring, long n) {
        switch (code) {
            case 0 -> burstStore0(ring, n);
            case 1 -> burstFixed0(ring, n);
            case 2 -> burstFixed8(ring, n);
            case 3 -> burstByte16(ring, n);
            case 4 -> burstObj(ring, n);
            default -> burstVarlen(ring, n);
        }
    }

    /// Runs `bursts` calls of one shape; returns the advanced `n`. A separate
    /// method per window so the timed loop is the same compiled code each time.
    static long window(int code, Object[] ring, long n, int bursts) {
        for (int r = 0; r < bursts; r++) {
            burst(code, ring, n);
            n += 64;
        }
        return n;
    }

    /// Number of failed checks for one burst of `code` at `n`.
    static long checkBurst(int code, Object[] ring, long n) {
        burst(code, ring, n);
        long bad = 0;
        for (int k = 0; k < 64; k++) {
            Object o = ring[(int) ((n + k) & 1023)];
            switch (code) {
                case 3 -> {
                    if (!(o instanceof byte[] b) || b.length != 16) {
                        bad++;
                    } else {
                        for (byte v : b) {
                            if (v != 0) {
                                bad++;
                            }
                        }
                    }
                }
                case 4 -> {
                    if (o == null || o.getClass() != Object.class) {
                        bad++;
                    }
                }
                default -> {
                    if (!(o instanceof long[] a)) {
                        bad++;
                        continue;
                    }
                    int want = switch (code) {
                        case 0, 1 -> 0;
                        case 2 -> 8;
                        default -> (int) ((n + k) & 63);
                    };
                    if (a.length != want) {
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

    static double median(double[] v, int n) {
        double[] s = Arrays.copyOf(v, n);
        Arrays.sort(s);
        return n == 0 ? Double.NaN : (n % 2 == 1 ? s[n / 2] : (s[n / 2 - 1] + s[n / 2]) / 2);
    }

    public static void main(String[] args) {
        int seconds = args.length > 0 ? Integer.parseInt(args[0]) : 10;
        int bursts = args.length > 1 ? Integer.parseInt(args[1]) : 2000;
        Object[] ring = new Object[1024];

        // Phase 1: CHECK every shape (interpreted first, compiled later).
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
        System.out.println("Zs2Ca2AllocWindows check shapes=" + SHAPES.length + " ops=" + checks
                + " bad=" + bad + " ok=" + (bad == 0));

        // Phase 2: warm every kernel and the window driver.
        for (int r = 0; r < 40; r++) {
            for (int code = 0; code < SHAPES.length; code++) {
                n = window(code, ring, n, 500);
            }
        }

        // Phase 3: calibrate, per shape. The thread-CPU clock's step (about
        // 1 us or finer on Linux, ~15.6 ms on Windows) must be small against a
        // window, so each shape's window is doubled until it costs at least
        // max(2 ms, 50 steps) of CPU. `bursts` is the starting size.
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        List<GarbageCollectorMXBean> beans = ManagementFactory.getGarbageCollectorMXBeans();
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
        System.err.printf(Locale.ROOT, "calib: cpu_clock_step_ns=%d window_cpu_target_ns=%d bursts=%s%n",
                step, want, Arrays.toString(perShape));

        // Phase 4: round-robin windows; a window that saw a collection is dropped.
        int cap = 100_000;
        double[][] ns = new double[SHAPES.length][cap];
        int[] kept = new int[SHAPES.length];
        int[] dropped = new int[SHAPES.length];
        long deadline = System.nanoTime() + seconds * 1_000_000_000L;
        while (System.nanoTime() < deadline) {
            for (int code = 0; code < SHAPES.length; code++) {
                long g0 = gcCount(beans);
                long c0 = mx.getCurrentThreadCpuTime();
                n = window(code, ring, n, perShape[code]);
                long c1 = mx.getCurrentThreadCpuTime();
                long g1 = gcCount(beans);
                if (g1 != g0) {
                    dropped[code]++;
                } else if (kept[code] < cap) {
                    ns[code][kept[code]++] = (double) (c1 - c0) / (64.0 * perShape[code]);
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
                    "window: shape=%s min_ns_per_op=%.3f median_ns_per_op=%.3f kept=%d dropped_gc=%d ops_per_window=%d%n",
                    SHAPES[code], m, median(ns[code], kept[code]), kept[code], dropped[code],
                    64L * perShape[code]);
        }
        System.err.printf(Locale.ROOT,
                "parts: alloc_fixed0=%.3f alloc_obj=%.3f alloc_byte16=%.3f bytes64=%.3f varlen_extra=%.3f floor_store0=%.3f%n",
                min[1] - min[0], min[4] - min[0], min[3] - min[0], min[2] - min[1], min[5] - min[2],
                min[0]);
        // Phase 5 (zt t6/tc, zt2ca item 7): re-check every shape AFTER the
        // timed windows, when `burst*` are the compiled kernels the windows
        // measured. Phase 1 ran before warm-up, so its line alone said nothing
        // about the compiled code ("warm in drive, assert in main").
        long rebad = 0;
        int rechecks = 0;
        for (int r = 0; r < 2000; r++) {
            for (int code = 0; code < SHAPES.length; code++) {
                rebad += checkBurst(code, ring, n);
                n += 64;
                rechecks += 64;
            }
        }
        System.out.println("Zs2Ca2AllocWindows recheck shapes=" + SHAPES.length + " ops=" + rechecks
                + " bad=" + rebad + " ok=" + (rebad == 0));
        if (ring[(int) (n & 1023)] == null && ring[0] == null) {
            System.err.println("ring empty");
        }
    }
}
