// j27 wave 4 (lane CLONEIR): the inline array clone() of the optimizing tier
// (CRATONVM_JIT_IR_CLONE_INLINE, opt-in): an array clone() bound to a thin clone
// helper is allocated and copied inline, every miss declining to that helper.
// Every row is a hot static method whose one `invokevirtual <array>.clone()` is
// the site under test; the instance rows pin what must NOT change (the helper
// and the native keep them).
// Page: docs/feature-designs/native-dispatch-architecture.md
//
// Expected HotSpot 25 output (facts only), identical with the switch on and off:
//   fact z-bad 0
//   fact z-class [Z
//   fact b-bad 0
//   fact b-class [B
//   fact c-bad 0
//   fact c-class [C
//   fact s-bad 0
//   fact s-class [S
//   fact i-bad 0
//   fact i-class [I
//   fact f-bad 0
//   fact f-class [F
//   fact j-bad 0
//   fact j-class [J
//   fact d-bad 0
//   fact d-class [D
//   fact ii-bad 0
//   fact ii-shallow true
//   fact ii-class [[I
//   fact str-bad 0
//   fact str-after-gc-bad 0
//   fact str-class [Ljava.lang.String;
//   fact objarr-warm-class [Ljava.lang.Object;
//   fact objarr-midrun-class [Ljava.lang.String;
//   fact objarr-boxed-class [Ljava.lang.Integer;
//   fact objarr-bad 0
//   fact big-bad 0
//   fact null-npe 1000
//   fact obj-bad 0
//   fact nocl-thrown 50000
//   fact nocl-message J27CloneIrRows$NoClone
//   fact override-warm-tagged 0
//   fact override-tagged 50000
//   fact override-class J27CloneIrRows$Sub
//   fact override-after-tagged 0
// -Dj27.timing=true adds `time <row>-ns-per-op <n>` lines (not compared).
//
// Host check (not part of the facts): with CRATONVM_JIT_IR_CLONE_INLINE=1 and
// CRATONVM_DBG_JITC=1 each array row's compile prints `clone-inline (ir) ...
// class=[I ref=false` (etc.; `ref=true` for ii / str / objarr on Generational,
// a `string-prefix declined ... a reference copy on ZGC or narrow slots` /
// `no published reference-store plan` note on ZGC / G1); no instance row
// prints one. Run once per collector (Generational, G1, ZGC), and once with
// the switch unset: the facts are the same every time.

import java.util.Arrays;

public class J27CloneIrRows {
    static final int N = 200_000;
    static final int M = 50_000;
    static final int LENGTHS = 18;

    static boolean timing = Boolean.getBoolean("j27.timing");
    static long sink;

    public static void main(String[] args) throws Exception {
        primitiveRows();
        intMatrixRow();
        stringChurnRow();
        objectArrayRow();
        bigRow();
        nullRow();
        objectRow();
        noCloneRow();
        overrideRow();
    }

    static void time(String row, long t0, long ops) {
        if (timing) {
            System.out.println("time " + row + "-ns-per-op " + (System.nanoTime() - t0) / ops);
        }
    }

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    // ---- one clone site per primitive element type ------------------------

    static boolean[] cz(boolean[] a) { return a.clone(); }
    static byte[] cb(byte[] a) { return a.clone(); }
    static char[] cc(char[] a) { return a.clone(); }
    static short[] cs(short[] a) { return a.clone(); }
    static int[] ci(int[] a) { return a.clone(); }
    static float[] cf(float[] a) { return a.clone(); }
    static long[] cj(long[] a) { return a.clone(); }
    static double[] cd(double[] a) { return a.clone(); }

    /** Element j of the source of length n (never 0, so a zeroed copy is caught). */
    static int v(int n, int j) {
        return j * 31 + n * 7 + 1;
    }

