// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i5 wave 101, lane L7: CRATONVM_INTERP_FUSED_LONG_ARITH
// (default off), the `lload_N` / `lload n` arms' `lload; {lload_Y | lload n
// | lconst_0 | lconst_1}; ladd | lsub | lmul | land | lor | lxor [; lstore_Z
// | lstore n]` and `lload; <same operand>; lcmp` as one dispatch
// (docs/internal/fixed-bugs/interpreter-i1-L1-proposal-superinstruction-coverage-KEPT-OFF-20261224.md,
// "Wave 101").
//
// Rows that should DROP with the switch on (javac-shaped long loops whose
// bound is a parameter, so the loop test is `lload i; lload_0; lcmp`):
//   addLoop      s += i              lload 4; lload_0; lcmp | lload_2; lload 4; ladd; lstore_2
//                                    | lload 4; lconst_1; ladd; lstore 4
//   mixLoop      s = (s * m ^ i) & k  lload_2; lload 4; lmul (push form) + the loop groups
//   cmpLoop      if (i < a) hits++    lload 6; lload 4; lcmp + lconst_1 ladd/lsub stores
// Controls that should stay FLAT (no long group; the off cost is one more
// range test of `b1` per `lload`, and these rows have no `lload` at all):
//   intLoop      s += i over ints (already fused int groups)
//   fieldLoop    c.count++; c.total += i   getfield / putfield
//   arrayLoop    a[i & 1023] += i; then a sum   iaload / iastore
//   staticCall   s += inc(i)          invokestatic
//   virtualCall  s += box.get(i)      invokevirtual (monomorphic)
//
// Every row runs REPS timed repetitions after one untimed warm-up and
// reports the MINIMUM; compare on the SAME binary, interleaved, medians of
// the per-round minima.
//
// Run:
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L7W101LongArithBench
//   CRATONVM_INTERP_FUSED_LONG_ARITH=1 cratonvm --java-home <jdk25> --nojit -cp <dir> L7W101LongArithBench
// Also run each arm with the JIT on (no `--nojit`): the checksums must match.
// stdout: per row one deterministic `<row> checksum=<n>` line and one
// `TIME <row> <x> ns/op (min of N)` timing line (filter out `TIME `).
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// addLoop checksum=499999500000
// mixLoop checksum=3160208449
// cmpLoop checksum=500000
// intLoop checksum=1783293664
// fieldLoop checksum=500000500000
// arrayLoop checksum=1783293664
// staticCall checksum=1784293664
// virtualCall checksum=1783293664
// HOTSPOT_EXPECTED_END
public class L7W101LongArithBench {
    interface Row {
        long run();
    }

    static final int REPS = 7;
    static final int ITERS = 1_000_000;
    static long bound = ITERS;

    static long addLoop(long n) {
        long s = 0;
        for (long i = 0; i < n; i++) {
            s += i;
        }
        return s;
    }

    static long mixLoop(long n) {
        long s = 1;
        long m = 31;
        long k = 0xFFFF_FFFFL;
        for (long i = 0; i < n; i++) {
            s = (s * m ^ i) & k;
        }
        return s;
    }

    static long cmpLoop(long n) {
        long hits = 0;
        long a = n;
        for (long i = 0; i < n; i++) {
            if (i < a) {
                hits++;
            }
            a = a - 1;
        }
        return hits + a;
    }

    static long intLoop() {
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += i;
        }
        return s;
    }

    static final class Counter {
        int count;
        long total;
    }

    static long fieldLoop() {
        Counter c = new Counter();
        for (int i = 0; i < ITERS; i++) {
            c.count++;
            c.total += i;
        }
        return c.count + c.total;
    }

    static long arrayLoop() {
        int[] a = new int[1024];
        for (int i = 0; i < ITERS; i++) {
            a[i & 1023] += i;
        }
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    static int inc(int x) {
        return x + 1;
    }

    static long staticCall() {
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += inc(i);
        }
        return s;
    }

    static class Box {
        int get(int x) {
            return x;
        }
    }

    static long virtualCall() {
        Box box = new Box();
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += box.get(i);
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
        System.out.printf("TIME %-12s %8.2f ns/op (min of %d)%n", name, (double) best / ITERS, REPS);
    }

    public static void main(String[] args) {
        time("addLoop", () -> addLoop(bound));
        time("mixLoop", () -> mixLoop(bound));
        time("cmpLoop", () -> cmpLoop(bound));
        time("intLoop", L7W101LongArithBench::intLoop);
        time("fieldLoop", L7W101LongArithBench::fieldLoop);
        time("arrayLoop", L7W101LongArithBench::arrayLoop);
        time("staticCall", L7W101LongArithBench::staticCall);
        time("virtualCall", L7W101LongArithBench::virtualCall);
    }
}
