// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 89, lane L7: the two fusions picked from the
// battery's opcode-pair census
// (docs/internal/fixed-bugs/interpreter-i85-L7-proposal-opcode-pair-census-of-the-battery-DONE-20261122), each behind a
// default-off switch:
//   CRATONVM_INTERP_FUSED_LOAD_PAIR   aload_N; {aload_M | iload_M}; putfield
//                                     (one dispatch, quickened store) or
//                                     ...; invoke* (one dispatch for the loads)
//   CRATONVM_INTERP_FUSED_ICONST_GOTO iconst_1; goto (boolean materialisation)
//
// Rows (ITERS = 2 000 000 iterations each):
//   ctorLoop      `new P(i & 7, o)`: the constructor body is
//                 aload_0; iload_1; putfield; aload_0; aload_2; putfield.
//   setterLoop    `h.set(i & 3)`: aload_0; iload_1; putfield.
//   callPairLoop  `via(b)`: aload_0; aload_1; invokevirtual get.
//   boolLoop      `gt3(i & 7)`: `return x > 3;`, javac's
//                 if_icmple; iconst_1; goto; iconst_0; ireturn.
// ctorLoop / setterLoop / callPairLoop move with LOAD_PAIR, boolLoop with
// ICONST_GOTO; every other row should stay flat under each switch.
//
// Every row runs REPS = 9 timed repetitions after one untimed warm-up and
// reports the MINIMUM; compare the switches on the SAME binary, interleaved,
// medians of the per-round minima.
//
// Run:
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L7W89LoadPairBench
//   CRATONVM_INTERP_FUSED_LOAD_PAIR=1 cratonvm --java-home <jdk25> --nojit -cp <dir> L7W89LoadPairBench
//   CRATONVM_INTERP_FUSED_ICONST_GOTO=1 cratonvm --java-home <jdk25> --nojit -cp <dir> L7W89LoadPairBench
// stdout: per row one deterministic `<row> checksum=<n>` line and one
// `<row> <x> ns/op (min of N)` timing line (not compared).
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// ctorLoop checksum=7000000
// setterLoop checksum=3000000
// callPairLoop checksum=2000000
// boolLoop checksum=1000000
// HOTSPOT_EXPECTED_END
public class L7W89LoadPairBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 2_000_000;

    static final class P {
        final int a;
        final Object b;

        P(int a, Object b) {
            this.a = a;
            this.b = b;
        }
    }

    static final class Holder {
        int v;

        void set(int v) {
            this.v = v;
        }
    }

    static final class Box {
        int x = 1;
    }

    int get(Box b) {
        return b.x;
    }

    int via(Box b) {
        return this.get(b);
    }

    static boolean gt3(int x) {
        return x > 3;
    }

    // 2 000 000 / 8 blocks of 0 + ... + 7 = 28: 7 000 000.
    static int ctorLoop() {
        Object o = "o";
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += new P(i & 7, o).a;
        }
        return s;
    }

    // 2 000 000 / 4 blocks of 0 + 1 + 2 + 3 = 6: 3 000 000.
    static int setterLoop() {
        Holder h = new Holder();
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            h.set(i & 3);
            s += h.v;
        }
        return s;
    }

    // Every call answers 1: 2 000 000.
    int callPairLoop() {
        Box b = new Box();
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += via(b);
        }
        return s;
    }

    // 4..7 of every 8 are > 3: 1 000 000.
    static int boolLoop() {
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            if (gt3(i & 7)) {
                s++;
            }
        }
        return s;
    }

    static void time(String name, Row row) {
        int checksum = row.run(); // untimed warm-up
        long best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            int c = row.run();
            long dt = System.nanoTime() - t0;
            if (dt < best) {
                best = dt;
            }
            if (c != checksum) {
                checksum = -1;
            }
        }
        System.out.println(name + " checksum=" + checksum);
        System.out.printf("%-15s %8.2f ns/op (min of %d)%n", name, (double) best / ITERS, REPS);
    }

    public static void main(String[] args) {
        L7W89LoadPairBench self = new L7W89LoadPairBench();
        time("ctorLoop", L7W89LoadPairBench::ctorLoop);
        time("setterLoop", L7W89LoadPairBench::setterLoop);
        time("callPairLoop", self::callPairLoop);
        time("boolLoop", L7W89LoadPairBench::boolLoop);
    }
}
