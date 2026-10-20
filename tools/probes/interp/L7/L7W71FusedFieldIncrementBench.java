// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 71, lane L7: ns per loop iteration for
//  (1) the `aload_N; aload_N; getfield` prefix under
//      CRATONVM_INTERP_FUSED_FIELD_INCREMENT_NO_DUP, now screened in the
//      dispatch loop (`field_fast::no_dup_group_may_fuse`) so the two-field
//      shapes (`this.a = this.b + k`) no longer pay a declined helper call
//      (wave 70: `twoFields` +15% with the switch on);
//  (2) the field increment whose value is used, under
//      CRATONVM_INTERP_FUSED_FIELD_INCREMENT_VALUE (`last = post++`,
//      `s += ++pre`, `buf[size++] = i`, long and byte forms).
// Both switches default off; CRATONVM_INTERP_FUSED_FIELD_INCREMENT and
// CRATONVM_INTERP_FUSED_FIELD_INCREMENT_WIDE are default on and stay on.
//
// Rows (1 000 000 iterations each):
//   noDupInc      `count = count + 1`. NO_DUP: down (the W70 hit).
//   twoFields     `a = b + 1` over two int fields of `this`. NO_DUP: must
//                 now be flat against NO_DUP off (it was +15% in W70).
//   twoFieldsLocal `a2 = b + k` (iload). NO_DUP: flat.
//   twoFieldsSub  `a3 = b - 3`. NO_DUP: flat.
//   twoFieldsLong `la = lb + 1` over two long fields. NO_DUP: flat.
//   threeFields   `a4 = b + c` (the operand is a load: refused at the
//                 operand byte). NO_DUP: flat.
//   postUsed      `last = post++` (dup_x1 after the getfield). VALUE: down.
//   preUsed       `s += ++pre` (dup_x1 before the putfield). VALUE: down.
//   appendPost    `buf[size++ & 1023] = i`. VALUE: down.
//   postLong      `lastL = pl++` (dup2_x1). VALUE: down.
//   preLong       `ls += ++pl2` (dup2_x1). VALUE: down.
//   preByte       `s += ++bb` on a byte field (i2b; dup_x1). VALUE: down.
//   fieldInc      `fc++` (the statement form, fused since wave 68). Flat
//                 under both switches.
//   localBound    `s += i & 3` (no field). Flat.
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare switch off / on on the
// SAME fat-LTO binary, interleaved, several rounds, and take the median of
// the per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W71FusedFieldIncrementBench
// stdout: per row one deterministic `<row> checksum=<n>` line (identical on
// HotSpot 25 and under every switch) and one `<row> <x> ns/op (min of 9)`
// timing line.
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// noDupInc checksum=1000000
// twoFields checksum=42
// twoFieldsLocal checksum=44
// twoFieldsSub checksum=38
// twoFieldsLong checksum=42
// threeFields checksum=42
// postUsed checksum=999999
// preUsed checksum=1784293664
// appendPost checksum=1000000
// postLong checksum=999999
// preLong checksum=1784293716
// preByte checksum=-497888
// fieldInc checksum=1000000
// localBound checksum=1500000
// HOTSPOT_EXPECTED_END
// (the timing lines, which contain "ns/", are not compared)
public class L7W71FusedFieldIncrementBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 1_000_000;

    int count;
    int a;
    int a2;
    int a3;
    int a4;
    int b = 41;
    int c = 1;
    long la;
    long lb = 41;
    int post;
    int pre;
    int[] buf = new int[1024];
    int size;
    long pl;
    long pl2;
    byte bb;
    int fc;

    int noDupInc(int n) {
        count = 0;
        for (int i = 0; i < n; i++) {
            count = count + 1;
        }
        return count;
    }

    int twoFields(int n) {
        for (int i = 0; i < n; i++) {
            a = b + 1;
        }
        return a;
    }

    int twoFieldsLocal(int n) {
        int k = 3;
        for (int i = 0; i < n; i++) {
            a2 = b + k;
        }
        return a2;
    }

    int twoFieldsSub(int n) {
        for (int i = 0; i < n; i++) {
            a3 = b - 3;
        }
        return a3;
    }

    int twoFieldsLong(int n) {
        for (int i = 0; i < n; i++) {
            la = lb + 1;
        }
        return (int) la;
    }

    int threeFields(int n) {
        for (int i = 0; i < n; i++) {
            a4 = b + c;
        }
        return a4;
    }

    int postUsed(int n) {
        post = 0;
        int last = 0;
        for (int i = 0; i < n; i++) {
            last = post++;
        }
        return last;
    }

    int preUsed(int n) {
        pre = 0;
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += ++pre;
        }
        return s;
    }

    int appendPost(int n) {
        size = 0;
        for (int i = 0; i < n; i++) {
            buf[size++ & 1023] = i;
        }
        return size;
    }

    int postLong(int n) {
        pl = 0;
        long last = 0;
        for (int i = 0; i < n; i++) {
            last = pl++;
        }
        return (int) last;
    }

    int preLong(int n) {
        pl2 = 0;
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += ++pl2;
        }
        return (int) (s ^ (s >>> 32));
    }

    int preByte(int n) {
        bb = 0;
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += ++bb;
        }
        return s;
    }

    int fieldInc(int n) {
        fc = 0;
        for (int i = 0; i < n; i++) {
            fc++;
        }
        return fc;
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
        System.out.printf("%-15s %8.2f ns/op (min of %d)%n", name, (double) best / ITERS, REPS);
    }

    public static void main(String[] args) {
        final int n = ITERS;
        final L7W71FusedFieldIncrementBench t = new L7W71FusedFieldIncrementBench();
        row("noDupInc", () -> t.noDupInc(n));
        row("twoFields", () -> t.twoFields(n));
        row("twoFieldsLocal", () -> t.twoFieldsLocal(n));
        row("twoFieldsSub", () -> t.twoFieldsSub(n));
        row("twoFieldsLong", () -> t.twoFieldsLong(n));
        row("threeFields", () -> t.threeFields(n));
        row("postUsed", () -> t.postUsed(n));
        row("preUsed", () -> t.preUsed(n));
        row("appendPost", () -> t.appendPost(n));
        row("postLong", () -> t.postLong(n));
        row("preLong", () -> t.preLong(n));
        row("preByte", () -> t.preByte(n));
        row("fieldInc", () -> t.fieldInc(n));
        row("localBound", () -> localBound(n));
    }
}
