// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 69, lane L7: ns per loop iteration for the shapes
// CRATONVM_INTERP_FUSED_FIELD_INCREMENT_WIDE adds to the fused instance-field
// increment (narrow fields behind their own i2b / i2s / i2c, long fields with
// lconst_0/1 or a long local), and for the declined shapes
// CRATONVM_INTERP_FUSED_INCREMENT_EARLY_DECLINE screens before the body.
// Compare each switch off / on on the SAME binary (both default off; the
// wave-68 CRATONVM_INTERP_FUSED_FIELD_INCREMENT and the wave-67/68 static
// increments are default on and stay on).
//
// Rows (1 000 000 iterations each):
//   fieldInc      `count++` on an int field (fused since wave 68; must not
//                 move under either switch).
//   byteField     `b++` on a byte field (i2b). WIDE: down.
//   shortField    `s += 3` on a short field (i2s). WIDE: down.
//   charField     `c++` on a char field (i2c). WIDE: down.
//   longField     `l++` on a long field (lconst_1; ladd). WIDE: down.
//   longLocal     `l += k`, `k` a long local (lload). WIDE: down.
//   postInc       `last = post++` (dup_x1: never fused). EARLY: down a little.
//   pairBody      `x = y + 1` over two int statics (the static increment's
//                 decline). EARLY: down a little.
//   longLdc       `l += 3` (ldc2_w: never fused). EARLY with WIDE: flat or
//                 down a little.
//   longIntLocal  `l += i & 7` (an i2l: never fused). EARLY: down a little.
//   volatileByte  `vb++` on a volatile byte (never quickened: the body's
//                 site probe declines; EARLY does not help, WIDE flat).
//   localBound    `s += i & 3`, `i < n` (control: no field; flat).
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare switch off / on on the
// SAME fat-LTO binary, interleaved, several rounds, and take the median of
// the per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W69FusedFieldIncrementWideBench
// stdout is deterministic, identical on HotSpot 25 (and under every switch):
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// fieldInc checksum=1000000
// byteField checksum=64
// shortField checksum=-14656
// charField checksum=16960
// longField checksum=1000000
// longLocal checksum=3000000
// postInc checksum=999999
// pairBody checksum=42
// longLdc checksum=3000000
// longIntLocal checksum=3500000
// volatileByte checksum=64
// localBound checksum=1500000
// HOTSPOT_EXPECTED_END
// stderr: `<row> <x> ns/op (min of 9)`, one line per row.
public class L7W69FusedFieldIncrementWideBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 1_000_000;

    static int x;
    static int y = 41;

    int count;
    byte b;
    short s;
    char c;
    long l;
    long lk;
    int post;
    long ll;
    long li;
    volatile byte vb;

    int fieldInc(int n) {
        count = 0;
        for (int i = 0; i < n; i++) {
            count++;
        }
        return count;
    }

    int byteField(int n) {
        b = 0;
        for (int i = 0; i < n; i++) {
            b++;
        }
        return b;
    }

    int shortField(int n) {
        s = 0;
        for (int i = 0; i < n; i++) {
            s += 3;
        }
        return s;
    }

    int charField(int n) {
        c = 0;
        for (int i = 0; i < n; i++) {
            c++;
        }
        return c;
    }

    int longField(int n) {
        l = 0;
        for (int i = 0; i < n; i++) {
            l++;
        }
        return (int) l;
    }

    int longLocal(int n) {
        lk = 0;
        long k = 3;
        for (int i = 0; i < n; i++) {
            lk += k;
        }
        return (int) lk;
    }

    int postInc(int n) {
        post = 0;
        int last = 0;
        for (int i = 0; i < n; i++) {
            last = post++;
        }
        return last;
    }

    static int pairBody(int n) {
        for (int i = 0; i < n; i++) {
            x = y + 1;
        }
        return x;
    }

    int longLdc(int n) {
        ll = 0;
        for (int i = 0; i < n; i++) {
            ll += 3;
        }
        return (int) ll;
    }

    int longIntLocal(int n) {
        li = 0;
        for (int i = 0; i < n; i++) {
            li += i & 7;
        }
        return (int) li;
    }

    int volatileByte(int n) {
        vb = 0;
        for (int i = 0; i < n; i++) {
            vb++;
        }
        return vb;
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
        final L7W69FusedFieldIncrementWideBench t = new L7W69FusedFieldIncrementWideBench();
        row("fieldInc", () -> t.fieldInc(n));
        row("byteField", () -> t.byteField(n));
        row("shortField", () -> t.shortField(n));
        row("charField", () -> t.charField(n));
        row("longField", () -> t.longField(n));
        row("longLocal", () -> t.longLocal(n));
        row("postInc", () -> t.postInc(n));
        row("pairBody", () -> pairBody(n));
        row("longLdc", () -> t.longLdc(n));
        row("longIntLocal", () -> t.longIntLocal(n));
        row("volatileByte", () -> t.volatileByte(n));
        row("localBound", () -> localBound(n));
    }
}
