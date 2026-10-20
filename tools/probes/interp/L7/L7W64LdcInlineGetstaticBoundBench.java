// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 64, lane L7: ns per loop iteration for the
// shapes the two wave-64 switches target.
//
//   CRATONVM_INTERP_LDC_INLINE_HIT=1         the `ldc` / `ldc_w` arm pushes a
//       warm int / float itself instead of calling `execute_ldc` (proposal
//       i63-L7-proposal-the-ldc-arm-answers-a-warm-int-inline-20261027).
//   CRATONVM_INTERP_FUSED_GETSTATIC_BOUND=1  the loop head `iload i;
//       getstatic #F; if_icmp<cond>` is one fused dispatch (proposal
//       i63-L7-proposal-the-fused-compare-and-branch-takes-a-getstatic-int-bound-20261027).
//
// Rows (1 000 000 iterations each):
//   staticBound   `i < N`, `N` a non-final static int. GETSTATIC_BOUND:
//                 expected down by about two dispatches and the out-of-line
//                 `op_getstatic` call.
//   volatileBound `i < V`, `V` a volatile static int (keeps its fence).
//                 GETSTATIC_BOUND: expected down, by less.
//   ldcBody       `s += 100_003` with a local bound: the `ldc` arm itself.
//                 LDC_INLINE_HIT: expected down (one call, two re-indexes).
//   floatBody     `f += 0.5f`: a float `ldc`. LDC_INLINE_HIT: expected down.
//   ldcBoth       `i < 1_000_000` and `s += 100_003 + (i & 3)`: an `ldc`
//                 bound (wave 63's FUSED_LDC_BOUND) and an `ldc` body.
//   ldcBound      `i < 1_000_000` (control for LDC_INLINE_HIT: the bound is
//                 an `ldc` too, so it moves with it unless FUSED_LDC_BOUND
//                 is on, which takes it first).
//   localBound    `i < n` with `n` a parameter (control: already fused,
//                 flat under both switches).
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM (the least-disturbed run), not
// a mean. Still compare switch off / on on the SAME fat-LTO binary,
// interleaved, several rounds, and take the median of the per-round minima.
//
// Run: cratonvm --java-home <jdk25> [--nojit] -cp <dir> L7W64LdcInlineGetstaticBoundBench
//      under each switch off / on. With the JIT on the loops are
//      OSR-compiled after a few thousand back edges, so the switches should
//      move only the interpreted prefix there; --nojit is the measurement.
// stdout is deterministic, identical on HotSpot 25 (and under every switch):
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// staticBound checksum=1500000
// volatileBound checksum=1500000
// ldcBody checksum=1218752192
// floatBody checksum=500000
// ldcBoth checksum=1220252192
// ldcBound checksum=1500000
// localBound checksum=1500000
// HOTSPOT_EXPECTED_END
// stderr: `<row> <x> ns/op (min of 9)`, one line per row.
public class L7W64LdcInlineGetstaticBoundBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 1_000_000;

    static int N = ITERS;
    static volatile int V = ITERS;

    static int staticBound() {
        int s = 0;
        for (int i = 0; i < N; i++) {
            s += i & 3;
        }
        return s;
    }

    static int volatileBound() {
        int s = 0;
        for (int i = 0; i < V; i++) {
            s += i & 3;
        }
        return s;
    }

    static int ldcBody(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += 100_003;
        }
        return s;
    }

    static int floatBody(int n) {
        float f = 0;
        for (int i = 0; i < n; i++) {
            f += 0.5f;
        }
        return (int) f;
    }

    static int ldcBoth() {
        int s = 0;
        for (int i = 0; i < 1_000_000; i++) {
            s += 100_003 + (i & 3);
        }
        return s;
    }

    static int ldcBound() {
        int s = 0;
        for (int i = 0; i < 1_000_000; i++) {
            s += i & 3;
        }
        return s;
    }

    static int localBound(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += i & 3;
        }
        return s;
    }

    static void row(String name, Row r) {
        int checksum = r.run(); // untimed warm-up: fills the sites
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < REPS; rep++) {
            long t0 = System.nanoTime();
            int c = r.run();
            long dt = System.nanoTime() - t0;
            if (dt < best) {
                best = dt;
            }
            if (c != checksum) {
                checksum = -1;
            }
        }
        System.out.println(name + " checksum=" + checksum);
        System.err.printf("%-13s %8.2f ns/op (min of %d)%n", name, (double) best / ITERS, REPS);
    }

    public static void main(String[] args) {
        final int n = ITERS;
        row("staticBound", L7W64LdcInlineGetstaticBoundBench::staticBound);
        row("volatileBound", L7W64LdcInlineGetstaticBoundBench::volatileBound);
        row("ldcBody", () -> ldcBody(n));
        row("floatBody", () -> floatBody(n));
        row("ldcBoth", L7W64LdcInlineGetstaticBoundBench::ldcBoth);
        row("ldcBound", L7W64LdcInlineGetstaticBoundBench::ldcBound);
        row("localBound", () -> localBound(n));
    }
}
