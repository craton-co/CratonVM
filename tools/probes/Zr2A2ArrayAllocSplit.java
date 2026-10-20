// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z2/a2 (2026-10-12): the decisive split of `Rbc6ArrayAllocProbe`, which
// times out at 180 s on CratonVM in both modes while HotSpot prints its five
// lines.
//
// Rbc6's default n = 200000 is not a small workload. Summed over
// i = -1000..n, its five methods ask for:
//
//   newarrayPath           int[i]            ~ 2.0e10 elements, ~80 GB
//   anewarrayPath          Present[i]        ~ 2.0e10 references
//   missingComponent       Gone[|i|+1]       a NoClassDefFoundError per call when
//                                            Gone.class is absent, else ~2.0e10 refs
//   multianewarrayOuter    int[i][3]         ~ 2.0e10 int[3] SUB-ARRAYS (one
//                                            allocation each) + the outer arrays
//   multianewarrayInner    int[2][i]         ~ 4.0e10 elements
//
// so the question "hang or crawl" is answered by running each method ALONE,
// at a smaller n, on both VMs, and comparing CPU per phase -- and by running
// two n values: every row is quadratic in n, so doubling n must cost ~4x. A
// phase that costs much more than 4x per doubling is super-linear (a real
// defect, e.g. GC cost growing with the cycle count); a phase that is a
// constant factor slower than HotSpot is a throughput gap.
//
// Usage: java Zr2A2ArrayAllocSplit [n=20000] [phases=all|name,name,...]
//
// Output: one `phase=` line per phase with its checksum (deterministic; equals
// HotSpot's, and Rbc6's own line when n matches), then one `zr2a2-cpu` line
// per phase with the CPU and wall time (ThreadMXBean, this thread) and the
// phase's allocation count. `--compatible` and `--jdk-only` must print the
// same `phase=` lines.
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;

public class Zr2A2ArrayAllocSplit {
    static final class Present {}

    static int newarrayPath(int a) {
        int x = a * 3;
        try {
            x += 1;
            int[] arr = new int[a];
            x += arr.length;
        } catch (NegativeArraySizeException e) {
            return x + e.getMessage().length();
        }
        return x;
    }

    static int anewarrayPath(int a) {
        int x = a + 7;
        try {
            x += 1;
            Present[] arr = new Present[a];
            x += arr.length;
        } catch (NegativeArraySizeException e) {
            return x * 2 + e.getMessage().length();
        }
        return x;
    }

    static int missingComponent(int a) {
        int x = a * 11;
        try {
            x += 5;
            Gone[] arr = new Gone[Math.abs(a) + 1];
            x += arr.length;
        } catch (NoClassDefFoundError e) {
            return x;
        }
        return -1;
    }

    static int multianewarrayOuterPath(int a) {
        int x = a - 2;
        try {
            x += 1;
            int[][] arr = new int[a][3];
            x += arr.length;
        } catch (NegativeArraySizeException e) {
            return x + e.getMessage().length();
        }
        return x;
    }

    static int multianewarrayInnerPath(int a) {
        int x = a - 5;
        try {
            x += 1;
            int[][] arr = new int[2][a];
            x += arr.length;
        } catch (NegativeArraySizeException e) {
            return x + e.getMessage().length();
        }
        return x;
    }

    static long run(String phase, int n) {
        long s = 0;
        for (int i = -1000; i < n; i++) {
            switch (phase) {
                case "newarrayPath" -> s += newarrayPath(i);
                case "anewarrayPath" -> s += anewarrayPath(i);
                case "missingComponent" -> s += missingComponent(i);
                case "multianewarrayOuter" -> s += multianewarrayOuterPath(i);
                case "multianewarrayInner" -> s += multianewarrayInnerPath(i);
                default -> throw new IllegalArgumentException(phase);
            }
        }
        return s;
    }

    /** Allocations a phase makes (arrays, outer and inner), for the per-allocation figure. */
    static long allocations(String phase, int n) {
        long pos = Math.max(0, n);
        long tri = pos * (pos - 1) / 2; // sum of i over 0..n-1
        return switch (phase) {
            case "multianewarrayOuter" -> pos + tri; // the outer arrays, then i int[3] each
            case "multianewarrayInner" -> 3 * pos;   // outer int[2][] + two int[i]
            case "missingComponent" -> (long) n + 1000; // one attempt per call
            default -> pos;
        };
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20_000;
        String[] all = {
            "newarrayPath", "anewarrayPath", "missingComponent",
            "multianewarrayOuter", "multianewarrayInner"
        };
        String[] phases = args.length > 1 && !args[1].equals("all") ? args[1].split(",") : all;
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        StringBuilder timing = new StringBuilder();
        for (String phase : phases) {
            long c0 = mx.getCurrentThreadCpuTime();
            long w0 = System.nanoTime();
            long sum = run(phase, n);
            long cpu = mx.getCurrentThreadCpuTime() - c0;
            long wall = System.nanoTime() - w0;
            System.out.println("phase=" + phase + " n=" + n + " sum=" + sum);
            long allocs = allocations(phase, n);
            timing.append(String.format(java.util.Locale.ROOT,
                    "zr2a2-cpu phase=%s n=%d cpu_ms=%d wall_ms=%d allocs=%d ns_per_alloc=%.1f%n",
                    phase, n, cpu / 1_000_000, wall / 1_000_000, allocs,
                    allocs == 0 ? 0.0 : cpu / (double) allocs));
        }
        System.out.print(timing);
    }
}
