// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i5 wave 102, lane L7: CRATONVM_INTERP_FUSED_DOUBLE_ARITH
// (default off), the `dload_0..3` arm's `dload; {dload_Y | dload n | dconst_0
// | dconst_1}; dadd | dsub | dmul | ddiv | drem [; dstore_Z | dstore n]` and
// `dload; <same operand>; dcmpl | dcmpg` as one dispatch (the double twin of
// wave 101's CRATONVM_INTERP_FUSED_LONG_ARITH).
//
// The doubles are parameters, so they sit in locals 0..5 and the first load
// of each group is a `dload_0..3`:
// Rows that should DROP with the switch on:
//   addLoop    s += x; x = x * k       dload_0; dload_2; dadd; dstore_0
//                                      | dload_2; dload 4; dmul; dstore_2
//   cmpLoop    if (a < b) hits++; a = a - d   dload_0; dload_2; dcmpg
//                                      | dload_0; dload 4; dsub; dstore_0
//   divLoop    s = s + a / b; a = a + 1.0   dload_2; dload 4; ddiv (push form)
//                                      | dload_2; dconst_1; dadd; dstore_2
//   remLoop    s = s % m + x            dload_0; dload_2; drem (push form)
// Controls that should stay FLAT (no double group; the off cost is one
// range test of `b1` per `dload_N`):
//   intLoop    s += i over ints
//   longLoop   s += i over longs (wave 101's fused long groups)
//   polyLoop   r = r * t + c with t, c in locals 6+ (`dload n` first: no arm)
//
// Every row runs REPS timed repetitions after one untimed warm-up and
// reports the MINIMUM; compare on the SAME binary, interleaved, medians of
// the per-round minima.
//
// Run:
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L7W102DoubleArithBench
//   CRATONVM_INTERP_FUSED_DOUBLE_ARITH=1 cratonvm --java-home <jdk25> --nojit -cp <dir> L7W102DoubleArithBench
// Also run each arm with the JIT on (no `--nojit`): the checksums must match
// each other and HotSpot's (IEEE arithmetic is exact; the orchestrator
// records HotSpot's lines).
// stdout: per row one deterministic `<row> checksum=<v>` line and one
// `TIME <row> <x> ns/op (min of N)` timing line (filter out `TIME `).
public class L7W102DoubleArithBench {
    interface Row {
        double run();
    }

    static final int REPS = 7;
    static final int ITERS = 1_000_000;
    static int bound = ITERS;

    static double addLoop(double s, double x, double k, int n) {
        for (int i = 0; i < n; i++) {
            s += x;
            x = x * k;
        }
        return s + x;
    }

    static double cmpLoop(double a, double b, double d, int n) {
        int hits = 0;
        for (int i = 0; i < n; i++) {
            if (a < b) {
                hits++;
            }
            a = a - d;
        }
        return hits + a;
    }

    static double divLoop(double s, double a, double b, int n) {
        for (int i = 0; i < n; i++) {
            s = s + a / b;
            a = a + 1.0;
        }
        return s;
    }

    static double remLoop(double s, double m, double x, int n) {
        for (int i = 0; i < n; i++) {
            s = s % m + x;
        }
        return s;
    }

    static double intLoop(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += i;
        }
        return s;
    }

    static double longLoop(long n) {
        long s = 0;
        for (long i = 0; i < n; i++) {
            s += i;
        }
        return s;
    }

    static double polyLoop(int n, int pad, double unused, double r, double t, double c) {
        for (int i = 0; i < n; i++) {
            r = r * t + c;
        }
        return r + pad + unused;
    }

    static void time(String name, Row row) {
        double checksum = row.run(); // untimed warm-up
        long best = Long.MAX_VALUE;
        boolean stable = true;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            double c = row.run();
            long dt = System.nanoTime() - t0;
            if (dt < best) {
                best = dt;
            }
            if (Double.doubleToRawLongBits(c) != Double.doubleToRawLongBits(checksum)) {
                stable = false;
            }
        }
        System.out.println(name + " checksum=" + (stable ? Double.toString(checksum) : "UNSTABLE"));
        System.out.printf("TIME %-12s %8.2f ns/op (min of %d)%n", name, (double) best / ITERS, REPS);
    }

    public static void main(String[] args) {
        time("addLoop", () -> addLoop(0.0, 0.5, 1.0000001, bound));
        time("cmpLoop", () -> cmpLoop(1000.0, 400000.25, 0.75, bound));
        time("divLoop", () -> divLoop(0.0, 1.0, 3.0, bound));
        time("remLoop", () -> remLoop(1.0, 97.5, 1.25, bound));
        time("intLoop", () -> intLoop(bound));
        time("longLoop", () -> longLoop(bound));
        time("polyLoop", () -> polyLoop(bound, 0, 0.0, 0.5, 0.999999, 0.25));
    }
}
