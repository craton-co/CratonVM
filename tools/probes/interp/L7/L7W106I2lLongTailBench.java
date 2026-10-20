// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i5 wave 106, lane L7: CRATONVM_INTERP_FUSED_I2L_LONG_TAIL (default off).
// The `i2l` arm runs `i2l; ladd | lsub | lmul | land | lor | lxor | lcmp [; lstore]` over the
// operand stack's `long, int` as one dispatch: javac's `sum += <int expression>` with a long
// sum whose term is NOT a plain int local (wave 105's `lload; iload; i2l; <op>` group already
// takes that one). `i2l -> ladd` (1.93%) and `ladd -> lstore_1` (1.26%) are the top pairs of
// the wave-88 battery census
// (docs/internal/fixed-bugs/interpreter-i85-L7-proposal-opcode-pair-census-of-the-battery-DONE-20261122.md).
//
// Rows that should DROP with the switch on (three dispatches become one per element):
//   prodSum    s += i * 3            ...; imul; i2l; ladd; lstore_1
//   maskSum    s += i & 0xff         ...; iand; i2l; ladd; lstore_1
//   fieldSum   s += h.f              aload_0; getfield; i2l; ladd; lstore_1
//   charSum    s += str.charAt(k)    invokevirtual; i2l; ladd; lstore_1 (call-dominated: small)
//   hashMix    h = h * 31 + (i ^ 7)  ...; ixor; i2l; ladd; lstore_1
//   cmpExpr    if (s < i * 2) hits++ ...; imul; i2l; lcmp; ifge (two dispatches become one)
// Controls that should stay FLAT:
//   sumInts    s += i                (wave 105's local-operand group; no `i2l` dispatch)
//   intLoop    s += i over ints      (no `i2l`)
//   widenStore long x = i * 3; s ^= x (`i2l; lstore`: not a tail, declines on the byte test)
//
// Every row runs REPS timed repetitions after one untimed warm-up and reports the MINIMUM;
// compare on the SAME binary, interleaved, medians of the per-round minima.
//
// Run:
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L7W106I2lLongTailBench
//   CRATONVM_INTERP_FUSED_I2L_LONG_TAIL=1 cratonvm --java-home <jdk25> --nojit -cp <dir> L7W106I2lLongTailBench
// Also each arm with the JIT on (no `--nojit`), and under --compatible: the checksums must match.
// stdout: per row one deterministic `<row> checksum=<n>` line and one
// `TIME <row> <x> ns/op (min of N)` timing line (filter out `TIME `).
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// prodSum checksum=1499998500000
// maskSum checksum=127493856
// fieldSum checksum=1499998500000
// charSum checksum=85578125
// hashMix checksum=-131555623846085920
// cmpExpr checksum=749999
// sumInts checksum=499999500000
// intLoop checksum=1783293664
// widenStore checksum=3261120
// HOTSPOT_EXPECTED_END
public class L7W106I2lLongTailBench {
    interface Row {
        long run();
    }

    static final class Holder {
        int f;
    }

    static final int REPS = 7;
    static final int ITERS = 1_000_000;
    static int bound = ITERS;
    static final String STR = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789+/";

    static long prodSum(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i * 3;
        }
        return s;
    }

    static long maskSum(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i & 0xff;
        }
        return s;
    }

    static long fieldSum(Holder h, int n) {
        long s = 0;
        h.f = 0;
        for (int i = 0; i < n; i++) {
            s += h.f;
            h.f += 3;
        }
        return s;
    }

    static long charSum(String str, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += str.charAt(i & 63);
        }
        return s;
    }

    static long hashMix(int n) {
        long h = 0;
        for (int i = 0; i < n; i++) {
            h = h * 31 + (i ^ 7);
        }
        return h;
    }

    static long cmpExpr(int n) {
        long s = n / 2;
        long hits = 0;
        for (int i = 0; i < n; i++) {
            if (s < i * 2) {
                hits++;
            }
        }
        return hits;
    }

    static long sumInts(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i;
        }
        return s;
    }

    static long intLoop(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += i;
        }
        return s;
    }

    static long widenStore(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            long x = i * 3;
            s ^= x;
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
        Holder h = new Holder();
        time("prodSum", () -> prodSum(bound));
        time("maskSum", () -> maskSum(bound));
        time("fieldSum", () -> fieldSum(h, bound));
        time("charSum", () -> charSum(STR, bound));
        time("hashMix", () -> hashMix(bound));
        time("cmpExpr", () -> cmpExpr(bound));
        time("sumInts", () -> sumInts(bound));
        time("intLoop", () -> intLoop(bound));
        time("widenStore", () -> widenStore(bound));
    }
}
