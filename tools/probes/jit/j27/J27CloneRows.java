// j27 wave 3 (lane CLONE): compiled VIRTUAL clone() -- arrays of every kind and
// instances that inherit Object.clone -- through the thin helper
// (CRATONVM_JIT_CLONE_VIRTUAL_THIN, default ON), with every non-plain case on
// the generic path. Each row runs in a loop long enough to compile its method
// in both tiers; every fact is checked on every iteration and only the summary
// is printed.
// Page: docs/known-issues/jit/javac-zipfs-typecheck-sites-and-instance-clone-leave-compiled-code-20261027.md
//
// Expected HotSpot 25 output (facts only):
//   fact plain-copy true
//   fact plain-sum 25493856
//   fact wide-copy true
//   fact wide-class J27CloneRows$Wide
//   fact override-mixed 3780000
//   fact override-tag-seen 20000
//   fact cnse-message J27CloneRows$NotCloneable
//   fact cnse-count 200000
//   fact array-int true
//   fact array-long true
//   fact array-byte true
//   fact array-short true
//   fact array-char true
//   fact array-boolean true
//   fact array-float true
//   fact array-double true
//   fact array-string true
//   fact array-int2d true
//   fact array-zero true
//   fact original-unchanged true
//   fact churn-ok true
//   fact churn-copies 400000
// -Dj27.timing=true adds `time <row>-ns-per-op <n>` lines (not compared).

public class J27CloneRows {
    static final int N = 200_000;

    /** A plain Cloneable class that does not override clone(). */
    static class Plain implements Cloneable {
        int a;
        long b;
        String s;
        int[] arr;

        Plain(int a) {
            this.a = a;
            this.b = 1L << 40 | a;
            this.s = "p" + (a & 7);
            this.arr = new int[] {a, a + 1};
        }

        /** invokevirtual Plain.clone()Ljava/lang/Object; -> Object.clone. */
        Plain copy() {
            try {
                return (Plain) clone();
            } catch (CloneNotSupportedException e) {
                throw new AssertionError(e);
            }
        }
    }

    /** A subclass with extra fields, inheriting Object.clone too. */
    static class Wide extends Plain {
        double d;
        Object o;
        short sh;

        Wide(int a) {
            super(a);
            this.d = a * 0.5;
            this.o = Integer.valueOf(a);
            this.sh = (short) a;
        }
    }

    /** The clone() site is Base's; Sub overrides it, Base inherits Object's. */
    static class Base implements Cloneable {
        int tag;

        Object dup() throws CloneNotSupportedException {
            return clone();
        }
    }

    static class Sub extends Base {
        @Override
        protected Object clone() throws CloneNotSupportedException {
            Sub s = (Sub) super.clone();
            s.tag = 99;
            return s;
        }
    }

    /** Not Cloneable: Object.clone throws CloneNotSupportedException. */
    static class NotCloneable {
        Object tryClone() throws CloneNotSupportedException {
            return clone();
        }
    }

    static boolean timing = Boolean.getBoolean("j27.timing");

    public static void main(String[] args) throws Exception {
        plainRows();
        overrideRow();
        cnseRow();
        arrayRows();
        churnRow();
    }

    static void time(String row, long t0, long ops) {
        if (timing) {
            System.out.println("time " + row + "-ns-per-op " + (System.nanoTime() - t0) / ops);
        }
    }

    static void plainRows() {
        boolean plainOk = true;
        boolean wideOk = true;
        long sum = 0;
        String wideClass = "";
        long t0 = System.nanoTime();
        // Plain receivers first, so `copy()`'s clone site compiles with Plain
        // dominant (bound); the Wide loop then reaches the bound site with a
        // subclass receiver the helper must screen itself.
        for (int i = 0; i < N; i++) {
            Plain p = new Plain(i);
            Plain c = p.copy();
            plainOk &= c != p && c.getClass() == Plain.class && c.a == p.a && c.b == p.b
                    && c.s == p.s && c.arr == p.arr;
            sum += c.a & 0xff;
        }
        for (int i = 0; i < N; i++) {
            Wide w = new Wide(i);
            Plain wc = w.copy();
            Wide w2 = (Wide) wc;
            wideOk &= w2 != w && w2.a == w.a && w2.b == w.b && w2.s == w.s && w2.arr == w.arr
                    && w2.d == w.d && w2.o == w.o && w2.sh == w.sh;
            wideClass = wc.getClass().getName();
        }
        time("plain", t0, N);
        System.out.println("fact plain-copy " + plainOk);
        // sum over i of (i & 0xff): 781 full cycles of 0..255 (32640 each) plus 0..63.
        System.out.println("fact plain-sum " + sum);
        System.out.println("fact wide-copy " + wideOk);
        System.out.println("fact wide-class " + wideClass);
    }

    static void overrideRow() throws CloneNotSupportedException {
        long mixed = 0;
        int tagSeen = 0;
        Base base = new Base();
        Sub sub = new Sub();
        base.tag = 10;
        sub.tag = 1;
        long t0 = System.nanoTime();
        for (int i = 0; i < N; i++) {
            Base r = (i % 10 == 9) ? sub : base;
            Base c = (Base) r.dup();
            if (c == r || c.getClass() != r.getClass()) {
                throw new AssertionError("clone identity at " + i);
            }
            if (c.tag == 99) {
                tagSeen++;
            }
            mixed += c.tag;
        }
        time("override", t0, N);
        // 180000 Base copies of tag 10 + 20000 Sub copies retagged 99 = 3780000.
        System.out.println("fact override-mixed " + mixed);
        System.out.println("fact override-tag-seen " + tagSeen);
    }