    static void primitiveRows() {
        // Lengths 0..17: every tail of an 8-byte word for every element size.
        boolean[][] z = new boolean[LENGTHS][];
        byte[][] b = new byte[LENGTHS][];
        char[][] c = new char[LENGTHS][];
        short[][] s = new short[LENGTHS][];
        int[][] i4 = new int[LENGTHS][];
        float[][] f = new float[LENGTHS][];
        long[][] j8 = new long[LENGTHS][];
        double[][] d = new double[LENGTHS][];
        for (int n = 0; n < LENGTHS; n++) {
            z[n] = new boolean[n];
            b[n] = new byte[n];
            c[n] = new char[n];
            s[n] = new short[n];
            i4[n] = new int[n];
            f[n] = new float[n];
            j8[n] = new long[n];
            d[n] = new double[n];
            for (int k = 0; k < n; k++) {
                z[n][k] = (v(n, k) & 2) != 0;
                b[n][k] = (byte) v(n, k);
                c[n][k] = (char) (v(n, k) * 977);
                s[n][k] = (short) (v(n, k) * 1031);
                i4[n][k] = v(n, k) * 0x01010101;
                f[n][k] = v(n, k) * 0.5f;
                j8[n][k] = (long) v(n, k) << 33 | v(n, k);
                d[n][k] = v(n, k) * 0.25;
            }
        }
        int bz = 0, bb = 0, bc = 0, bs = 0, bi = 0, bf = 0, bj = 0, bd = 0;
        long t0 = System.nanoTime();
        for (int it = 0; it < N; it++) {
            int n = it % LENGTHS;
            boolean[] cz = cz(z[n]);
            bz += cz == z[n] || !Arrays.equals(cz, z[n]) ? 1 : 0;
            byte[] cb = cb(b[n]);
            bb += cb == b[n] || !Arrays.equals(cb, b[n]) ? 1 : 0;
            char[] cc = cc(c[n]);
            bc += cc == c[n] || !Arrays.equals(cc, c[n]) ? 1 : 0;
            short[] cs = cs(s[n]);
            bs += cs == s[n] || !Arrays.equals(cs, s[n]) ? 1 : 0;
            int[] ci = ci(i4[n]);
            bi += ci == i4[n] || !Arrays.equals(ci, i4[n]) ? 1 : 0;
            float[] cf = cf(f[n]);
            bf += cf == f[n] || !Arrays.equals(cf, f[n]) ? 1 : 0;
            long[] cj = cj(j8[n]);
            bj += cj == j8[n] || !Arrays.equals(cj, j8[n]) ? 1 : 0;
            double[] cd = cd(d[n]);
            bd += cd == d[n] || !Arrays.equals(cd, d[n]) ? 1 : 0;
            if (n > 0) {
                // The copy is independent of its source.
                ci[n - 1] ^= 1;
                bi += ci[n - 1] == i4[n][n - 1] ? 1 : 0;
                cb[0] ^= 1;
                bb += cb[0] == b[n][0] ? 1 : 0;
            }
        }
        time("prim8", t0, N);
        fact("z-bad", bz);
        fact("z-class", cz(z[1]).getClass().getName());
        fact("b-bad", bb);
        fact("b-class", cb(b[1]).getClass().getName());
        fact("c-bad", bc);
        fact("c-class", cc(c[1]).getClass().getName());
        fact("s-bad", bs);
        fact("s-class", cs(s[1]).getClass().getName());
        fact("i-bad", bi);
        fact("i-class", ci(i4[1]).getClass().getName());
        fact("f-bad", bf);
        fact("f-class", cf(f[1]).getClass().getName());
        fact("j-bad", bj);
        fact("j-class", cj(j8[1]).getClass().getName());
        fact("d-bad", bd);
        fact("d-class", cd(d[1]).getClass().getName());
    }

    // ---- int[][]: a reference array of int[] (shallow) --------------------

    static int[][] cii(int[][] a) { return a.clone(); }

    static void intMatrixRow() {
        int[][][] src = new int[LENGTHS][][];
        for (int n = 0; n < LENGTHS; n++) {
            src[n] = new int[n][];
            for (int k = 0; k < n; k++) {
                src[n][k] = new int[] {v(n, k), k};
            }
        }
        int bad = 0;
        boolean shallow = true;
        long t0 = System.nanoTime();
        for (int it = 0; it < N; it++) {
            int n = it % LENGTHS;
            int[][] c = cii(src[n]);
            bad += c == src[n] || c.length != n ? 1 : 0;
            for (int k = 0; k < n; k++) {
                shallow &= c[k] == src[n][k];
            }
            if (n > 0) {
                c[0] = null;
                bad += src[n][0] == null ? 1 : 0;
            }
        }
        time("ii", t0, N);
        fact("ii-bad", bad);
        fact("ii-shallow", shallow);
        fact("ii-class", cii(src[2]).getClass().getName());
    }

    // ---- String[] under GC churn ------------------------------------------

    static String[] cstr(String[] a) { return a.clone(); }

    static void stringChurnRow() {
        String[][] src = new String[LENGTHS][];
        for (int n = 0; n < LENGTHS; n++) {
            src[n] = fresh(n, 0);
        }
        String[][] ring = new String[64][];
        String[][] ringSrc = new String[64][];
        int bad = 0;
        long t0 = System.nanoTime();
        for (int it = 0; it < N; it++) {
            int n = it % LENGTHS;
            if (it % 1000 == 999) {
                // Young referents in a fresh source (the old one stays in the ring).
                src[n] = fresh(n, it);
            }
            String[] c = cstr(src[n]);
            bad += c == src[n] || c.length != n ? 1 : 0;
            for (int k = 0; k < n; k++) {
                bad += c[k] != src[n][k] ? 1 : 0;
            }
            ring[it & 63] = c;
            ringSrc[it & 63] = src[n];
            // Churn: short-lived garbage so collections run during the loop.
            long[] junk = new long[48];
            junk[it & 47] = it;
            sink += junk[(it + 1) & 47];
        }
        time("str", t0, N);
        System.gc();
        int after = 0;
        for (int r = 0; r < 64; r++) {
            String[] c = ring[r];
            String[] a = ringSrc[r];
            after += c == null || c == a || !Arrays.equals(c, a) ? 1 : 0;
            for (int k = 0; c != null && k < c.length; k++) {
                after += c[k] != a[k] ? 1 : 0;
            }
        }
        fact("str-bad", bad);
        fact("str-after-gc-bad", after);
        fact("str-class", cstr(src[3]).getClass().getName());
    }

