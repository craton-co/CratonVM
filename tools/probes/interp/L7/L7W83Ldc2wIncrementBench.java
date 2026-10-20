// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 83, lane L7: ns per loop iteration for
//   CRATONVM_INTERP_FUSED_INCREMENT_LDC2W  the fused static and field
//       increments' long operand may be a warm `ldc2_w` long constant
//       (i69-L7-proposal-ldc2w-long-operand-for-the-fused-increments-
//       20261102; default off; needs the default-on wide switches and the
//       `ldc2_w` inline hit)
//
// Rows (ITERS = 1 000 000 iterations each):
//   staticLdc     `S += 3L` on a long static (`getstatic; ldc2_w; ladd;
//                 putstatic`). On: one dispatch instead of four, expected
//                 down like the wave-68 `longStatic` row.
//   fieldLdc      `l += 3L` on a long field (`aload_0; dup; getfield;
//                 ldc2_w; ladd; putfield`; W69's `longLdc`). On: down.
//   fieldLdcValue `sum += (v += 7L)` (the `dup2_x1` pre-increment). On:
//                 down.
//   staticLconst  `S2++` on a long static (`lconst_1`, fused since wave
//                 68). Control: flat.
//   fieldLconst   `l2++` on a long field (`lconst_1`). Control: flat.
//   staticDouble  `SD += 2.5` (`ldc2_w` of a Double; `dadd`): never fused.
//                 On: the `getstatic` arm's prefilter lets the `ldc2_w`
//                 through, so a declined call per iteration (the off-state
//                 cost of the switch's ON position; expected small).
//   fieldDouble   `d += 2.5` on a double field: the helper already declined
//                 it at its decoder; on, the decoder also probes the
//                 constant table first. Expected flat or up a little.
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare the switch off / on
// on the SAME binary, interleaved, several rounds, and take the median of
// the per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W83Ldc2wIncrementBench
// stdout: per row one deterministic `<row> checksum=<n>` line (identical on
// HotSpot 25 and under every switch) and one `<row> <x> ns/op (min of N)`
// timing line.
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// staticLdc checksum=3000000
// fieldLdc checksum=3000000
// fieldLdcValue checksum=3500003500000
// staticLconst checksum=1000000
// fieldLconst checksum=1000000
// staticDouble checksum=2500000
// fieldDouble checksum=2500000
// HOTSPOT_EXPECTED_END
// (the timing lines, which contain "ns/", are not compared)
public class L7W83Ldc2wIncrementBench {
    interface Row {
        long run();
    }

    static final int REPS = 9;
    static final int ITERS = 1_000_000;

    static long sl;
    static long sl2;
    static double sd;

    long l;
    long l2;
    long v;
    double d;

    static long staticLdc(int n) {
        sl = 0;
        for (int i = 0; i < n; i++) {
            sl += 3L;
        }
        return sl;
    }

    long fieldLdc(int n) {
        l = 0;
        for (int i = 0; i < n; i++) {
            l += 3L;
        }
        return l;
    }

    long fieldLdcValue(int n) {
        v = 0;
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (v += 7L);
        }
        return sum;
    }

    static long staticLconst(int n) {
        sl2 = 0;
        for (int i = 0; i < n; i++) {
            sl2++;
        }
        return sl2;
    }

    long fieldLconst(int n) {
        l2 = 0;
        for (int i = 0; i < n; i++) {
            l2++;
        }
        return l2;
    }

    static long staticDouble(int n) {
        sd = 0;
        for (int i = 0; i < n; i++) {
            sd += 2.5;
        }
        return (long) sd;
    }

    long fieldDouble(int n) {
        d = 0;
        for (int i = 0; i < n; i++) {
            d += 2.5;
        }
        return (long) d;
    }

    static void row(String name, Row r) {
        long checksum = r.run(); // untimed warm-up: fills the sites
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < REPS; rep++) {
            long t0 = System.nanoTime();
            long c = r.run();
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
        final int n = ITERS;
        final L7W83Ldc2wIncrementBench t = new L7W83Ldc2wIncrementBench();
        row("staticLdc", () -> staticLdc(n));
        row("fieldLdc", () -> t.fieldLdc(n));
        row("fieldLdcValue", () -> t.fieldLdcValue(n));
        row("staticLconst", () -> staticLconst(n));
        row("fieldLconst", () -> t.fieldLconst(n));
        row("staticDouble", () -> staticDouble(n));
        row("fieldDouble", () -> t.fieldDouble(n));
    }
}
