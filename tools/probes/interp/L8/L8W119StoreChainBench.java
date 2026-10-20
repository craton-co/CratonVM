// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i6 wave 119, lane L8: CRATONVM_INTERP_FUSED_STORE_CHAIN
// (default off). A served `aload_N; {aload_M | iload_M}; putfield` pair runs
// the same pairs right after it in the same dispatch: a constructor's or a
// setter's run of `this.x = x` stores (the wave-88 census's `putfield ->
// aload_0`, in 116 of 169 bench probes).
//
// Rows:
//   ctor3      new P3(i, i + 1, tag): three stores in <init>     CHAIN
//   setters4   t.set(i, tag, -i): four stores of `this`          CHAIN
//   ctor6      new P6(i, i, i, tag, tag, tag): three stores from
//              iload_1..3, then three from aload 4..6 (wave 120)    CHAIN (+ WIDE)
//   single     t.one(i): one store (nothing to chain)            flat
//   control    s += i                                            flat
//
// Every row runs REPS timed repetitions after one untimed warm-up and
// reports the MINIMUM. Compare arms on the SAME binary, interleaved, medians
// of the per-round minima. On the Windows dev box pin to the P-cores
// (logical CPUs 0-15), or an E-core run reads 1.8-2.8x slower
// (docs/known-issues/interpreter/gap-hybrid-cpu-e-core-scheduling-halves-interpreter-timings-20261018.md):
//   start /affinity FFFF /b /wait cratonvm --java-home <jdk25> --nojit -cp <dir> L8W119StoreChainBench
//   CRATONVM_INTERP_FUSED_STORE_CHAIN=1 (same)
//   CRATONVM_INTERP_FUSED_STORE_CHAIN=1 CRATONVM_INTERP_FUSED_STORE_CHAIN_WIDE=1 (same;
//   wave 120: `ctor6` should drop further, the other rows stay where the
//   chain alone puts them)
// An optional argument scales every row (`L8W119StoreChainBench 100` for a
// JIT run); the checksums below are for scale 1.
// stdout: per row one deterministic `<row> checksum=<n>` line and one
// `TIME <row> <x> ns/op <y> us` line (not compared). About 2 s under --nojit.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// ctor3 checksum=20000100000
// ctor6 checksum=19999900000
// setters4 checksum=19999900000
// single checksum=19999900000
// control checksum=19999900000
// HOTSPOT_EXPECTED_END
public class L8W119StoreChainBench {
    interface Row {
        long run();
    }

    static final int REPS = 5;
    static final int ITERS = 200_000;
    static int scale = 1;
    static int loops = ITERS;

    static final class P3 {
        int a;
        int b;
        Object c;

        P3(int a, int b, Object c) {
            this.a = a;
            this.b = b;
            this.c = c;
        }
    }

    static final class P6 {
        int a;
        int b;
        int c;
        Object d;
        Object e;
        Object f;

        P6(int a, int b, int c, Object d, Object e, Object f) {
            this.a = a;
            this.b = b;
            this.c = c;
            this.d = d;
            this.e = e;
            this.f = f;
        }
    }

    int f1;
    int f2;
    Object f3;
    Object f4;

    void set(int x, Object o, int y) {
        this.f1 = x;
        this.f3 = o;
        this.f2 = y;
        this.f4 = o;
    }

    void one(int x) {
        this.f1 = x;
    }

    static long ctor3() {
        long s = 0;
        Object tag = "t";
        int n = loops;
        for (int i = 0; i < n; i++) {
            P3 p = new P3(i, i + 1, tag);
            s += p.b;
        }
        return s;
    }

    static long ctor6() {
        long s = 0;
        Object tag = "t";
        int n = loops;
        for (int i = 0; i < n; i++) {
            P6 p = new P6(i, i, i, tag, tag, tag);
            s += p.c;
        }
        return s;
    }

    static long setters4() {
        L8W119StoreChainBench t = new L8W119StoreChainBench();
        Object tag = "t";
        long s = 0;
        int n = loops;
        for (int i = 0; i < n; i++) {
            t.set(i, tag, -i);
            s += t.f1;
        }
        return s;
    }

    static long single() {
        L8W119StoreChainBench t = new L8W119StoreChainBench();
        long s = 0;
        int n = loops;
        for (int i = 0; i < n; i++) {
            t.one(i);
            s += t.f1;
        }
        return s;
    }

    static long control() {
        long s = 0;
        int n = loops;
        for (int i = 0; i < n; i++) {
            s += i;
        }
        return s;
    }

    static void time(String name, Row row) {
        long checksum = row.run(); // untimed warm-up
        long best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            long c = row.run();
            long dt = System.nanoTime() - t0;
            if (dt < best) {
                best = dt;
            }
            if (c != checksum) {
                checksum = -1;
            }
        }
        System.out.println(name + " checksum=" + checksum);
        System.out.printf(java.util.Locale.ROOT, "TIME %-9s %8.2f ns/op %10.1f us (min of %d, scale %d)%n",
                name, (double) best / loops, best / 1000.0, REPS, scale);
    }

    public static void main(String[] args) {
        if (args.length > 0) {
            scale = Integer.parseInt(args[0]);
            loops = ITERS * scale;
        }
        time("ctor3", L8W119StoreChainBench::ctor3);
        time("ctor6", L8W119StoreChainBench::ctor6);
        time("setters4", L8W119StoreChainBench::setters4);
        time("single", L8W119StoreChainBench::single);
        time("control", L8W119StoreChainBench::control);
    }
}
