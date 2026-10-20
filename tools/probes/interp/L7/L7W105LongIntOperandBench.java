// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i5 wave 105, lane L7: CRATONVM_INTERP_FUSED_LONG_INT_OPERAND (default off).
// The fused long group (`CRATONVM_INTERP_FUSED_LONG_ARITH`, on) also takes an int local
// widened by `i2l` as its operand: `lload; {iload_Y | iload n}; i2l; <long op> [; lstore]` and
// `...; lcmp` as one dispatch. `i2l -> ladd` is the top pair of the wave-88 battery census
// (docs/internal/fixed-bugs/interpreter-i85-L7-proposal-opcode-pair-census-of-the-battery-DONE-20261122.md).
//
// Rows that should DROP with the switch on (javac's `long s; int i; s += i` shapes):
//   sumInts   s += i                lload_1; iload_3; i2l; ladd; lstore_1
//   negSum    s += i, i < 0          the same, sign extension checked by the checksum
//   xorLoop   h ^= i                lload_1; iload_3; i2l; lxor; lstore_1
//   cmpInt    if (s < i) hits++     lload_1; iload 5; i2l; lcmp; ifge (an `iload n` operand)
//   sumWide   s += i, s and i past local 3   lload_3; iload 5; i2l; ladd; lstore_3
// Controls that should stay FLAT:
//   addLoop   s += i over a long i  (the long group, unchanged)
//   intLoop   s += i over ints      (no lload)
//   arrayLoop a[i & 1023] += i      (no lload)
//
// Every row runs REPS timed repetitions after one untimed warm-up and reports the MINIMUM;
// compare on the SAME binary, interleaved, medians of the per-round minima.
//
// Run:
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L7W105LongIntOperandBench
//   CRATONVM_INTERP_FUSED_LONG_INT_OPERAND=1 cratonvm --java-home <jdk25> --nojit -cp <dir> L7W105LongIntOperandBench
// Also each arm with the JIT on (no `--nojit`), and with CRATONVM_INTERP_FUSED_LONG_ARITH=0
// (the new operand rides on that switch: off there, nothing fuses): the checksums must match.
// stdout: per row one deterministic `<row> checksum=<n>` line and one
// `TIME <row> <x> ns/op (min of N)` timing line (filter out `TIME `).
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// sumInts checksum=499999500000
// negSum checksum=-500000500000
// xorLoop checksum=1000000
// cmpInt checksum=499999
// sumWide checksum=499999500000
// addLoop checksum=499999500000
// intLoop checksum=1783293664
// arrayLoop checksum=1783293664
// HOTSPOT_EXPECTED_END
public class L7W105LongIntOperandBench {
    interface Row {
        long run();
    }

    static final int REPS = 7;
    static final int ITERS = 1_000_000;
    static int bound = ITERS;

    static long sumInts(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i;
        }
        return s;
    }

    static long negSum(int n) {
        long s = 0;
        for (int i = -n; i < 0; i++) {
            s += i;
        }
        return s;
    }

    static long xorLoop(int n) {
        long h = 0;
        for (int i = 1; i <= n; i++) {
            h ^= i;
        }
        return h;
    }

    static long cmpInt(int n) {
        long s = n / 2;
        long hits = 0;
        for (int i = 0; i < n; i++) {
            if (s < i) {
                hits++;
            }
        }
        return hits;
    }

    static long sumWide(int a, int b, int n) {
        long s = a;
        for (int i = b; i < n; i++) {
            s += i;
        }
        return s;
    }

    static long addLoop(long n) {
        long s = 0;
        for (long i = 0; i < n; i++) {
            s += i;
        }
        return s;
    }

    static long intLoop() {
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += i;
        }
        return s;
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
                checksum = Long.MIN_VALUE;
            }
        }
        System.out.println(name + " checksum=" + checksum);
        System.out.printf("TIME %-12s %8.2f ns/op (min of %d)%n", name, (double) best / ITERS, REPS);
    }

    public static void main(String[] args) {
        time("sumInts", () -> sumInts(bound));
        time("negSum", () -> negSum(bound));
        time("xorLoop", () -> xorLoop(bound));
        time("cmpInt", () -> cmpInt(bound));
        time("sumWide", () -> sumWide(0, 0, bound));
        time("addLoop", () -> addLoop(bound));
        time("intLoop", L7W105LongIntOperandBench::intLoop);
        time("arrayLoop", L7W105LongIntOperandBench::arrayLoop);
    }
}