    static String[] fresh(int n, int salt) {
        String[] a = new String[n];
        for (int k = 0; k < n; k++) {
            // `null` every fifth slot: a null element is copied too.
            a[k] = k % 5 == 4 ? null : new String("s" + n + "_" + k + "_" + salt);
        }
        return a;
    }

    // ---- an Object[]-typed site whose receivers change class mid-run ------

    static Object[] cobj(Object[] a) { return a.clone(); }

    static int objLoop(Object[] a, int n) {
        int bad = 0;
        for (int it = 0; it < n; it++) {
            Object[] c = cobj(a);
            bad += c == a || c.getClass() != a.getClass() || !Arrays.equals(c, a) ? 1 : 0;
        }
        return bad;
    }

    static void objectArrayRow() {
        Object[] plain = {"a", 1, null};
        String[] strings = {"x", "y", "z", "w"};
        Integer[] boxes = {7, 8};
        long t0 = System.nanoTime();
        int bad = objLoop(plain, N);
        time("objarr", t0, N);
        bad += objLoop(strings, M);
        bad += objLoop(boxes, M);
        fact("objarr-warm-class", cobj(plain).getClass().getName());
        fact("objarr-midrun-class", cobj(strings).getClass().getName());
        fact("objarr-boxed-class", cobj(boxes).getClass().getName());
        fact("objarr-bad", bad);
    }

    // ---- past the inline cap (the helper's large-array arm) ---------------

    static void bigRow() {
        long[] big = new long[5000];
        for (int k = 0; k < big.length; k++) {
            big[k] = (long) k * k;
        }
        int bad = 0;
        for (int it = 0; it < 2000; it++) {
            long[] c = cj(big);
            bad += c == big || !Arrays.equals(c, big) ? 1 : 0;
        }
        fact("big-bad", bad);
    }

    // ---- a null receiver: the NPE is the call's -----------------------------

    static void nullRow() {
        int[] none = null;
        int npe = 0;
        for (int it = 0; it < 1000; it++) {
            try {
                sink += ci(it == -1 ? new int[1] : none).length;
            } catch (NullPointerException e) {
                npe++;
            }
        }
        fact("null-npe", npe);
    }

    // ---- an instance with reference fields (never expanded inline) --------

    static class Node implements Cloneable {
        String name;
        int[] data;
        Node next;
        long v;

        Node copy() throws CloneNotSupportedException {
            return (Node) clone();
        }
    }

    static void objectRow() throws CloneNotSupportedException {
        Node tail = new Node();
        tail.name = "tail";
        Node n = new Node();
        n.name = "head";
        n.data = new int[] {1, 2, 3};
        n.next = tail;
        int bad = 0;
        for (int it = 0; it < N; it++) {
            n.v = it;
            Node c = n.copy();
            bad += c == n || c.getClass() != Node.class || c.name != n.name || c.data != n.data
                    || c.next != tail || c.v != it ? 1 : 0;
        }
        fact("obj-bad", bad);
    }

    // ---- a receiver that is not Cloneable ---------------------------------

    static class NoClone {
        Object attempt() throws CloneNotSupportedException {
            return clone();
        }
    }

    static void noCloneRow() {
        NoClone x = new NoClone();
        int thrown = 0;
        String message = "";
        for (int it = 0; it < M; it++) {
            try {
                sink += x.attempt().hashCode();
            } catch (CloneNotSupportedException e) {
                thrown++;
                message = e.getMessage();
            }
        }
        fact("nocl-thrown", thrown);
        fact("nocl-message", message);
    }

    // ---- an overriding subclass first seen mid-run ------------------------

    static class Base implements Cloneable {
        int tag;

        Base dup() throws CloneNotSupportedException {
            return (Base) clone();
        }
    }

    static class Sub extends Base {
        @Override
        protected Object clone() throws CloneNotSupportedException {
            Sub c = (Sub) super.clone();
            c.tag = 7;
            return c;
        }
    }

    static int dupLoop(Base b, int n) throws CloneNotSupportedException {
        int tagged = 0;
        for (int it = 0; it < n; it++) {
            Base c = b.dup();
            tagged += c != b && c.getClass() == b.getClass() && c.tag == 7 ? 1 : 0;
        }
        return tagged;
    }

    static void overrideRow() throws CloneNotSupportedException {
        int warm = dupLoop(new Base(), N);
        Sub sub = new Sub();
        int tagged = dupLoop(sub, M);
        fact("override-warm-tagged", warm);
        fact("override-tagged", tagged);
        fact("override-class", sub.dup().getClass().getName());
        fact("override-after-tagged", dupLoop(new Base(), M));
    }
}
