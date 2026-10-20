// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 67, lane L7: ns per loop iteration for the
// shapes CRATONVM_INTERP_FUSED_STATIC_INCREMENT targets (`getstatic #F;
// <int operand>; iadd | isub; putstatic #F` answered as one group; proposal
// i66-L7-proposal-a-fused-static-int-increment-20261030). Measure against
// the now-default CRATONVM_INTERP_GETSTATIC_INLINE_HIT and
// CRATONVM_INTERP_PUTSTATIC_INLINE_HIT (held on both sides), switch off / on.
//
// Rows (1 000 000 iterations each):
//   counterBody  `counter++` on a static int (iconst_1; iadd). Expected down.
//   decBody      `counter--` (iconst_1; isub). Down.
//   addBody      `total += 7` (bipush). Down.
//   localBody    `acc += k`, `k` a local (iload_N). Down.
//   volatileBody `vcount++` on a volatile static int (the same three SeqCst
//                fences either way). Down by less.
//   pairBody     `x = y + 1` (two pool indices: never fused; control, flat).
//   localBound   `s += i & 3`, `i < n` (control: no static; flat).
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare switch off / on on the
// SAME fat-LTO binary, interleaved, several rounds, and take the median of
// the per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W67FusedStaticIncrementBench
// stdout is deterministic, identical on HotSpot 25 (and under every switch):
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// counterBody checksum=1000000
// decBody checksum=0
// addBody checksum=7000000
// localBody checksum=3500000
// volatileBody checksum=1000000
// pairBody checksum=1000000
// localBound checksum=1500000
// HOTSPOT_EXPECTED_END
// stderr: `<row> <x> ns/op (min of 9)`, one line per row.
public class L7W67FusedStaticIncrementBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 1_000_000;

    static int counter;
    static int total;
    static int acc;
    static volatile int vcount;
    static int x;
    static int y;

    static int counterBody(int n) {
        counter = 0;
        for (int i = 0; i < n; i++) {
            counter++;
        }
        return counter;
    }

    static int decBody(int n) {
        counter = n;
        for (int i = 0; i < n; i++) {
            counter--;
        }
        return counter;
    }

    static int addBody(int n) {
        total = 0;
        for (int i = 0; i < n; i++) {
            total += 7;
        }
        return total;
    }

    static int localBody(int n) {
        acc = 0;
        for (int i = 0; i < n; i++) {
            int k = i & 7;
            acc += k;
        }
        return acc;
    }

    static int volatileBody(int n) {
        vcount = 0;
        for (int i = 0; i < n; i++) {
            vcount++;
        }
        return vcount;
    }

    static int pairBody(int n) {
        y = 0;
        for (int i = 0; i < n; i++) {
            x = y + 1;
            y = x;
        }
        return y;
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
        row("counterBody", () -> counterBody(n));
        row("decBody", () -> decBody(n));
        row("addBody", () -> addBody(n));
        row("localBody", () -> localBody(n));
        row("volatileBody", () -> volatileBody(n));
        row("pairBody", () -> pairBody(n));
        row("localBound", () -> localBound(n));
    }
}
