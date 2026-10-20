// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 68, lane L7: ns per loop iteration for the
// shapes CRATONVM_INTERP_FUSED_FIELD_INCREMENT targets (`aload_N; dup;
// getfield #F; <int operand>; iadd | isub; putfield #F` answered as one
// group; proposal i67-L7-proposal-a-fused-instance-field-increment-20261031).
// Compare switch off / on on the SAME binary.
//
// Rows (1 000 000 iterations each):
//   fieldInc     `count++` on an int field of `this` (aload_0). Expected down.
//   fieldDec     `down--` (iconst_1; isub). Down.
//   fieldAdd     `total += 7` (bipush). Down.
//   fieldLocal   `acc += k`, `k` a local (iload_N). Down.
//   otherObj     `h.count++` with `h` in local 1 of a static method
//                (aload_1). Down.
//   listAdd      `ArrayList.add` then `clear` every 1024 (modCount++ is the
//                fused shape; `size = s + 1` is not). Down by a little.
//   byteField    `b++` on a byte field (an i2b sits before the putfield:
//                never fused; control, flat).
//   postInc      `last = count++` (dup_x1: never fused; control, flat).
//   localBound   `s += i & 3`, `i < n` (control: no field; flat).
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare switch off / on on the
// SAME fat-LTO binary, interleaved, several rounds, and take the median of
// the per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W68FusedFieldIncrementBench
// stdout is deterministic, identical on HotSpot 25 (and under every switch):
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// fieldInc checksum=1000000
// fieldDec checksum=-1000000
// fieldAdd checksum=7000000
// fieldLocal checksum=3500000
// otherObj checksum=1000000
// listAdd checksum=576
// byteField checksum=64
// postInc checksum=999999
// localBound checksum=1500000
// HOTSPOT_EXPECTED_END
// stderr: `<row> <x> ns/op (min of 9)`, one line per row.
import java.util.ArrayList;

public class L7W68FusedFieldIncrementBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 1_000_000;

    static class Holder {
        int count;
    }

    int count;
    int down;
    int total;
    int acc;
    byte b;
    int post;

    int fieldInc(int n) {
        count = 0;
        for (int i = 0; i < n; i++) {
            count++;
        }
        return count;
    }

    int fieldDec(int n) {
        down = 0;
        for (int i = 0; i < n; i++) {
            down--;
        }
        return down;
    }

    int fieldAdd(int n) {
        total = 0;
        for (int i = 0; i < n; i++) {
            total += 7;
        }
        return total;
    }

    int fieldLocal(int n) {
        acc = 0;
        for (int i = 0; i < n; i++) {
            int k = i & 7;
            acc += k;
        }
        return acc;
    }

    static int otherObj(Holder h, int n) {
        h.count = 0;
        for (int i = 0; i < n; i++) {
            h.count++;
        }
        return h.count;
    }

    static int listAdd(ArrayList<Integer> list, int n) {
        Integer boxed = 1;
        list.clear();
        for (int i = 0; i < n; i++) {
            if (list.size() == 1024) {
                list.clear();
            }
            list.add(boxed);
        }
        return list.size();
    }

    int byteField(int n) {
        b = 0;
        for (int i = 0; i < n; i++) {
            b++;
        }
        return b;
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
        final L7W68FusedFieldIncrementBench t = new L7W68FusedFieldIncrementBench();
        final Holder h = new Holder();
        final ArrayList<Integer> list = new ArrayList<>();
        row("fieldInc", () -> t.fieldInc(n));
        row("fieldDec", () -> t.fieldDec(n));
        row("fieldAdd", () -> t.fieldAdd(n));
        row("fieldLocal", () -> t.fieldLocal(n));
        row("otherObj", () -> otherObj(h, n));
        row("listAdd", () -> listAdd(list, n));
        row("byteField", () -> t.byteField(n));
        row("postInc", () -> t.postInc(n));
        row("localBound", () -> localBound(n));
    }
}
