// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 70, lane L7: ns per loop iteration for the field
// update written without `dup` (`count = count + 1`: `aload_0; aload_0;
// getfield; iconst_1; iadd; putfield`), which
// CRATONVM_INTERP_FUSED_FIELD_INCREMENT_NO_DUP adds to the fused
// instance-field increment, and for the controls that must not move.
// Compare the switch off / on on the SAME binary (default off; the wave-68
// CRATONVM_INTERP_FUSED_FIELD_INCREMENT is default on and stays on; run the
// noDupLong row also with CRATONVM_INTERP_FUSED_FIELD_INCREMENT_WIDE=1, and
// the twoFields row also with CRATONVM_INTERP_FUSED_INCREMENT_EARLY_DECLINE=1).
//
// Rows (1 000 000 iterations each):
//   noDupInc     `count = count + 1` (iconst_1). NO_DUP: down.
//   noDupSub     `down = down - 3` (iconst_3; isub). NO_DUP: down.
//   noDupLocal   `acc = acc + k`, `k` an int local (iload). NO_DUP: down.
//   noDupLong    `l = l + 1` on a long field (lconst_1; ladd). NO_DUP with
//                WIDE: down; NO_DUP alone: flat (the body declines).
//   twoFields    `a = b + 1` over two int fields of `this` (the prefix
//                passes, the pool indexes differ: a helper call that
//                declines). NO_DUP: flat or up a little; with EARLY_DECLINE
//                the screen returns before the body.
//   crossField   `this.cnt = other.cnt + 1` (aload_0; aload_1: two locals,
//                never offered to the helper). Flat.
//   fieldInc     `count++` (the `dup` form, fused since wave 68). Flat.
//   postInc      `last = post++` (dup_x1: never fused). Flat.
//   localBound   `s += i & 3`, `i < n` (no field). Flat.
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare switch off / on on the
// SAME fat-LTO binary, interleaved, several rounds, and take the median of
// the per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W70FusedFieldIncrementNoDupBench
// stdout is deterministic, identical on HotSpot 25 (and under every switch):
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// noDupInc checksum=1000000
// noDupSub checksum=-3000000
// noDupLocal checksum=3000000
// noDupLong checksum=1000000
// twoFields checksum=42
// crossField checksum=8
// fieldInc checksum=1000000
// postInc checksum=999999
// localBound checksum=1500000
// HOTSPOT_EXPECTED_END
// stderr: `<row> <x> ns/op (min of 9)`, one line per row.
public class L7W70FusedFieldIncrementNoDupBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 1_000_000;

    int count;
    int down;
    int acc;
    long l;
    int a;
    int b = 41;
    int cnt;
    int fc;
    int post;

    int noDupInc(int n) {
        count = 0;
        for (int i = 0; i < n; i++) {
            count = count + 1;
        }
        return count;
    }

    int noDupSub(int n) {
        down = 0;
        for (int i = 0; i < n; i++) {
            down = down - 3;
        }
        return down;
    }

    int noDupLocal(int n) {
        acc = 0;
        int k = 3;
        for (int i = 0; i < n; i++) {
            acc = acc + k;
        }
        return acc;
    }

    int noDupLong(int n) {
        l = 0;
        for (int i = 0; i < n; i++) {
            l = l + 1;
        }
        return (int) l;
    }

    int twoFields(int n) {
        for (int i = 0; i < n; i++) {
            a = b + 1;
        }
        return a;
    }

    int crossField(L7W70FusedFieldIncrementNoDupBench other, int n) {
        for (int i = 0; i < n; i++) {
            this.cnt = other.cnt + 1;
        }
        return cnt;
    }

    int fieldInc(int n) {
        fc = 0;
        for (int i = 0; i < n; i++) {
            fc++;
        }
        return fc;
    }

    int postInc(int n) {
        post = 0;
        int last = 0;
        for (int i = 0; i < n; i++) {
            last = post++;
        }
        return last;
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
        final L7W70FusedFieldIncrementNoDupBench t = new L7W70FusedFieldIncrementNoDupBench();
        final L7W70FusedFieldIncrementNoDupBench other = new L7W70FusedFieldIncrementNoDupBench();
        other.cnt = 7;
        row("noDupInc", () -> t.noDupInc(n));
        row("noDupSub", () -> t.noDupSub(n));
        row("noDupLocal", () -> t.noDupLocal(n));
        row("noDupLong", () -> t.noDupLong(n));
        row("twoFields", () -> t.twoFields(n));
        row("crossField", () -> t.crossField(other, n));
        row("fieldInc", () -> t.fieldInc(n));
        row("postInc", () -> t.postInc(n));
        row("localBound", () -> localBound(n));
    }
}