    static void cnseRow() {
        NotCloneable nc = new NotCloneable();
        String message = "";
        int count = 0;
        for (int i = 0; i < N; i++) {
            try {
                nc.tryClone();
            } catch (CloneNotSupportedException e) {
                message = e.getMessage();
                count++;
            }
        }
        System.out.println("fact cnse-message " + message);
        System.out.println("fact cnse-count " + count);
    }

    static void arrayRows() {
        boolean ints = true, longs = true, bytes = true, shorts = true, chars = true;
        boolean bools = true, floats = true, doubles = true, strings = true, ints2d = true;
        boolean zero = true, unchanged = true;
        String[] names = {"a", "b", "c", "d", "e"};
        long t0 = System.nanoTime();
        for (int i = 0; i < N; i++) {
            int n = 1 + (i & 15);
            int[] ia = new int[n];
            long[] la = new long[n];
            byte[] ba = new byte[n];
            short[] sa = new short[n];
            char[] ca = new char[n];
            boolean[] za = new boolean[n];
            float[] fa = new float[n];
            double[] da = new double[n];
            String[] str = new String[n];
            int[][] i2 = new int[n][];
            for (int k = 0; k < n; k++) {
                ia[k] = i * 31 + k;
                la[k] = ((long) i << 33) + k;
                ba[k] = (byte) (i + k);
                sa[k] = (short) (i - k);
                ca[k] = (char) ('A' + (i + k) % 26);
                za[k] = ((i + k) & 1) == 0;
                fa[k] = i + k * 0.25f;
                da[k] = i * 1.5 + k;
                str[k] = names[(i + k) % names.length];
                i2[k] = ia;
            }
            int[] ic = ia.clone();
            ints &= ic != ia && java.util.Arrays.equals(ic, ia);
            ic[0] ^= 1;
            unchanged &= ia[0] == i * 31;
            longs &= java.util.Arrays.equals(la.clone(), la);
            bytes &= java.util.Arrays.equals(ba.clone(), ba);
            shorts &= java.util.Arrays.equals(sa.clone(), sa);
            chars &= java.util.Arrays.equals(ca.clone(), ca);
            bools &= java.util.Arrays.equals(za.clone(), za);
            floats &= java.util.Arrays.equals(fa.clone(), fa);
            doubles &= java.util.Arrays.equals(da.clone(), da);
            String[] sc = str.clone();
            strings &= sc != str && sc.getClass() == String[].class && java.util.Arrays.equals(sc, str);
            sc[0] = "z";
            unchanged &= str[0] == names[i % names.length];
            int[][] i2c = i2.clone();
            ints2d &= i2c != i2 && i2c.getClass() == int[][].class && i2c[n - 1] == ia;
            zero &= new int[0].clone().length == 0 && new Object[0].clone().length == 0
                    && new double[0].clone().getClass() == double[].class;
        }
        time("array", t0, N);
        System.out.println("fact array-int " + ints);
        System.out.println("fact array-long " + longs);
        System.out.println("fact array-byte " + bytes);
        System.out.println("fact array-short " + shorts);
        System.out.println("fact array-char " + chars);
        System.out.println("fact array-boolean " + bools);
        System.out.println("fact array-float " + floats);
        System.out.println("fact array-double " + doubles);
        System.out.println("fact array-string " + strings);
        System.out.println("fact array-int2d " + ints2d);
        System.out.println("fact array-zero " + zero);
        System.out.println("fact original-unchanged " + unchanged);
    }

    /** Clone under allocation churn and explicit collections: every copy is complete. */
    static void churnRow() {
        boolean ok = true;
        int copies = 0;
        Object[] keep = new Object[64];
        for (int i = 0; i < N; i++) {
            Object[] src = new Object[1 + (i & 63)];
            for (int k = 0; k < src.length; k++) {
                src[k] = new int[] {i, k};
            }
            int[] prim = new int[1 + (i & 127)];
            for (int k = 0; k < prim.length; k++) {
                prim[k] = i ^ k;
            }
            Object[] garbage = new Object[256];
            for (int k = 0; k < garbage.length; k++) {
                garbage[k] = new long[8];
            }
            Object[] oc = src.clone();
            int[] pc = prim.clone();
            copies += 2;
            keep[i & 63] = oc;
            if ((i & 16383) == 0) {
                System.gc();
            }
            for (int k = 0; k < src.length; k++) {
                int[] e = (int[]) oc[k];
                ok &= e == src[k] && e[0] == i && e[1] == k;
            }
            for (int k = 0; k < prim.length; k++) {
                ok &= pc[k] == (i ^ k);
            }
        }
        // The kept copies survived collections intact.
        for (Object o : keep) {
            Object[] oc = (Object[]) o;
            for (int k = 0; k < oc.length; k++) {
                ok &= ((int[]) oc[k])[1] == k;
            }
        }
        System.out.println("fact churn-ok " + ok);
        System.out.println("fact churn-copies " + copies);
    }
}
